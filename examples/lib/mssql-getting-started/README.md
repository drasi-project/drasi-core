# MS SQL Getting Started Example

This example demonstrates how to use DrasiLib with MS SQL Server for real-time order monitoring using Change Data Capture (CDC).

## What It Demonstrates

- **MS SQL Source**: Connects to MS SQL Server and streams CDC changes
- **MS SQL Bootstrapper**: Loads initial data from tables  
- **Continuous Queries**: Three Cypher queries monitoring orders in different ways
- **Log Reaction**: Prints changes to console in real-time
- **Results API**: HTTP endpoint to query current results

## Architecture

```
MS SQL Server (Docker)
    ↓ (CDC Stream)
MS SQL Source
    ↓ (Events)
Continuous Queries (3)
    - all-orders: All orders
    - pending-orders: Status = 'Pending'
    - large-orders: TotalAmount > $1000
    ↓ (Results)
Log Reaction + Results API
```

## Prerequisites

- Docker and Docker Compose
- Rust toolchain
- curl and jq (for testing)

## Quick Start

### 1. Start MS SQL Server

```bash
docker compose up -d
```

Wait ~30 seconds for SQL Server to be ready:

```bash
docker compose logs mssql | grep "SQL Server is now ready"
```

### 2. Initialize Database

The setup script creates the OrdersDB database, enables CDC, and loads sample data:

```bash
./setup.sh
```

This creates 5 initial orders in various states.

### 3. Run the Example

```bash
cargo run --release
```

You should see:
- Bootstrap loading initial 5 orders
- CDC stream starting
- Log reaction showing changes in real-time

### 4. Test CDC Changes

In another terminal, run the test scripts:

```bash
# Add a new order
./scripts/add_order.sh "John Doe" "Pending" 2500.00

# Update an order status
./scripts/update_status.sh 1 "Shipped"

# Update order amount  
./scripts/update_amount.sh 2 750.00

# Delete an order
./scripts/delete_order.sh 3
```

Watch the main terminal - you'll see the log reaction print each change in real-time!

### 5. Query Results via API

```bash
# All orders
curl http://localhost:8080/queries/all-orders/results | jq '.'

# Just pending orders
curl http://localhost:8080/queries/pending-orders/results | jq '.'

# Large orders (> $1000)
curl http://localhost:8080/queries/large-orders/results | jq '.'
```

**Note**: Due to a current limitation in DrasiLib (bootstrap events don't populate `current_results`), the Results API will only show changes that occurred AFTER the application started, not the bootstrapped data. This is a known issue being addressed. The Log Reaction will show all changes including bootstrap data.

## Project Structure

```
mssql-getting-started/
├── src/
│   └── main.rs              # Main application logic
├── scripts/
│   ├── add_order.sh         # Add new order
│   ├── update_status.sh     # Update order status
│   ├── update_amount.sh     # Update order amount
│   └── delete_order.sh      # Delete order
├── docker-compose.yml       # MS SQL Server container
├── setup.sh                 # Database initialization
└── README.md               # This file
```

## Queries Explained

### all-orders
```cypher
MATCH (o:Orders)
RETURN o.OrderId AS order_id,
       o.CustomerName AS customer_name,
       o.Status AS status,
       o.TotalAmount AS total_amount
```
Returns all orders with their key fields.

### pending-orders
```cypher
MATCH (o:Orders)
WHERE o.Status = 'Pending'
RETURN o.OrderId AS order_id,
       o.CustomerName AS customer_name,
       o.TotalAmount AS total_amount
```
Filters to only pending orders - demonstrates WHERE clause.

### large-orders
```cypher
MATCH (o:Orders)
WHERE o.TotalAmount > 1000
RETURN o.OrderId AS order_id,
       o.CustomerName AS customer_name,
       o.Status AS status,
       o.TotalAmount AS total_amount
```
Shows orders over $1000 - demonstrates numeric filtering.

## MS SQL CDC Configuration

CDC is enabled on the `dbo.Orders` table with:
- **Capture Instance**: `dbo_Orders`
- **Retention**: 3 days (4320 minutes)
- **Poll Interval**: 2 seconds (configured in source)

The CDC stream polls `cdc.fn_cdc_get_all_changes_dbo_Orders()` for changes and persists the LSN checkpoint to avoid reprocessing.

## Cleanup

```bash
# Stop the application (Ctrl+C)

# Stop and remove containers
docker compose down -v
```

## Troubleshooting

### SQL Server not starting
Check logs: `docker compose logs mssql`

### CDC not capturing changes
Verify CDC is enabled:
```bash
docker compose exec mssql /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P 'YourStrong!Passw0rd' -d OrdersDB -C \
  -Q "SELECT name FROM sys.tables WHERE schema_name(schema_id) = 'cdc'"
```

Should show `dbo_Orders_CT` table.

### No changes showing up
- Check SQL Server logs: `docker compose logs -f mssql`  
- Enable debug logging: `RUST_LOG=debug cargo run --release`
- Verify LSN range: See CDC tables in database

## Further Reading

- [MS SQL CDC Documentation](https://docs.microsoft.com/en-us/sql/relational-databases/track-changes/about-change-data-capture-sql-server)
- [Drasi Documentation](https://drasi.io)
- [Cypher Query Language](https://neo4j.com/developer/cypher/)

## Known Limitations

1. **Results API Bootstrap Issue**: The REST API `/queries/:id/results` currently doesn't return bootstrapped data due to how DrasiLib processes bootstrap events separately from regular changes. The Log Reaction will show all data correctly. This is being addressed.

2. **Single Table**: This example demonstrates a single table. For multi-table queries with JOIN relationships, see the Drasi documentation on relationship mapping.

3. **Schema Changes**: If the table schema changes, the source needs to be restarted to pick up new columns.
