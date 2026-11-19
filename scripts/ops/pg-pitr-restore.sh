#!/usr/bin/env bash
# Restore a base backup into a NEW data directory and replay archived WAL up to
# a target time (or to the end of the archive), then start a throw-away
# PostgreSQL container on it for verification / promotion.
#
# Usage: pg-pitr-restore.sh BASE_DIR WAL_ARCHIVE_DIR NEW_DATA_DIR [TARGET_TIME] [PORT]
#   BASE_DIR         a base-<stamp> directory from pg-basebackup.sh
#   WAL_ARCHIVE_DIR  the archive_command destination (files <segment>.gz)
#   NEW_DATA_DIR     must not exist or be empty; becomes PGDATA
#   TARGET_TIME      e.g. '2026-11-15 12:34:56+08'; omit to replay everything
#   PORT             host port for the verification container (default 15432)
#
# Env: PITR_CONTAINER (default hfs-pitr-restore), PG_IMAGE (default
# postgres:16-alpine; must match the source major version).
# PG_BIN=/usr/lib/postgresql/16/bin runs the recovered server natively with
# pg_ctl instead of in a container (hosts without docker, or a wedged daemon);
# it listens on 127.0.0.1:PORT and its socket lives in NEW_DATA_DIR.
# The container runs with recovery_target_action=promote, so once recovery
# reaches the target it becomes a normal read-write server. To put it into
# production, stop HFS/HTS, point them at it (or move NEW_DATA_DIR into the
# production volume) -- docs/ops/backup-restore.md "Point-in-time recovery".
set -euo pipefail
BASE=$1; WAL=$(cd "$2" && pwd); NEW=$3; TARGET=${4:-}; PORT=${5:-15432}
NAME=${PITR_CONTAINER:-hfs-pitr-restore}; IMG=${PG_IMAGE:-postgres:16-alpine}
[ ! -e "$NEW" ] || [ -z "$(ls -A "$NEW")" ] || { echo "$NEW is not empty" >&2; exit 1; }
mkdir -p "$NEW"; NEW=$(cd "$NEW" && pwd)
t0=$(date +%s)
tar -xzf "$BASE/base.tar.gz" -C "$NEW"
mkdir -p "$NEW/pg_wal" && tar -xzf "$BASE/pg_wal.tar.gz" -C "$NEW/pg_wal"
if [ -n "${PG_BIN:-}" ]; then ARCH=$WAL; else ARCH=/wal-archive; fi
{
  echo "restore_command = 'gunzip < $ARCH/%f.gz > %p'"
  [ -n "$TARGET" ] && echo "recovery_target_time = '$TARGET'"
  echo "recovery_target_action = 'promote'"
  # the restored server must not archive into the production archive
  echo "archive_mode = 'off'"
} >> "$NEW/postgresql.auto.conf"
touch "$NEW/recovery.signal"
t1=$(date +%s)
if [ -n "${PG_BIN:-}" ]; then
  chmod 700 "$NEW"
  "$PG_BIN/pg_ctl" -D "$NEW" -l "$NEW/recovery.log" -o "-p $PORT -c listen_addresses=127.0.0.1 -c unix_socket_directories=$NEW" start >/dev/null
  echo "waiting for recovery to finish..."
  for _ in $(seq 1 3600); do
    [ "$("$PG_BIN/psql" -h "$NEW" -p "$PORT" -U "${PGUSER:-hfs}" -d postgres -Atc 'SELECT pg_is_in_recovery()' 2>/dev/null)" = f ] && break
    sleep 1
  done
else
# the postgres image runs as uid 70 (alpine) / 999 (debian); match the files
uid=$(docker run --rm --entrypoint id "$IMG" -u postgres)
docker run --rm -v "$NEW:/d" --entrypoint chown "$IMG" -R "$uid:$uid" /d
docker run --rm -v "$NEW:/d" --entrypoint chmod "$IMG" 700 /d
docker run -d --name "$NAME" -p "127.0.0.1:$PORT:5432" -v "$NEW:/var/lib/postgresql/data" -v "$WAL:/wal-archive:ro" "$IMG" >/dev/null
echo "waiting for recovery to finish..."
for _ in $(seq 1 3600); do
  if docker exec "$NAME" pg_isready -q 2>/dev/null && \
     [ "$(docker exec "$NAME" psql -U "${PGUSER:-hfs}" -d postgres -Atc 'SELECT pg_is_in_recovery()' 2>/dev/null)" = f ]; then
    break
  fi
  sleep 1
done
fi
t2=$(date +%s)
echo "extract $((t1 - t0))s, WAL replay+promote $((t2 - t1))s, total $((t2 - t0))s"
if [ -n "${PG_BIN:-}" ]; then
  grep -E "recovery stopping|redo done|last completed transaction|selected new timeline|consistent recovery state" "$NEW/recovery.log" | tail -5
  echo "server on 127.0.0.1:$PORT (stop with: $PG_BIN/pg_ctl -D $NEW stop)"
else
  docker logs "$NAME" 2>&1 | grep -E "recovery stopping|redo done|last completed transaction|selected new timeline|consistent recovery state" | tail -5
  echo "container $NAME on 127.0.0.1:$PORT (remove with: docker rm -f $NAME)"
fi
