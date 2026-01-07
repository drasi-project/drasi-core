#!/bin/bash
# Update an order's status

set -e

if [ $# -ne 2 ]; then
    echo "Usage: $0 <order_id> <new_status>"
    echo "Example: $0 1 \"Shipped\""
    echo "Valid statuses: Pending, Shipped, Completed, Cancelled"
    exit 1
fi

ORDER_ID=$1
NEW_STATUS=$2

echo "Updating order #$ORDER_ID to status: $NEW_STATUS"

docker compose exec mssql /opt/mssql-tools18/bin/sqlcmd \
    -S localhost \
    -U sa \
    -P 'YourStrong!Passw0rd' \
    -d OrdersDB \
    -C \
    -Q "UPDATE dbo.Orders SET Status = '$NEW_STATUS' WHERE OrderId = $ORDER_ID"

echo "✓ Order status updated successfully"
echo "Watch the application logs to see the CDC change!"
