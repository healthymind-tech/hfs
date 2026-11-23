#!/usr/bin/env bash
# hfs-unify-db: move the terminology database (`hts`) into schema `terminology`
# of the HFS database (docs/design/unified-server.md §5.5, runbook
# docs/ops/unified-server.md). Idempotent and resumable: every step records
# itself in public.hfs_terminology_state.note of the target database and a
# re-run skips what is done; after success a re-run is a no-op.
#
# Usage: unify-db.sh [options] [run|status|verify|export-delta|cleanup]
#   run (default)   preflight → freeze → snapshot → rename → copy → verify →
#                   record → switch → unfreeze
#   status          print the recorded state and the object/row inventory
#   verify          re-run the verification (counts, checksums, fingerprint)
#   export-delta    CodeSystem/ValueSet/ConceptMap written in the new schema
#                   after moved_at, as a FHIR transaction Bundle on stdout
#                   (re-import into the old database if a rollback must stick)
#   cleanup         DROP DATABASE <stage> (after the observation window);
#                   with --drop-source also DROP DATABASE <source>
#
# Options (environment variable in brackets):
#   --container NAME     PostgreSQL container, tools run inside it [PG_CONTAINER, hfs-eval-hfs-pg]
#   --superuser NAME     superuser for psql/pg_dump [PGU, postgres]
#   --source DB          terminology database [SRC_DB, hts]
#   --target DB          HFS database [DST_DB, hfs]
#   --role ROLE          owner of the copied objects [DST_ROLE, hfs]
#   --schema NAME        target schema [TX_SCHEMA, terminology]
#   --stage DB           staging copy [STAGE_DB, <source>_stage]
#   --snapshot MODE      template (default): CREATE DATABASE … TEMPLATE <source>;
#                        terminology reads pause for the file copy (--pause-cmd).
#                        dump: pg_dump | pg_restore into the stage; no read
#                        pause, about twice the copy time.
#   --in-place           no stage: rename inside <source> itself (its readers
#                        must stay stopped for the whole copy; disk ≈ 1.2×)
#   --jobs N             parallel index/constraint build (post-data) [JOBS, 8]
#   --checksum MODE      full (default: order-independent md5 sum of every row)
#                        | count (row counts only)
#   --replace            drop what is in <schema> and copy again (resume after a
#                        killed copy, or redo after a failed verify)
#   --dry-run            print the plan and the preflight, change nothing
#   --yes                do not ask before the steps that pause readers
#   --freeze-cmd CMD     run before the copy: terminology writes off
#                        (Stage 1: HFS_TERMINOLOGY_READONLY=true + restart hfs;
#                        Stage 0: stop writes at the edge or accept a frozen window)
#   --unfreeze-cmd CMD   run at the end (or on failure after freeze)
#   --pause-cmd CMD      extra step before the template copy (the script itself
#                        refuses new connections to <source> and ends open ones)
#   --resume-cmd CMD     extra step after it (connections are allowed again first)
#   --smoke-cmd CMD      extra verification, must exit 0 (e.g. $lookup/$expand
#                        through a terminology pool on the new schema)
#   --switch-cmd CMD     point the server at the schema (drop
#                        HFS_TERMINOLOGY_DATABASE_URL + restart)
#   --min-free-factor F  free space needed on the data volume, × size(source)
#                        [2.2 template/dump, 1.2 in-place]
#   --wal-dir PATH       host directory of the WAL archive; preflight wants
#                        ≥ size(source) free there [WAL_DIR]
#   --timings FILE       append "step seconds" lines [TIMINGS]
#   --drop-source        cleanup: also drop <source>
#   --data-path PATH     PostgreSQL data directory inside the container
#                        [/var/lib/postgresql/data]
set -euo pipefail

