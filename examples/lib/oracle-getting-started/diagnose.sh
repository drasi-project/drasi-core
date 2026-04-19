#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT_DIR"

if ! docker ps --format '{{.Names}}' | grep -q '^drasi-oracle-example$'; then
  echo "Oracle container is not running"
  exit 1
fi

docker exec -i drasi-oracle-example sh -lc 'sqlplus -s / as sysdba' <<'SQL'
SET PAGESIZE 100 LINESIZE 200
COLUMN LOG_MODE FORMAT A20
SELECT LOG_MODE, SUPPLEMENTAL_LOG_DATA_MIN FROM V$DATABASE;
EXIT;
SQL

docker exec -i drasi-oracle-example sh -lc 'sqlplus -s system/drasi123@//localhost:1521/FREEPDB1' <<'SQL'
SET PAGESIZE 100 LINESIZE 200
SELECT OWNER, TABLE_NAME FROM ALL_TABLES WHERE OWNER = 'SYSTEM' AND TABLE_NAME = 'DRASI_PRODUCTS';
EXIT;
SQL
