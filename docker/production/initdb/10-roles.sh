#!/bin/sh
# Runs once, on an empty data directory. Creates one role + database per server
# so a compromised or misbehaving server cannot touch the other's data, and so
# pg_stat_activity / logs show which server a connection belongs to.
set -eu
psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d postgres <<SQL
CREATE ROLE hfs LOGIN PASSWORD '${HFS_DB_PASSWORD}';
CREATE ROLE hts LOGIN PASSWORD '${HTS_DB_PASSWORD}';
CREATE DATABASE hfs OWNER hfs;
CREATE DATABASE hts OWNER hts;
SQL
for db in hfs hts; do
  # pg_trgm: HFS creates it itself when allowed; pre-creating avoids needing
  # superuser at runtime. pg_stat_statements: the index review loop.
  psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$db" <<SQL
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS btree_gin;
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
SQL
done
