#!/usr/bin/env bash
# Delete archived WAL that no kept base backup needs.
#
# Usage: wal-prune.sh BASEBACKUP_DIR WAL_ARCHIVE_DIR [--dry-run]
#
# Reads backup_label from the OLDEST base-<stamp>/base.tar.gz kept by
# pg-basebackup.sh and removes every archived segment (<segment>.gz) older than
# that backup's starting segment, with pg_archivecleanup (run from the postgres
# image, so the host needs no PostgreSQL tools). Point-in-time recovery stays
# possible from the oldest base backup onwards. Refuses to run without at
# least one complete base backup. PG_BIN=<dir with pg_archivecleanup> runs it
# natively instead of through docker.
set -euo pipefail
BASE=$1; WAL=$(cd "$2" && pwd); DRY=${3:-}
IMG=${PG_IMAGE:-postgres:16-alpine}
oldest=$(ls -1d "$BASE"/base-* 2>/dev/null | grep -v partial | sort | head -1 || true)
[ -n "$oldest" ] && [ -f "$oldest/base.tar.gz" ] || { echo "no complete base backup in $BASE; not pruning" >&2; exit 1; }
seg=$(tar -xzOf "$oldest/base.tar.gz" backup_label | sed -n 's/^START WAL LOCATION: .*(file \([0-9A-F]\{24\}\)).*/\1/p')
[ -n "$seg" ] || { echo "cannot read START WAL LOCATION from $oldest" >&2; exit 1; }
echo "oldest base backup $(basename "$oldest") starts at $seg"
before=$(find "$WAL" -maxdepth 1 -name '*.gz' | wc -l)
args=(-x .gz); [ "$DRY" = --dry-run ] && args+=(-n)
if [ -n "${PG_BIN:-}" ]; then
  "$PG_BIN/pg_archivecleanup" "${args[@]}" "$WAL" "$seg"
else
  docker run --rm -u "$(stat -c %u:%g "$WAL")" -v "$WAL:/wal" --entrypoint pg_archivecleanup "$IMG" "${args[@]}" /wal "$seg"
fi
after=$(find "$WAL" -maxdepth 1 -name '*.gz' | wc -l)
echo "archive: $before -> $after segments"
