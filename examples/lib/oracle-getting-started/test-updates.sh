#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT_DIR"

docker exec -i drasi-oracle-example sh -lc 'sqlplus -s system/drasi123@//localhost:1521/FREEPDB1' <<'SQL'
WHENEVER SQLERROR EXIT FAILURE
INSERT INTO SYSTEM.DRASI_PRODUCTS (ID, NAME, PRICE) VALUES (1, 'Widget', 19.99);
COMMIT;
UPDATE SYSTEM.DRASI_PRODUCTS SET NAME = 'Widget Updated', PRICE = 21.50 WHERE ID = 1;
COMMIT;
DELETE FROM SYSTEM.DRASI_PRODUCTS WHERE ID = 1;
COMMIT;
EXIT;
SQL

echo "INSERT/UPDATE/DELETE statements executed"
