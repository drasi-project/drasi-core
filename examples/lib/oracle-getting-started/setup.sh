#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT_DIR"

docker compose up -d

echo "Waiting for Oracle to be ready..."
for i in {1..60}; do
  if docker exec -i drasi-oracle-example sh -lc 'sqlplus -s system/drasi123@//localhost:1521/FREEPDB1' <<'SQL' >/dev/null 2>&1
SET HEADING OFF FEEDBACK OFF VERIFY OFF PAGESIZE 0
SELECT 1 FROM DUAL;
EXIT;
SQL
  then
    echo "Oracle is ready"
    break
  fi

  if [ "$i" -eq 60 ]; then
    echo "Oracle did not become ready"
    docker logs drasi-oracle-example --tail 40
    exit 1
  fi

  if [ $((i % 10)) -eq 0 ]; then
    echo "  Still waiting... ($i/60)"
  fi
  sleep 2
done

sleep 5

LOG_MODE=$(docker exec -i drasi-oracle-example sh -lc 'sqlplus -s / as sysdba' <<'SQL'
SET HEADING OFF FEEDBACK OFF VERIFY OFF PAGESIZE 0
SELECT LOG_MODE FROM V$DATABASE;
EXIT;
SQL
)

if ! printf '%s\n' "$LOG_MODE" | grep -qx 'ARCHIVELOG'; then
  docker exec -i drasi-oracle-example sh -lc 'sqlplus -s / as sysdba' <<'SQL'
WHENEVER SQLERROR EXIT FAILURE
SHUTDOWN IMMEDIATE;
STARTUP MOUNT;
ALTER DATABASE ARCHIVELOG;
ALTER DATABASE OPEN;
EXIT;
SQL
fi

docker exec -i drasi-oracle-example sh -lc 'sqlplus -s / as sysdba' <<'SQL'
WHENEVER SQLERROR EXIT FAILURE
BEGIN
  EXECUTE IMMEDIATE 'ALTER DATABASE ADD SUPPLEMENTAL LOG DATA';
EXCEPTION
  WHEN OTHERS THEN
    IF SQLCODE NOT IN (-32588, -32017) THEN RAISE; END IF;
END;
/
EXIT;
SQL

docker exec -i drasi-oracle-example sh -lc 'sqlplus -s system/drasi123@//localhost:1521/FREEPDB1' <<'SQL'
WHENEVER SQLERROR EXIT FAILURE
BEGIN
  EXECUTE IMMEDIATE 'DROP TABLE SYSTEM.DRASI_PRODUCTS PURGE';
EXCEPTION
  WHEN OTHERS THEN NULL;
END;
/
CREATE TABLE SYSTEM.DRASI_PRODUCTS (
  ID NUMBER PRIMARY KEY,
  NAME VARCHAR2(100) NOT NULL,
  PRICE NUMBER(10,2) NOT NULL
);
ALTER TABLE SYSTEM.DRASI_PRODUCTS ADD SUPPLEMENTAL LOG DATA (ALL) COLUMNS;
EXIT;
SQL

echo "Oracle example setup complete"
