# DrasiLib Stock Monitor Example

This is a standalone example crate demonstrating programmatic use of drasi-lib to build a real-time stock price monitoring system.

## Overview

The example shows how to:

- Create an HTTP source to receive price updates
- Use ScriptFile bootstrap provider for initial data
- Define multiple Cypher queries for continuous monitoring
- Attach a LogReaction to print query results directly to the console
- Expose a simple Results API to query current state

## Architecture

```
┌─────────────────┐     ┌──────────────────┐
│  Bootstrap File │────▶│                  │
│ (initial stocks)│     │                  │
└─────────────────┘     │   HTTP Source    │────┐
                        │  (port 9000)     │    │
┌─────────────────┐     │                  │    │
│  HTTP Events    │────▶│                  │    │
│ (price updates) │     └──────────────────┘    │
└─────────────────┘                             │
                                                ▼
                        ┌──────────────────────────────────────┐
                        │            3 Queries                 │
                        ├──────────────────────────────────────┤
                        │ 1. all-prices: All stock prices      │
                        │ 2. gainers: Stocks with price > prev │
                        │ 3. high-volume: Volume > 1M          │
                        └──────────────────────────────────────┘
                                                │
                                                ▼
                        ┌──────────────────────────────────────┐
                        │          Log Reaction                │
                        │    (prints to console)               │
                        └──────────────────────────────────────┘
```

## Running

This is a standalone crate with its own `Cargo.toml`. Run it from this directory:

```bash
# Using the run script
./run.sh

# Or directly with cargo
cargo run

# With debug logging
RUST_LOG=debug cargo run
```

The first build may take a few minutes as it compiles dependencies (including RocksDB).

## Initial Data

The example bootstraps with 5 stocks from `bootstrap_data.jsonl`:

| Symbol | Price   | Previous Close | Volume    |
|--------|---------|----------------|-----------|
| AAPL   | 175.50  | 173.00         | 5,000,000 |
| MSFT   | 380.25  | 382.00         | 2,500,000 |
| GOOGL  | 142.80  | 140.50         | 1,200,000 |
| TSLA   | 245.00  | 250.00         | 8,000,000 |
| NVDA   | 520.75  | 515.00         | 3,500,000 |

## Queries

### all-prices
Returns all stock prices with their details.

```cypher
MATCH (sp:stock_prices)
RETURN sp.symbol AS symbol,
       sp.price AS price,
       sp.previous_close AS previous_close,
       sp.volume AS volume
```

### gainers
Stocks where current price exceeds previous close.

```cypher
MATCH (sp:stock_prices)
WHERE sp.price > sp.previous_close
RETURN sp.symbol AS symbol,
       sp.price AS price,
       sp.previous_close AS previous_close,
       ((sp.price - sp.previous_close) / sp.previous_close * 100) AS gain_percent
```

### high-volume
Stocks with trading volume over 1 million.

```cypher
MATCH (sp:stock_prices)
WHERE sp.volume > 1000000
RETURN sp.symbol AS symbol,
       sp.price AS price,
       sp.volume AS volume
```

## Testing

### Using curl

```bash
# Update AAPL price (triggers gainers query)
curl -X POST http://localhost:9000/sources/stock-prices/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "update",
    "element": {
      "type": "node",
      "id": "price_AAPL",
      "labels": ["stock_prices"],
      "properties": {
        "symbol": "AAPL",
        "price": 180.00,
        "previous_close": 173.00,
        "volume": 7500000
      }
    }
  }'

# Insert new stock
curl -X POST http://localhost:9000/sources/stock-prices/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "insert",
    "element": {
      "type": "node",
      "id": "price_AMZN",
      "labels": ["stock_prices"],
      "properties": {
        "symbol": "AMZN",
        "price": 178.25,
        "previous_close": 175.00,
        "volume": 4200000
      }
    }
  }'

# Delete a stock
curl -X POST http://localhost:9000/sources/stock-prices/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "delete",
    "id": "price_TSLA",
    "labels": ["stock_prices"]
  }'
```

### Using VS Code REST Client

Open `change.http` with the REST Client extension and click "Send Request" on any request.

### Viewing Results

Query results can be retrieved via the Results API on port 8080:

```bash
# Get all stock prices
curl http://localhost:8080/queries/all-prices/results

# Get gainers
curl http://localhost:8080/queries/gainers/results

# Get high volume stocks
curl http://localhost:8080/queries/high-volume/results
```

Results are also printed to the console by the Log reaction when changes occur.

See `results.http` for pre-built REST client requests.

## HTTP Endpoints

### HTTP Source (port 9000)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | /health | Health check |
| POST | /sources/stock-prices/events | Single event |
| POST | /sources/stock-prices/events/batch | Multiple events |

### Results API (port 8080)

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | /queries/all-prices/results | Get all stock prices |
| GET | /queries/gainers/results | Get stocks with gains |
| GET | /queries/high-volume/results | Get high volume stocks |

## Event Format

### Insert/Update

```json
{
  "operation": "insert",
  "element": {
    "type": "node",
    "id": "unique_id",
    "labels": ["stock_prices"],
    "properties": {
      "symbol": "TICKER",
      "price": 100.00,
      "previous_close": 99.00,
      "volume": 1000000
    }
  }
}
```

### Delete

```json
{
  "operation": "delete",
  "id": "unique_id",
  "labels": ["stock_prices"]
}
```

## Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Crate manifest (standalone workspace) |
| `main.rs` | Example source code |
| `bootstrap_data.jsonl` | Initial stock data |
| `change.http` | REST client file for sending events |
| `results.http` | REST client file for viewing query results |
| `run.sh` | Shell script to run the example |

## Dependencies

This crate depends on drasi-lib components via relative paths:

- `drasi-lib` - Core library
- `drasi-source-http` - HTTP source component
- `drasi-reaction-log` - Log reaction component
- `drasi-bootstrap-scriptfile` - Script file bootstrap provider
