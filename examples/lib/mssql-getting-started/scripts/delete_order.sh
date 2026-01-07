#!/bin/bash
# Delete an order

set -e

if [ $# -ne 1 ]; then
    echo "Usage: $0 <order_id>"
    echo "Example: $0 3"
    exit 1
fi

ORDER_ID=$1

echo "Deleting order #$ORDER_ID"

docker compose exec mssql /opt/mssql-tools18/bin/sqlcmd \
    -S localhost \
    -U sa \
    -P 'YourStrong!Passw0rd' \
    -d OrdersDB \
    -C \
    -Q "DELETE FROM dbo.Orders WHERE OrderId = $ORDER_ID"

echo "✓ Order deleted successfully"
echo "Watch the application logs to see the CDC change!"