C=${PG_CONTAINER:-hfs-eval-hfs-pg}; PGU=${PGU:-postgres}
SRC=${SRC_DB:-hts}; DST=${DST_DB:-hfs}; ROLE=${DST_ROLE:-hfs}; SCHEMA=${TX_SCHEMA:-terminology}
STAGE=${STAGE_DB:-}; SNAP=template; INPLACE=0; JOBS=${JOBS:-8}; CHECKSUM=full
REPLACE=0; DRY=0; YES=0; FREEZE=""; UNFREEZE=""; PAUSE=""; RESUME=""; SMOKE=""; SWITCH=""
FACTOR=""; WAL_DIR=${WAL_DIR:-}; TIMINGS=${TIMINGS:-}; DROP_SOURCE=0; DATA=/var/lib/postgresql/data
CMD=run
while [ $# -gt 0 ]; do
  case $1 in
    --container) C=$2; shift 2;; --superuser) PGU=$2; shift 2;;
    --source) SRC=$2; shift 2;; --target) DST=$2; shift 2;; --role) ROLE=$2; shift 2;;
    --schema) SCHEMA=$2; shift 2;; --stage) STAGE=$2; shift 2;;
    --snapshot) SNAP=$2; shift 2;; --in-place) INPLACE=1; shift;;
    --jobs) JOBS=$2; shift 2;; --checksum) CHECKSUM=$2; shift 2;;
    --replace) REPLACE=1; shift;; --dry-run) DRY=1; shift;; --yes) YES=1; shift;;
    --freeze-cmd) FREEZE=$2; shift 2;; --unfreeze-cmd) UNFREEZE=$2; shift 2;;
    --pause-cmd) PAUSE=$2; shift 2;; --resume-cmd) RESUME=$2; shift 2;;
    --smoke-cmd) SMOKE=$2; shift 2;; --switch-cmd) SWITCH=$2; shift 2;;
    --min-free-factor) FACTOR=$2; shift 2;; --wal-dir) WAL_DIR=$2; shift 2;;
    --timings) TIMINGS=$2; shift 2;; --drop-source) DROP_SOURCE=1; shift;;
    --data-path) DATA=$2; shift 2;;
    -h|--help) sed -n '2,60p' "$0"; exit 0;;
    run|status|verify|export-delta|cleanup) CMD=$1; shift;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done
[ -n "$STAGE" ] || STAGE=${SRC}_stage
[ "$INPLACE" = 1 ] && STAGE=$SRC
[ -n "$FACTOR" ] || { [ "$INPLACE" = 1 ] && FACTOR=1.2 || FACTOR=2.2; }
for n in "$SRC" "$DST" "$ROLE" "$SCHEMA" "$STAGE"; do
  [[ $n =~ ^[a-z_][a-z0-9_]{0,62}$ ]] || { echo "invalid identifier: $n" >&2; exit 2; }
done
case $SCHEMA in public|pg_*|information_schema) echo "schema $SCHEMA is reserved" >&2; exit 2;; esac
case $SNAP in template|dump) ;; *) echo "--snapshot template|dump" >&2; exit 2;; esac
case $CHECKSUM in full|count) ;; *) echo "--checksum full|count" >&2; exit 2;; esac

log() { printf '%s [unify-db] %s\n' "$(date +%FT%T)" "$*" >&2; }
die() { log "ERROR: $*"; exit 1; }
psqlc() { local db=$1; shift; docker exec -i -e PGOPTIONS="-c client_min_messages=warning" "$C" psql -X -q -v ON_ERROR_STOP=1 -U "$PGU" -d "$db" "$@"; }
q() { psqlc "$1" -Atc "$2"; }   # one value / rows, unaligned
exists_db() { [ "$(q postgres "SELECT count(*) FROM pg_database WHERE datname = '$1'")" = 1 ]; }
T0=$(date +%s); STEP_T=$T0
timing() {  # timing <step>
  local now; now=$(date +%s)
  log "step $1 took $((now - STEP_T)) s"
  [ -z "$TIMINGS" ] || echo "$1 $((now - STEP_T))" >> "$TIMINGS"
  STEP_T=$now
}
hook() {  # hook <name> <cmd>
  [ -n "$2" ] || return 0
  log "hook $1: $2"
  [ "$DRY" = 1 ] && return 0
  bash -c "$2" || die "hook $1 failed"
}

