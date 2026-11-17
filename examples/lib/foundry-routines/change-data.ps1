[CmdletBinding()]
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidateSet('reset', 'stock-risk', 'unrelated', 'order-risk', 'restore', 'reenter')]
    [string]$Action,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$compose = Join-Path $PSScriptRoot 'compose.yml'

function Invoke-DemoSql([string]$Database, [string]$Sql) {
    Write-Output "`n--- $Database fixture database ---"
    Write-Output $Sql
    if ($DryRun) { return }

    if ($Database -eq 'mysql') {
        $Sql | docker compose -f $compose exec -T mysql sh -c `
            'export MYSQL_PWD="$MYSQL_ROOT_PASSWORD"; exec mysql --user=root --database=inventory --table'
    } else {
        $Sql | docker compose -f $compose exec -T postgres psql `
            --username=postgres --dbname=orders --no-psqlrc --set=ON_ERROR_STOP=1 --pset=pager=off
    }
    if ($LASTEXITCODE -ne 0) { throw "$Database SQL failed (exit $LASTEXITCODE)." }
}

switch ($Action) {
    'stock-risk' {
        Invoke-DemoSql mysql @'
UPDATE inventory
SET available = 3
WHERE sku = 'SKU-A';

SELECT sku, available
FROM inventory
WHERE sku = 'SKU-A';
'@
    }
    'unrelated' {
        Invoke-DemoSql postgres @'
UPDATE orders
SET notes = 'Use recyclable packaging'
WHERE order_id = 'order-1001';

SELECT order_id, notes
FROM orders
WHERE order_id = 'order-1001';
'@
    }
    'order-risk' {
        Invoke-DemoSql postgres @'
UPDATE orders
SET quantity = 12
WHERE order_id = 'order-1002';

SELECT order_id, sku, quantity, status
FROM orders
WHERE order_id = 'order-1002';
'@
    }
    'restore' {
        Invoke-DemoSql mysql @'
UPDATE inventory
SET available = 10
WHERE sku = 'SKU-A';

SELECT sku, available
FROM inventory
WHERE sku = 'SKU-A';
'@
    }
    'reenter' {
        Invoke-DemoSql mysql @'
UPDATE inventory
SET available = 2
WHERE sku = 'SKU-A';

SELECT sku, available
FROM inventory
WHERE sku = 'SKU-A';
'@
    }
    'reset' {
        Write-Warning 'Pause routines before resetting. Reset changes can otherwise invoke live agents.'
        Invoke-DemoSql mysql @'
UPDATE inventory
SET available = 10
WHERE sku IN ('SKU-A', 'SKU-B');

SELECT sku, available
FROM inventory
WHERE sku IN ('SKU-A', 'SKU-B')
ORDER BY sku;
'@
        Invoke-DemoSql postgres @'
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
'@
    }
}
