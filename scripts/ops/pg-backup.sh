#!/usr/bin/env bash
# Logical backup of the HFS and HTS PostgreSQL databases (pg_dump custom format).
#
# Usage: pg-backup.sh [-o OUT_DIR] [-k KEEP_DAYS] [-j JOBS] [DB ...]
#   DB defaults to "hfs hts" (those that exist). OUT_DIR defaults to $BACKUP_DIR or ./backups.
#
# Connection: either set PG_CONTAINER (run pg_dump inside that docker container,
# the docker compose layout) or the usual libpq variables (PGHOST, PGPORT,
# PGUSER, PGPASSWORD / ~/.pgpass) to run a local pg_dump of the same major
# version as the server.
#
# Every dump is written to <name>.dump.partial, verified with pg_restore --list,
# checksummed (sha256) and only then renamed, so a half-written file is never
# mistaken for a backup. Exit status is non-zero if any database failed.
# Also writes <stamp>-globals.sql (roles; passwords included -- protect it).
#
# Logical dumps are consistent per database (one snapshot each), not across the
# two databases; HFS and HTS do not reference each other's rows, so that is fine.
# For point-in-time recovery use pg-basebackup.sh + WAL archiving instead
# (docs/ops/backup-restore.md).
#
# Monitoring: with PROM_TEXTFILE_DIR set (node_exporter --collector.textfile.directory),
# writes hfs_backup.prom with hfs_backup_last_status{db} (0 = ok),
# hfs_backup_last_success_timestamp_seconds{db}, hfs_backup_last_size_bytes{db}
# and hfs_backup_last_duration_seconds{db}; docker/monitoring/alerts.yml alerts on them.
set -euo pipefail
OUT=${BACKUP_DIR:-./backups}; KEEP=${KEEP_DAYS:-14}; JOBS=1
while getopts "o:k:j:h" opt; do
  case $opt in
    o) OUT=$OPTARG;; k) KEEP=$OPTARG;; j) JOBS=$OPTARG;;
    h) sed -n '2,20p' "$0"; exit 0;; *) exit 2;;
  esac
done
shift $((OPTIND - 1))
DBS=("$@"); DEFAULT_DBS=0; [ ${#DBS[@]} -gt 0 ] || { DBS=(hfs hts); DEFAULT_DBS=1; }
mkdir -p "$OUT"
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
PGU=${PGUSER:-hfs}

run() {  # run a postgres client tool locally or inside $PG_CONTAINER
  if [ -n "${PG_CONTAINER:-}" ]; then docker exec -i "$PG_CONTAINER" "$@"; else "$@"; fi
}

log() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*"; }
rc=0
PROM=""
metric() {  # metric <db> <status> <duration> <size>
  [ -n "${PROM_TEXTFILE_DIR:-}" ] || return 0
  local now; now=$(date +%s)
  PROM+="hfs_backup_last_status{db=\"$1\"} $2"$'\n'
  PROM+="hfs_backup_last_duration_seconds{db=\"$1\"} $3"$'\n'
  if [ "$2" = 0 ]; then
    PROM+="hfs_backup_last_success_timestamp_seconds{db=\"$1\"} $now"$'\n'
    PROM+="hfs_backup_last_size_bytes{db=\"$1\"} $4"$'\n'
  else
    # keep the previous success timestamp so BackupMissing still fires on time
    local prev; prev=$(grep "^hfs_backup_last_success_timestamp_seconds{db=\"$1\"}" "$PROM_TEXTFILE_DIR/hfs_backup.prom" 2>/dev/null || true)
    [ -n "$prev" ] && PROM+="$prev"$'\n'
  fi
}
if [ "$DEFAULT_DBS" = 1 ]; then
  # One database (unified server, docs/ops/unified-server.md): after the move
  # and its cleanup there is no `hts` database; terminology is schema
  # `terminology` inside the `hfs` dump. Skip default databases that are gone.
  present=()
  for db in "${DBS[@]}"; do
    # only a definite "0" skips; a failed query keeps it (and fails loudly below)
    if [ "$(run psql -X -U "$PGU" -d postgres -Atc "SELECT count(*) FROM pg_database WHERE datname = '$db'" 2>/dev/null)" = 0 ]; then
      log "skip $db (no such database)"
    else
      present+=("$db")
    fi
  done
  DBS=("${present[@]}")
fi
for db in "${DBS[@]}"; do
  f="$OUT/$db-$STAMP.dump"
  t0=$(date +%s)
  log "dump $db -> $f"
  if [ "$JOBS" -gt 1 ] && [ -z "${PG_CONTAINER:-}" ]; then
    # directory format is the only one pg_dump parallelises
    if ! pg_dump -U "$PGU" -d "$db" -Fd -j "$JOBS" -Z 6 -f "$f.dir.partial"; then
      log "FAILED $db"; rc=1; rm -rf "$f.dir.partial"; metric "$db" 1 $(( $(date +%s) - t0 )) 0; continue
    fi
    mv "$f.dir.partial" "$f.dir"; f="$f.dir"
    (cd "$f" && find . -type f -print0 | sort -z | xargs -0 sha256sum) > "$f.sha256"
  else
    if ! run pg_dump -U "$PGU" -d "$db" -Fc -Z 6 > "$f.partial"; then
      log "FAILED $db"; rc=1; rm -f "$f.partial"; metric "$db" 1 $(( $(date +%s) - t0 )) 0; continue
    fi
    if ! run pg_restore --list < "$f.partial" > /dev/null; then
      log "FAILED $db: archive does not list"; rc=1; metric "$db" 1 $(( $(date +%s) - t0 )) 0; continue
    fi
    mv "$f.partial" "$f"
    sha256sum "$f" > "$f.sha256"
  fi
  log "ok $db $(du -sh "$f" | cut -f1) in $(( $(date +%s) - t0 ))s"
  metric "$db" 0 $(( $(date +%s) - t0 )) "$(du -sb "$f" | cut -f1)"
done
if [ -n "${PROM_TEXTFILE_DIR:-}" ]; then
  printf '%s' "$PROM" > "$PROM_TEXTFILE_DIR/hfs_backup.prom.$$" && mv "$PROM_TEXTFILE_DIR/hfs_backup.prom.$$" "$PROM_TEXTFILE_DIR/hfs_backup.prom"
fi
run pg_dumpall -U "$PGU" --globals-only > "$OUT/$STAMP-globals.sql" || { log "FAILED globals"; rc=1; }
chmod 600 "$OUT/$STAMP-globals.sql" 2>/dev/null || true

if [ "$KEEP" -gt 0 ]; then
  find "$OUT" -maxdepth 1 \( -name '*.dump' -o -name '*.dump.sha256' -o -name '*-globals.sql' \) -mtime +"$KEEP" -print -delete
  find "$OUT" -maxdepth 1 -name '*.dump.dir' -type d -mtime +"$KEEP" -print -exec rm -rf {} +
fi
exit $rc
