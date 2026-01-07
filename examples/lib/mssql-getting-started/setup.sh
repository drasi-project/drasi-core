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

echo "Setting up MS SQL Server for Drasi example..."

# Wait for SQL Server to be ready
echo "Waiting for SQL Server to be ready..."
for i in {1..30}; do
    if docker exec drasi-mssql-example /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "YourStrong!Passw0rd" -C -Q "SELECT 1" > /dev/null 2>&1; then
        echo "✓ SQL Server is ready"
        break
    fi
    if [ $i -eq 30 ]; then
        echo "✗ SQL Server failed to start"
        exit 1
    fi
    echo "  Waiting... ($i/30)"
    sleep 2
done

# Create database and enable CDC
echo "Creating database and enabling CDC..."
docker cp setup.sql drasi-mssql-example:/tmp/setup.sql
docker exec drasi-mssql-example /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "YourStrong!Passw0rd" -C -i /tmp/setup.sql

echo "✓ Database setup complete!"
echo ""
echo "Database: OrdersDB"
echo "Table: dbo.Orders"
echo "CDC: Enabled"
echo ""
echo "Initial orders:"
docker exec drasi-mssql-example /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "YourStrong!Passw0rd" -C -d OrdersDB -Q "SELECT OrderId, CustomerName, Status, TotalAmount FROM dbo.Orders" -Y 20

echo ""
echo "Ready to run: cargo run"
