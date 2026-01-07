#!/bin/bash
# Update an order's total amount

set -e

if [ $# -ne 2 ]; then
    echo "Usage: $0 <order_id> <new_amount>"
    echo "Example: $0 1 2500.00"
    exit 1
fi

ORDER_ID=$1
NEW_AMOUNT=$2

echo "Updating order #$ORDER_ID to amount: \$$NEW_AMOUNT"

docker compose exec mssql /opt/mssql-tools18/bin/sqlcmd \
    -S localhost \
    -U sa \
    -P 'YourStrong!Passw0rd' \
    -d OrdersDB \
    -C \
    -Q "UPDATE dbo.Orders SET TotalAmount = $NEW_AMOUNT WHERE OrderId = $ORDER_ID"

echo "✓ Order amount updated successfully"
echo "Watch the application logs to see the CDC change!"
