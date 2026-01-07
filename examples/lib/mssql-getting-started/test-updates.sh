#!/bin/bash
# Copyright 2025 The Drasi Authors.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

set -e

echo "Running test updates on Orders table..."
echo ""

# Function to execute SQL
exec_sql() {
    docker exec drasi-mssql-example /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "YourStrong!Passw0rd" -C -d OrdersDB -Q "$1"
}

echo "1. Updating order status from Pending to Shipped..."
exec_sql "UPDATE dbo.Orders SET Status = 'Shipped' WHERE OrderId = 1"
echo "   Watch for: [pending-orders] - Order removed"
echo "   Watch for: [all-orders] ~ Order status change"
sleep 3

echo ""
echo "2. Inserting a new large order..."
exec_sql "INSERT INTO dbo.Orders (CustomerName, Status, TotalAmount) VALUES ('Frank Castle', 'Pending', 3500.00)"
echo "   Watch for: [all-orders] + New order"
echo "   Watch for: [pending-orders] + New order"
echo "   Watch for: [large-orders] + New order (over $1000)"
sleep 3

echo ""
echo "3. Updating order amount to exceed $1000 threshold..."
exec_sql "UPDATE dbo.Orders SET TotalAmount = 1250.00 WHERE OrderId = 2"
echo "   Watch for: [large-orders] + Order now qualifies"
sleep 3

echo ""
echo "4. Completing a pending order..."
exec_sql "UPDATE dbo.Orders SET Status = 'Completed' WHERE OrderId = 3"
echo "   Watch for: [pending-orders] - Order removed"
echo "   Watch for: [all-orders] ~ Status update"
sleep 3

echo ""
echo "5. Inserting another order..."
exec_sql "INSERT INTO dbo.Orders (CustomerName, Status, TotalAmount) VALUES ('Grace Hopper', 'Pending', 575.50)"
echo "   Watch for: [all-orders] + New order"
echo "   Watch for: [pending-orders] + New order"
sleep 3

echo ""
echo "6. Deleting an order..."
exec_sql "DELETE FROM dbo.Orders WHERE OrderId = 4"
echo "   Watch for: [all-orders] - Order deleted"
sleep 3

echo ""
echo "Test updates complete!"
echo ""
echo "Current orders:"
exec_sql "SELECT OrderId, CustomerName, Status, TotalAmount FROM dbo.Orders" -Y 20
