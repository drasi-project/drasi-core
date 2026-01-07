#!/bin/bash
# Check CDC configuration

echo "=== CDC Database Status ==="
docker exec drasi-mssql-example /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "YourStrong!Passw0rd" -C -d OrdersDB -Q "SELECT name, is_cdc_enabled FROM sys.databases WHERE name = 'OrdersDB'" -Y 30

echo ""
echo "=== CDC Capture Instances ==="
docker exec drasi-mssql-example /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "YourStrong!Passw0rd" -C -d OrdersDB -Q "SELECT capture_instance, OBJECT_NAME(source_object_id) as source_table FROM cdc.change_tables" -Y 30

echo ""
echo "=== CDC Functions Available ==="
docker exec drasi-mssql-example /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "YourStrong!Passw0rd" -C -d OrdersDB -Q "SELECT SPECIFIC_NAME FROM INFORMATION_SCHEMA.ROUTINES WHERE SPECIFIC_SCHEMA = 'cdc' AND SPECIFIC_NAME LIKE 'fn_cdc_get_all_changes%'" -Y 50

echo ""
echo "=== Current LSN Info ==="
docker exec drasi-mssql-example /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "YourStrong!Passw0rd" -C -d OrdersDB -Q "SELECT sys.fn_cdc_get_max_lsn() AS max_lsn, sys.fn_cdc_get_min_lsn('dbo_Orders') AS min_lsn" -Y 30

echo ""
echo "=== Check for any CDC changes ==="
docker exec drasi-mssql-example /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -P "YourStrong!Passw0rd" -C -d OrdersDB -Q "DECLARE @from_lsn binary(10), @to_lsn binary(10); SET @from_lsn = sys.fn_cdc_get_min_lsn('dbo_Orders'); SET @to_lsn = sys.fn_cdc_get_max_lsn(); SELECT COUNT(*) as change_count FROM cdc.fn_cdc_get_all_changes_dbo_Orders(@from_lsn, @to_lsn, 'all')" -Y 30
