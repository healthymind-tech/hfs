#!/usr/bin/env bash
# Physical base backup for point-in-time recovery (PITR).
#
# Usage: pg-basebackup.sh [-o OUT_DIR] [-k KEEP_COUNT]
#
# Requires the server to archive WAL (archive_mode=on + archive_command, see
# docker/production/docker-compose.yml) and a role with REPLICATION (the
# superuser created by the postgres image qualifies). Writes
# <OUT>/base-<stamp>/{base.tar.gz,pg_wal.tar.gz,backup_manifest} and verifies it
# with pg_verifybackup. A base backup plus every archived WAL segment after it
# can be replayed to any moment up to the newest archived segment
# (pg-pitr-restore.sh). Keep at least two base backups; WAL older than the
# oldest kept base backup can be pruned (see -k and docs/ops/backup-restore.md).
#
# Connection: PG_CONTAINER=<container> runs pg_basebackup inside the database
# container (output is streamed out as tar), otherwise local libpq variables.
set -euo pipefail
OUT=${BASEBACKUP_DIR:-./basebackups}; KEEP=${KEEP_COUNT:-2}
while getopts "o:k:h" opt; do
  case $opt in o) OUT=$OPTARG;; k) KEEP=$OPTARG;; h) sed -n '2,17p' "$0"; exit 0;; *) exit 2;; esac
done
PGU=${PGUSER:-hfs}
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
D="$OUT/base-$STAMP"; mkdir -p "$D.partial"
t0=$(date +%s)
if [ -n "${PG_CONTAINER:-}" ]; then
  tmp=/tmp/basebackup-$STAMP
  docker exec "$PG_CONTAINER" pg_basebackup -U "$PGU" -D "$tmp" -Ft -z -X stream -c fast -l "hfs-$STAMP"
  docker exec "$PG_CONTAINER" pg_verifybackup -n "$tmp" >/dev/null 2>&1 || \
    docker exec "$PG_CONTAINER" sh -c "cd $tmp && tar -tzf base.tar.gz >/dev/null && tar -tzf pg_wal.tar.gz >/dev/null"
  docker cp "$PG_CONTAINER:$tmp/." "$D.partial/"
  docker exec "$PG_CONTAINER" rm -rf "$tmp"
else
  pg_basebackup -U "$PGU" -D "$D.partial" -Ft -z -X stream -c fast -l "hfs-$STAMP"
fi
(cd "$D.partial" && tar -tzf base.tar.gz >/dev/null && tar -tzf pg_wal.tar.gz >/dev/null)
mv "$D.partial" "$D"
(cd "$D" && sha256sum ./*.gz backup_manifest 2>/dev/null > SHA256SUMS || true)
echo "base backup $D $(du -sh "$D" | cut -f1) in $(( $(date +%s) - t0 ))s"
# prune: keep the newest $KEEP base backups
ls -1d "$OUT"/base-* 2>/dev/null | grep -v partial | sort | head -n -"$KEEP" | while read -r old; do
  echo "prune $old"; rm -rf "$old"
done
