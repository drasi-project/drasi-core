#!/bin/bash
# Add a new order to the Orders table

set -e

if [ $# -ne 3 ]; then
    echo "Usage: $0 <customer_name> <status> <total_amount>"
    echo "Example: $0 \"John Doe\" \"Pending\" 1500.00"
    exit 1
fi

CUSTOMER_NAME=$1
STATUS=$2
TOTAL_AMOUNT=$3

echo "Adding new order:"
echo "  Customer: $CUSTOMER_NAME"
echo "  Status: $STATUS"
echo "  Amount: \$$TOTAL_AMOUNT"

docker compose exec mssql /opt/mssql-tools18/bin/sqlcmd \
    -S localhost \
    -U sa \
    -P 'YourStrong!Passw0rd' \
    -d OrdersDB \
    -C \
    -Q "INSERT INTO dbo.Orders (CustomerName, Status, TotalAmount, CreatedAt) VALUES ('$CUSTOMER_NAME', '$STATUS', $TOTAL_AMOUNT, GETDATE())"

echo "✓ Order added successfully"
echo "Watch the application logs to see the CDC change!"