# ── state (public.hfs_terminology_state, hm-0011) ─────────────────────────────
STEPS=(preflight frozen snapshot renamed copied verified recorded switched done)
step_index() { local i; for i in "${!STEPS[@]}"; do [ "${STEPS[$i]}" = "$1" ] && { echo "$i"; return; }; done; echo -1; }
ensure_state_table() {
  # hm-0011's SQL, idempotent; the server records the migration itself when it
  # first runs embedded (CREATE … IF NOT EXISTS makes the order irrelevant).
  psqlc "$DST" <<SQL
SET ROLE $ROLE;
CREATE SCHEMA IF NOT EXISTS $SCHEMA;
CREATE TABLE IF NOT EXISTS hfs_terminology_state (
    id          BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    store       TEXT NOT NULL,
    moved_from  TEXT,
    moved_at    TIMESTAMPTZ,
    source_fingerprint TEXT,
    note        TEXT,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE hfs_terminology_state ADD COLUMN IF NOT EXISTS note TEXT;
SQL
}
state_note() {
  [ "$(q "$DST" "SELECT to_regclass('public.hfs_terminology_state') IS NOT NULL")" = t ] || return 0
  q "$DST" "SELECT coalesce((SELECT note FROM public.hfs_terminology_state WHERE id), '')"
}
set_note() {  # set_note <step>
  [ "$DRY" = 1 ] && return 0
  psqlc "$DST" -c "INSERT INTO public.hfs_terminology_state (id, store, moved_from, note)
     VALUES (TRUE, 'external-db', 'database:$SRC', '$1')
     ON CONFLICT (id) DO UPDATE SET note = EXCLUDED.note, updated_at = now()" >/dev/null
  log "state: $1"
}
done_step() {  # done_step <step> → 0 if recorded state is at or past it
  local cur; cur=$(state_note); [ -n "$cur" ] || return 1
  [ "$(step_index "$cur")" -ge "$(step_index "$1")" ]
}

# ── inventory / verification ──────────────────────────────────────────────────
# Tables of a schema with exact row counts and (full mode) an order-independent
# checksum: count + sum of the first 60 bits of md5(row::text).
inventory() {  # inventory <db> <schema>
  local db=$1 s=$2 sql
  if [ "$CHECKSUM" = full ]; then
    sql="SELECT format('SELECT %L, count(*), coalesce(sum((''x'' || substr(md5(t::text), 1, 15))::bit(60)::bigint), 0) FROM %I.%I t;', c.relname, n.nspname, c.relname)"
  else
    sql="SELECT format('SELECT %L, count(*), 0 FROM %I.%I t;', c.relname, n.nspname, c.relname)"
  fi
  q "$db" "$sql FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = '$s' AND c.relkind IN ('r','p') ORDER BY c.relname" \
    | psqlc "$db" -At -F ' ' | sort
}
objects() {  # objects <db> <schema>: indexes, constraints, triggers, sequences(last value), functions
  q "$1" "SELECT 'index ' || c.relname || ' ' || i.indisvalid FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid
            WHERE c.relnamespace = '$2'::regnamespace
          UNION ALL SELECT 'constraint ' || conrelid::regclass::text || ' ' || conname || ' ' || convalidated
            FROM pg_constraint WHERE connamespace = '$2'::regnamespace
          UNION ALL SELECT 'trigger ' || c.relname || ' ' || t.tgname || ' ' || t.tgenabled::text FROM pg_trigger t
            JOIN pg_class c ON c.oid = t.tgrelid WHERE c.relnamespace = '$2'::regnamespace AND NOT t.tgisinternal
          UNION ALL SELECT 'sequence ' || sequencename || ' ' || coalesce(last_value, 0) FROM pg_sequences WHERE schemaname = '$2'
          UNION ALL SELECT 'function ' || p.proname || ' ' || md5(p.prosrc) FROM pg_proc p WHERE p.pronamespace = '$2'::regnamespace
          ORDER BY 1" | sed "s/ $2\./ /g"
}
fingerprint() {  # fingerprint <db> <schema>
  q "$1" "SELECT CASE WHEN to_regclass('$2.hts_schema_meta') IS NULL THEN 'none'
          ELSE coalesce((SELECT value FROM $2.hts_schema_meta WHERE key = 'schema_fingerprint'), 'none') END"
}
schema_tables() { q "$1" "SELECT count(*) FROM pg_class WHERE relnamespace = to_regnamespace('$2') AND relkind IN ('r','p')"; }

verify() {
  local src_schema=$SCHEMA tmp; tmp=$(mktemp -d)
  log "verify: inventory of $STAGE.$src_schema vs $DST.$SCHEMA (checksum=$CHECKSUM)"
  inventory "$STAGE" "$src_schema" > "$tmp/src.inv" &
  inventory "$DST" "$SCHEMA" > "$tmp/dst.inv" &
  wait %1 || die "verify: table inventory of $STAGE failed"
  wait %2 || die "verify: table inventory of $DST failed"
  objects "$STAGE" "$src_schema" > "$tmp/src.obj" || die "verify: object inventory of $STAGE failed"
  objects "$DST" "$SCHEMA" > "$tmp/dst.obj" || die "verify: object inventory of $DST failed"
  local ok=1 n rows
  [ -s "$tmp/src.obj" ] || { log "verify: empty object inventory"; ok=0; }
  n=$(wc -l < "$tmp/src.inv"); rows=$(awk '{s+=$2} END {print s+0}' "$tmp/src.inv")
  [ "$n" -gt 0 ] || { log "verify: no tables in $STAGE.$src_schema"; ok=0; }
  diff -u "$tmp/src.inv" "$tmp/dst.inv" >&2 || { log "verify: table rows/checksums differ"; ok=0; }
  diff -u "$tmp/src.obj" "$tmp/dst.obj" >&2 || { log "verify: objects differ"; ok=0; }
  local fs fd; fs=$(fingerprint "$STAGE" "$src_schema"); fd=$(fingerprint "$DST" "$SCHEMA")
  [ "$fs" = "$fd" ] || { log "verify: hts_schema_meta fingerprint $fs != $fd"; ok=0; }
  local leaked; leaked=$(q "$DST" "SELECT count(*) FROM pg_class c WHERE c.relnamespace = 'public'::regnamespace
       AND c.relname IN ('code_systems','concepts','value_sets','concept_maps','hts_schema_meta','terminology_epoch')")
  [ "$leaked" = 0 ] || { log "verify: HTS tables found in $DST.public"; ok=0; }
  FPRINT="tables=$n rows=$rows checksum=$(md5sum < "$tmp/src.inv" | cut -c1-16) hts_schema=$fs"
  log "verify: $FPRINT objects=$(wc -l < "$tmp/src.obj")"
  rm -rf "$tmp"
  [ "$ok" = 1 ]
}

# ── commands ──────────────────────────────────────────────────────────────────
if [ "$CMD" = status ]; then
  echo "state: $(state_note)"
  [ -z "$(state_note)" ] || q "$DST" "SELECT * FROM public.hfs_terminology_state"
  for db in "$SRC" "$STAGE" "$DST"; do
    exists_db "$db" && echo "$db: $(q postgres "SELECT pg_size_pretty(pg_database_size('$db'))")" || echo "$db: (absent)"
  done
  echo "$DST.$SCHEMA tables: $(schema_tables "$DST" "$SCHEMA")"
  exit 0
fi
if [ "$CMD" = verify ]; then
  exists_db "$STAGE" || die "verify needs the stage database $STAGE (dropped by cleanup?)"
  verify && echo "verify: OK $FPRINT"; exit $?
fi
if [ "$CMD" = export-delta ]; then
  since=$(q "$DST" "SELECT moved_at FROM public.hfs_terminology_state WHERE id")
  [ -n "$since" ] || die "no moved_at recorded"
  log "export-delta: terminology resources changed after $since"
  q "$DST" "SELECT json_build_object('resourceType', 'Bundle', 'type', 'transaction', 'entry',
              coalesce(json_agg(json_build_object(
                'resource', r.data,
                'request', json_build_object('method', CASE WHEN r.is_deleted THEN 'DELETE' ELSE 'PUT' END,
                                             'url', r.resource_type || '/' || r.id))), '[]'::json))
            FROM $SCHEMA.resources r
           WHERE r.resource_type IN ('CodeSystem', 'ValueSet', 'ConceptMap')
             AND r.last_updated > '$since'::timestamptz"
  exit 0
fi
if [ "$CMD" = cleanup ]; then
  [ "$(state_note)" = done ] || die "cleanup only after a completed move (state: $(state_note))"
  [ "$INPLACE" = 1 ] && die "in-place moves have no stage"
  log "cleanup: DROP DATABASE $STAGE"
  [ "$DRY" = 1 ] || psqlc postgres -c "DROP DATABASE IF EXISTS $STAGE WITH (FORCE)"
  if [ "$DROP_SOURCE" = 1 ]; then
    log "cleanup: DROP DATABASE $SRC (rollback to the old database is no longer possible)"
    [ "$DRY" = 1 ] || psqlc postgres -c "DROP DATABASE IF EXISTS $SRC WITH (FORCE)"
  fi
  exit 0
fi

# ── run ───────────────────────────────────────────────────────────────────────
if done_step switched; then
  [ "$REPLACE" = 0 ] || die "--replace refused: the server already uses $DST.$SCHEMA (state $(state_note)); roll back first (docs/ops/unified-server.md)"
  [ "$(state_note)" = done ] && { log "already done (state: done); nothing to do"; exit 0; }
fi

FROZE=0; REALLOW=0
on_exit() {
  local rc=$?
  if [ "$REALLOW" = 1 ]; then
    log "re-allowing connections to $SRC"
    psqlc postgres -c "ALTER DATABASE $SRC WITH ALLOW_CONNECTIONS true" >/dev/null || log "could not re-allow connections to $SRC!"
  fi
  if [ "$rc" != 0 ] && [ "$FROZE" = 1 ] && [ -n "$UNFREEZE" ]; then
    log "failed (rc=$rc): unfreezing terminology writes; state stays at '$(state_note)', re-run to resume"
    bash -c "$UNFREEZE" || log "unfreeze hook failed too"
  fi
  log "total $(( $(date +%s) - T0 )) s"
}
trap on_exit EXIT

# preflight (always; cheap)
docker exec "$C" pg_isready -U "$PGU" -q || die "PostgreSQL in $C is not ready"
exists_db "$DST" || die "target database $DST does not exist"
SIZE=0
if exists_db "$SRC"; then SIZE=$(q postgres "SELECT pg_database_size('$SRC')"); fi
FREE=$(docker exec "$C" df -Pk "$DATA" | awk 'NR==2 {printf "%d", $4 * 1024}')
[ -n "$FREE" ] || die "could not read free space of $DATA in $C"
NEED=$(awk -v s="$SIZE" -v f="$FACTOR" 'BEGIN{printf "%d", s*f}')
log "preflight: $SRC=$((SIZE/1048576)) MiB, free on data volume $((FREE/1048576)) MiB, need ≥ $((NEED/1048576)) MiB (×$FACTOR, snapshot=$SNAP, in-place=$INPLACE)"
CUR=$(state_note)
PAST_COPY=0; done_step copied 2>/dev/null && PAST_COPY=1
if [ "$PAST_COPY" = 0 ] || [ "$REPLACE" = 1 ]; then
  exists_db "$SRC" || exists_db "$STAGE" || die "neither $SRC nor $STAGE exists"
  [ "$FREE" -ge "$NEED" ] || [ "$DRY" = 1 ] || die "not enough free space ($((FREE/1048576)) < $((NEED/1048576)) MiB); free space or use --in-place"
fi
if [ -n "$WAL_DIR" ]; then
  # The copy writes about size(source) of WAL (rehearsal: 0.96×): it sits in
  # pg_wal until archived and recycled, and the archive keeps a copy.
  WFREE=$(df -B1 --output=avail "$WAL_DIR" | tail -1)
  DEV_WAL=$(df -P "$WAL_DIR" | awk 'NR==2 {print $1}')
  DEV_DATA=$(docker exec "$C" df -P "$DATA" | awk 'NR==2 {print $1}')
  log "preflight: WAL archive $WAL_DIR free $((WFREE/1048576)) MiB (want ≥ $((SIZE/1048576)) MiB)"
  [ "$WFREE" -ge "$SIZE" ] || [ "$DRY" = 1 ] || die "WAL archive disk too small; prune the archive or take a base backup first"
  if [ "$DEV_WAL" = "$DEV_DATA" ] && { [ "$PAST_COPY" = 0 ] || [ "$REPLACE" = 1 ]; }; then
    TOTAL=$((NEED + 2 * SIZE))
    log "preflight: data volume and WAL archive share $DEV_DATA: copy + pg_wal + archive need ≈ $((TOTAL/1048576)) MiB, free $((FREE/1048576)) MiB"
    [ "$FREE" -ge "$TOTAL" ] || [ "$DRY" = 1 ] || [ "${UNIFY_ALLOW_TIGHT_DISK:-0}" = 1 ] \
      || die "not enough space for copy + WAL on one filesystem; free space, pause archiving (docs/ops/unified-server.md §2) or set UNIFY_ALLOW_TIGHT_DISK=1"
  fi
fi
DSTTABLES=$(schema_tables "$DST" "$SCHEMA")
log "preflight: $DST.$SCHEMA has ${DSTTABLES:-0} tables; recorded state '${CUR:-none}'"
if [ "${DSTTABLES:-0}" != 0 ] && [ "$PAST_COPY" = 0 ] && [ "$REPLACE" = 0 ]; then
  die "$DST.$SCHEMA already has tables but no completed copy is recorded (killed copy?): re-run with --replace"
fi
if [ "$DRY" = 1 ]; then
  log "dry run: would run steps after '${CUR:-none}': ${STEPS[*]}"
  log "dry run: snapshot: $([ "$INPLACE" = 1 ] && echo "in place in $SRC" || echo "$SNAP → $STAGE"); copy -n $SCHEMA $STAGE → $DST as $ROLE (post-data -j $JOBS)"
  exit 0
fi
ensure_state_table
[ -n "$CUR" ] || set_note preflight
timing preflight

if [ "$REPLACE" = 1 ]; then
  log "--replace: dropping $DST.$SCHEMA and restarting at the copy"
  psqlc "$DST" -c "DROP SCHEMA IF EXISTS $SCHEMA CASCADE" -c "CREATE SCHEMA $SCHEMA AUTHORIZATION $ROLE" >/dev/null
  if exists_db "$STAGE" && [ "$(q "$STAGE" "SELECT count(*) FROM pg_namespace WHERE nspname = '$SCHEMA'")" = 1 ]; then
    set_note renamed
  else
    set_note frozen
  fi
fi

# freeze: terminology writes off (FHIR untouched; reads keep being served)
hook freeze "$FREEZE"; FROZE=1
done_step frozen || set_note frozen
timing freeze

# snapshot
if ! done_step snapshot; then
  if [ "$INPLACE" = 1 ]; then
    log "in-place: $SRC itself is renamed; its readers must stay paused until the switch"
    hook pause "$PAUSE"
  else
    if exists_db "$STAGE"; then
      log "snapshot: dropping a leftover $STAGE"
      psqlc postgres -c "DROP DATABASE $STAGE WITH (FORCE)" >/dev/null
    fi
    if [ "$SNAP" = template ]; then
      [ "$YES" = 1 ] || [ -z "$PAUSE" ] || { read -r -p "pause terminology reads for the template copy? [y/N] " a; [ "$a" = y ] || die "aborted"; }
      hook pause "$PAUSE"
      PAUSE_T=$(date +%s)
      # A template copy needs no other session on the source: refuse new
      # connections (the terminology pools get errors and retry), end the
      # open ones, copy, allow again. Re-allowed on any failure (trap).
      REALLOW=1
      psqlc postgres -c "ALTER DATABASE $SRC WITH ALLOW_CONNECTIONS false" >/dev/null
      psqlc postgres -c "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity WHERE datname = '$SRC' AND pid <> pg_backend_pid()" >/dev/null
      for _ in 1 2 3 4 5 6 7 8 9 10; do
        [ "$(q postgres "SELECT count(*) FROM pg_stat_activity WHERE datname = '$SRC'")" = 0 ] && break
        sleep 0.5
      done
      psqlc postgres -c "CREATE DATABASE $STAGE TEMPLATE $SRC OWNER $PGU" >/dev/null
      psqlc postgres -c "ALTER DATABASE $SRC WITH ALLOW_CONNECTIONS true" >/dev/null; REALLOW=0
      hook resume "$RESUME"
      log "snapshot: template copy done; terminology reads paused $(( $(date +%s) - PAUSE_T )) s"
      [ -z "$TIMINGS" ] || echo "read_pause $(( $(date +%s) - PAUSE_T ))" >> "$TIMINGS"
    else
      psqlc postgres -c "CREATE DATABASE $STAGE OWNER $PGU" >/dev/null
      for e in pg_trgm btree_gin; do psqlc "$STAGE" -c "CREATE EXTENSION IF NOT EXISTS $e" >/dev/null; done
      docker exec "$C" sh -c "pg_dump -U $PGU -Fc -d $SRC | pg_restore -U $PGU -d $STAGE --no-owner --no-privileges 2>&1 \
        | grep -v 'already exists\|errors ignored on restore\|while PROCESSING TOC\|from TOC entry\|Command was:' || true"
    fi
  fi
  set_note snapshot
  timing snapshot
fi

# rename: move every non-extension object of public into $SCHEMA (in the stage).
# Extensions (pg_trgm, btree_gin) stay in public, so the dumped DDL references
# public.gin_trgm_ops, which exists in the target database too. (Renaming the
# public schema itself would move the extensions with it.)
if ! done_step renamed; then
  psqlc "$STAGE" <<SQL >/dev/null
CREATE SCHEMA IF NOT EXISTS $SCHEMA;
DO \$\$
DECLARE r record;
BEGIN
  -- tables, views, materialized views, foreign tables (indexes, constraints,
  -- triggers and owned sequences follow their table)
  FOR r IN
    SELECT c.relkind, format('public.%I', c.relname) AS name FROM pg_class c
     WHERE c.relnamespace = 'public'::regnamespace AND c.relkind IN ('r','p','v','m','f')
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e')
  LOOP
    EXECUTE format('ALTER %s %s SET SCHEMA $SCHEMA',
      CASE r.relkind WHEN 'v' THEN 'VIEW' WHEN 'm' THEN 'MATERIALIZED VIEW' WHEN 'f' THEN 'FOREIGN TABLE' ELSE 'TABLE' END, r.name);
  END LOOP;
  -- sequences not owned by a column
  FOR r IN
    SELECT format('public.%I', c.relname) AS name FROM pg_class c
     WHERE c.relnamespace = 'public'::regnamespace AND c.relkind = 'S'
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype IN ('a','i','e'))
  LOOP
    EXECUTE format('ALTER SEQUENCE %s SET SCHEMA $SCHEMA', r.name);
  END LOOP;
  -- functions and procedures (trigger functions)
  FOR r IN
    SELECT p.oid::regprocedure::text AS sig, p.prokind FROM pg_proc p
     WHERE p.pronamespace = 'public'::regnamespace
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e')
  LOOP
    EXECUTE format('ALTER %s %s SET SCHEMA $SCHEMA', CASE r.prokind WHEN 'p' THEN 'PROCEDURE' WHEN 'a' THEN 'AGGREGATE' ELSE 'FUNCTION' END, r.sig);
  END LOOP;
  -- free-standing types (enums, composites, domains)
  FOR r IN
    SELECT format('public.%I', t.typname) AS name, t.typtype FROM pg_type t
      LEFT JOIN pg_class c ON c.oid = t.typrelid
     WHERE t.typnamespace = 'public'::regnamespace
       AND (t.typtype IN ('e', 'd') OR (t.typtype = 'c' AND c.relkind = 'c'))
       AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_type'::regclass AND d.objid = t.oid AND d.deptype = 'e')
  LOOP
    EXECUTE format('ALTER %s %s SET SCHEMA $SCHEMA', CASE r.typtype WHEN 'd' THEN 'DOMAIN' ELSE 'TYPE' END, r.name);
  END LOOP;
