#!/usr/bin/env bash
# Restore a pg-backup.sh dump into a NEW, empty database and verify it.
#
# Usage: pg-restore.sh DUMP TARGET_DB [-j JOBS] [--replace] [--owner ROLE]
#   DUMP       a .dump file (custom format) or .dump.dir directory
#   TARGET_DB  database to create; refuses to touch an existing one unless
#              --replace is given (which DROPs it first -- stop HFS/HTS first!)
#   --owner    create the database owned by ROLE and restore every object as
#              ROLE (pg_restore --role), so the application role owns what it
#              uses -- use it for production restores (hfs / hts). Extensions
#              are created first by the connecting superuser.
#
# Connection as for pg-backup.sh (PG_CONTAINER or libpq variables). The dump's
# sha256 is checked when <DUMP>.sha256 exists. Ownership is mapped to the
# connecting role (--no-owner), so a dump from another cluster restores as-is.
# Prints the elapsed time and a short verification (schema version, row counts).
set -euo pipefail
DUMP=$1; DB=$2; shift 2
JOBS=4; REPLACE=0; OWNER=""
while [ $# -gt 0 ]; do
  case $1 in -j) JOBS=$2; shift 2;; --replace) REPLACE=1; shift;; --owner) OWNER=$2; shift 2;;
    *) echo "unknown $1" >&2; exit 2;; esac
done
PGU=${PGUSER:-hfs}
run() { if [ -n "${PG_CONTAINER:-}" ]; then docker exec -i "$PG_CONTAINER" "$@"; else "$@"; fi; }
psqlc() { run psql -U "$PGU" -v ON_ERROR_STOP=1 -At "$@"; }

if [ -f "$DUMP.sha256" ]; then
  if [ -d "$DUMP" ]; then (cd "$DUMP" && sha256sum -c --quiet "../$(basename "$DUMP").sha256")
  else (cd "$(dirname "$DUMP")" && sha256sum -c --quiet "$(basename "$DUMP").sha256"); fi
  echo "checksum ok"
fi
exists=$(psqlc -d postgres -c "SELECT 1 FROM pg_database WHERE datname = '$DB'")
if [ "$exists" = 1 ]; then
  [ $REPLACE = 1 ] || { echo "database $DB exists; pass --replace to drop it" >&2; exit 1; }
  psqlc -d postgres -c "DROP DATABASE \"$DB\" WITH (FORCE)"
fi
RARGS=(--no-owner --no-acl -j "$JOBS" --exit-on-error)
if [ -n "$OWNER" ]; then
  psqlc -d postgres -c "CREATE DATABASE \"$DB\" OWNER \"$OWNER\""
  # extensions (and their comments) need a superuser: create them up front and
  # leave them out of the role-owned restore
  if [ -d "$DUMP" ]; then toc=$(pg_restore -l "$DUMP"); else toc=$(run pg_restore -l < "$DUMP"); fi
  for ext in $(printf '%s\n' "$toc" | awk '/^[0-9]+; [0-9]+ [0-9]+ EXTENSION - /{print $6}'); do
    psqlc -d "$DB" -c "CREATE EXTENSION IF NOT EXISTS \"$ext\"" >/dev/null
  done
  LIST=$(mktemp); printf '%s\n' "$toc" | grep -v -E ' (EXTENSION|COMMENT - EXTENSION) ' > "$LIST"
  if [ -n "${PG_CONTAINER:-}" ]; then docker cp "$LIST" "$PG_CONTAINER:/tmp/restore-$$.list"; RARGS+=(-L "/tmp/restore-$$.list")
  else RARGS+=(-L "$LIST"); fi
  RARGS+=(--role="$OWNER")
else
  psqlc -d postgres -c "CREATE DATABASE \"$DB\""
fi
t0=$(date +%s)
if [ -d "$DUMP" ]; then
  [ -z "${PG_CONTAINER:-}" ] || { echo "directory dumps need a local pg_restore" >&2; exit 2; }
  pg_restore -U "$PGU" -d "$DB" "${RARGS[@]}" "$DUMP"
elif [ -n "${PG_CONTAINER:-}" ]; then
  # -j needs a seekable file, so copy the archive into the container first
  tmp=/tmp/restore-$$.dump
  docker cp "$DUMP" "$PG_CONTAINER:$tmp"
  run pg_restore -U "$PGU" -d "$DB" "${RARGS[@]}" "$tmp"
  run rm -f "$tmp" "/tmp/restore-$$.list"
else
  pg_restore -U "$PGU" -d "$DB" "${RARGS[@]}" "$DUMP"
fi
[ -z "${LIST:-}" ] || rm -f "$LIST"
t1=$(date +%s)
psqlc -d "$DB" -c "ANALYZE" >/dev/null
echo "restored $DB in $((t1 - t0))s (+ANALYZE $(( $(date +%s) - t1 ))s), size $(psqlc -d "$DB" -c "SELECT pg_size_pretty(pg_database_size(current_database()))")"
if [ "$(psqlc -d "$DB" -c "SELECT to_regclass('resources') IS NOT NULL")" = t ]; then
  echo "HFS schema_version: $(psqlc -d "$DB" -c 'SELECT version FROM schema_version')"
  [ "$(psqlc -d "$DB" -c "SELECT to_regclass('hm_schema_migrations') IS NOT NULL")" = t ] && \
    echo "HFS fork migrations: $(psqlc -d "$DB" -c "SELECT string_agg(name, ',' ORDER BY name) FROM hm_schema_migrations")"
  echo "live resources: $(psqlc -d "$DB" -c 'SELECT count(*) FROM resources WHERE NOT is_deleted')"
  echo "history rows:   $(psqlc -d "$DB" -c 'SELECT count(*) FROM resource_history')"
fi
if [ "$(psqlc -d "$DB" -c "SELECT to_regclass('concepts') IS NOT NULL")" = t ]; then
  echo "HTS code systems: $(psqlc -d "$DB" -c 'SELECT count(*) FROM code_systems')"
  echo "HTS concepts:     $(psqlc -d "$DB" -c 'SELECT count(*) FROM concepts')"
fi
