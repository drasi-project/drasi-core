#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' 'Usage: bash change-data.sh {reset|stock-risk|unrelated|order-risk|restore|reenter} [--dry-run]' >&2
  exit 2
}

[[ $# -ge 1 && $# -le 2 ]] || usage
action=$1
dry_run=false
if [[ $# -eq 2 ]]; then
  [[ $2 == --dry-run ]] || usage
  dry_run=true
fi
compose="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/compose.yml"

run_sql() {
  local database=$1 sql
  sql=$(cat)
  printf '\n--- %s fixture database ---\n%s\n' "$database" "$sql"
  if "$dry_run"; then return; fi

  if [[ $database == mysql ]]; then
    printf '%s\n' "$sql" | docker compose -f "$compose" exec -T mysql sh -c \
      'export MYSQL_PWD="$MYSQL_ROOT_PASSWORD"; exec mysql --user=root --database=inventory --table'
  else
    printf '%s\n' "$sql" | docker compose -f "$compose" exec -T postgres psql \
      --username=postgres --dbname=orders --no-psqlrc --set=ON_ERROR_STOP=1 --pset=pager=off
  fi
}

case "$action" in
  stock-risk)
    run_sql mysql <<'SQL'
UPDATE inventory
SET available = 3
WHERE sku = 'SKU-A';

SELECT sku, available
FROM inventory
WHERE sku = 'SKU-A';
SQL
    ;;
  unrelated)
    run_sql postgres <<'SQL'
UPDATE orders
SET notes = 'Use recyclable packaging'
WHERE order_id = 'order-1001';

SELECT order_id, notes
FROM orders
WHERE order_id = 'order-1001';
SQL
    ;;
  order-risk)
    run_sql postgres <<'SQL'
UPDATE orders
SET quantity = 12
WHERE order_id = 'order-1002';

SELECT order_id, sku, quantity, status
FROM orders
WHERE order_id = 'order-1002';
SQL
    ;;
  restore)
    run_sql mysql <<'SQL'
UPDATE inventory
SET available = 10
WHERE sku = 'SKU-A';

SELECT sku, available
FROM inventory
WHERE sku = 'SKU-A';
SQL
    ;;
  reenter)
    run_sql mysql <<'SQL'
UPDATE inventory
SET available = 2
WHERE sku = 'SKU-A';

SELECT sku, available
FROM inventory
WHERE sku = 'SKU-A';
SQL
    ;;
  reset)
    printf '%s\n' 'Pause routines before resetting. Reset changes can otherwise invoke live agents.' >&2
    run_sql mysql <<'SQL'
UPDATE inventory
SET available = 10
WHERE sku IN ('SKU-A', 'SKU-B');

SELECT sku, available
FROM inventory
WHERE sku IN ('SKU-A', 'SKU-B')
ORDER BY sku;
SQL
    run_sql postgres <<'SQL'
UPDATE orders
SET quantity = CASE order_id
        WHEN 'order-1001' THEN 5
        WHEN 'order-1002' THEN 3
    END,
    status = 'pending',
    notes = ''
WHERE order_id IN ('order-1001', 'order-1002');

SELECT order_id, sku, quantity, status, notes
FROM orders
WHERE order_id IN ('order-1001', 'order-1002')
ORDER BY order_id;
SQL
    ;;
  *) usage ;;
esac