END
\$\$;
SQL
  left=$(q "$STAGE" "SELECT count(*) FROM pg_class c WHERE c.relnamespace = 'public'::regnamespace AND c.relkind IN ('r','p','v','m','S')
          AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e')")
  [ "$left" = 0 ] || die "rename: $left relations left in $STAGE.public"
  set_note renamed
  timing rename
fi

# copy: pre-data (DDL without indexes/constraints/triggers), data (one COPY
# stream, no triggers fire: they are post-data), post-data (indexes,
# constraints, triggers) with $JOBS parallel jobs. Runs inside the container;
# only the small pre/post-data archives touch disk (/tmp of the container).
if ! done_step copied; then
  [ "$(schema_tables "$DST" "$SCHEMA")" = 0 ] || die "$DST.$SCHEMA is not empty; re-run with --replace"
  psqlc "$DST" -c "CREATE SCHEMA IF NOT EXISTS $SCHEMA AUTHORIZATION $ROLE" -c "ALTER SCHEMA $SCHEMA OWNER TO $ROLE" >/dev/null
  W=/tmp/unify-db-$$
  docker exec "$C" sh -euc "
    mkdir -p $W
    pg_dump -U $PGU -d $STAGE -n $SCHEMA -Fc --section=pre-data  --no-owner --no-privileges -f $W/pre.dump
    pg_dump -U $PGU -d $STAGE -n $SCHEMA -Fc --section=post-data --no-owner --no-privileges -f $W/post.dump
    # the schema exists already (owned by $ROLE): restore everything but its CREATE SCHEMA
    pg_restore -l $W/pre.dump | grep -v ' SCHEMA - $SCHEMA ' > $W/pre.list"
  log "copy: pre-data"
  docker exec "$C" pg_restore -U "$PGU" -d "$DST" --no-owner --no-privileges --role="$ROLE" --exit-on-error -L /tmp/unify-db-$$/pre.list /tmp/unify-db-$$/pre.dump
  timing copy_pre_data
  log "copy: data (single COPY stream $STAGE → $DST)"
  docker exec "$C" sh -euc "set -o pipefail 2>/dev/null || true
    pg_dump -U $PGU -d $STAGE -n $SCHEMA -Fc -Z0 --section=data --no-owner --no-privileges \
      | pg_restore -U $PGU -d $DST --no-owner --no-privileges --role=$ROLE --exit-on-error --section=data"
  timing copy_data
  log "copy: post-data (-j $JOBS)"
  docker exec "$C" pg_restore -U "$PGU" -d "$DST" --no-owner --no-privileges --role="$ROLE" --exit-on-error -j "$JOBS" /tmp/unify-db-$$/post.dump
  docker exec "$C" rm -rf "/tmp/unify-db-$$"
  timing copy_post_data
  log "copy: ANALYZE $SCHEMA"
  docker exec "$C" vacuumdb -U "$PGU" -d "$DST" -n "$SCHEMA" -Z -j "$JOBS" -q
  timing analyze
  set_note copied
fi

# verify
if ! done_step verified; then
  verify || die "verification failed: the switch is not done; fix and re-run (--replace copies again)"
  if [ -n "$SMOKE" ]; then hook smoke "$SMOKE"; fi
  psqlc "$DST" -c "UPDATE public.hfs_terminology_state SET source_fingerprint = '$FPRINT' WHERE id" >/dev/null
  set_note verified
  timing verify
fi

# record
if ! done_step recorded; then
  psqlc "$DST" -c "UPDATE public.hfs_terminology_state SET store = 'schema:$SCHEMA', moved_from = 'database:$SRC',
                   moved_at = now(), updated_at = now() WHERE id" >/dev/null
  set_note recorded
fi

# switch
if ! done_step switched; then
  hook switch "$SWITCH"
  set_note switched
  timing switch
fi

hook unfreeze "$UNFREEZE"; FROZE=0
set_note done
log "done: terminology is in $DST.$SCHEMA. Keep $SRC (rollback) and $STAGE until the observation window ends, then: $0 cleanup"
