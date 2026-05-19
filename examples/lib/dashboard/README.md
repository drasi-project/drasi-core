# DrasiLib Dashboard Example

A real-time stock price dashboard built with drasi-lib. This example demonstrates the
**HTTP Source → Multiple Queries → Dashboard Reaction** pattern, where live data flows
from an HTTP endpoint through continuous Cypher queries into a visual web dashboard.

## Architecture

```
┌─────────────────┐     ┌──────────────────┐
│  Bootstrap File │────▶│                  │
│ (8 stocks)      │     │                  │
└─────────────────┘     │   HTTP Source    │────┐
                        │  (port 9000)     │    │
┌─────────────────┐     │                  │    │
│  HTTP Events    │────▶│                  │    │
│ (price updates) │     └──────────────────┘    │
└─────────────────┘                             │
                                                ▼
                        ┌──────────────────────────────────────┐
                        │             4 Queries                │
                        ├──────────────────────────────────────┤
                        │ 1. all-prices  — All stock prices    │
                        │ 2. gainers     — Stocks with gains   │
                        │ 3. high-volume — Volume > 1M         │
                        │ 4. top-movers  — Biggest changes %   │
                        └──────────────────────────────────────┘
                                                │
                                                ▼
                        ┌──────────────────────────────────────┐
                        │       Dashboard Reaction             │
                        │    (web UI on port 3000)             │
                        │                                      │
                        │  • Drag-and-drop widget designer     │
                        │  • Real-time WebSocket data          │
                        │  • Charts, tables, gauges, KPIs      │
                        └──────────────────────────────────────┘
```

## Running

```bash
# Using the run script
./run.sh

# Or directly with cargo
cargo run

# With debug logging
RUST_LOG=debug cargo run
```

Then open **http://localhost:3000** in your browser.

## Quick Start

1. Start the example: `cargo run`
2. Open http://localhost:3000 in your browser
3. Click **"New Dashboard"** to create a dashboard
4. Click **"Add Widget"** and choose a widget type:
   - **Table** → select the `all-prices` query, columns: `symbol`, `price`, `volume`
   - **Bar Chart** → select the `gainers` query, category: `symbol`, value: `gain_percent`
   - **Gauge** → select the `top-movers` query, value: `change_percent`
   - **KPI** → select the `high-volume` query, value: `volume`
5. Drag and resize widgets on the grid
6. Use `change.http` or curl to send price updates and watch widgets update in real time

## Initial Data

The example bootstraps with 8 stocks:

| Symbol | Price   | Previous Close | Volume    | Status  |
|--------|---------|----------------|-----------|---------|
| AAPL   | 175.50  | 173.00         | 5,000,000 | Gainer  |
| MSFT   | 380.25  | 382.00         | 2,500,000 | Loser   |
| GOOGL  | 142.80  | 140.50         | 1,200,000 | Gainer  |
| TSLA   | 245.00  | 250.00         | 8,000,000 | Loser   |
| NVDA   | 520.75  | 515.00         | 3,500,000 | Gainer  |
| AMZN   | 178.25  | 175.00         | 4,200,000 | Gainer  |
| META   | 485.00  | 480.00         | 6,800,000 | Gainer  |
| NFLX   | 620.50  | 618.00         | 1,800,000 | Gainer  |

## Queries

### all-prices
All stock prices — ideal for a **Table** widget.

```cypher
MATCH (sp:stock_prices)
RETURN sp.symbol AS symbol,
       sp.price AS price,
       sp.previous_close AS previous_close,
       sp.volume AS volume
```

### gainers
Stocks beating their previous close — ideal for a **Bar Chart** widget showing gain percentages.

```cypher
MATCH (sp:stock_prices)
WHERE sp.price > sp.previous_close
RETURN sp.symbol AS symbol,
       sp.price AS price,
       sp.previous_close AS previous_close,
       ((sp.price - sp.previous_close) / sp.previous_close * 100) AS gain_percent
```

### high-volume
Stocks with volume over 1 million — ideal for a **Table** or **KPI** widget.

```cypher
MATCH (sp:stock_prices)
WHERE sp.volume > 1000000
RETURN sp.symbol AS symbol,
       sp.price AS price,
       sp.volume AS volume
```

### top-movers
Biggest price changes as a percentage — ideal for a **Gauge** or **KPI** widget.

```cypher
MATCH (sp:stock_prices)
WHERE sp.previous_close > 0
RETURN sp.symbol AS symbol,
       sp.price AS price,
       sp.previous_close AS previous_close,
       abs((sp.price - sp.previous_close) / sp.previous_close * 100) AS change_percent
```

## Sending Updates

### Using curl

```bash
# Update AAPL price
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
        "price": 182.00,
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
      "id": "price_AMD",
      "labels": ["stock_prices"],
      "properties": {
        "symbol": "AMD",
        "price": 165.50,
        "previous_close": 160.00,
        "volume": 3200000
      }
    }
  }'
```

### Using VS Code REST Client

Open `change.http` with the REST Client extension and click "Send Request".

## HTTP Endpoints

| Port | Method | Endpoint | Description |
|------|--------|----------|-------------|
| 3000 | GET | `/` | Dashboard web UI |
| 3000 | WS | `/ws` | Real-time data (WebSocket) |
| 3000 | GET | `/api/dashboards` | List saved dashboards |
| 3000 | GET | `/api/queries` | List available query IDs |
| 9000 | POST | `/sources/stock-prices/events` | Send single event |
| 9000 | POST | `/sources/stock-prices/events/batch` | Send batch events |
| 9000 | GET | `/health` | Source health check |
| 8080 | GET | `/queries/:id/results` | Query results (debug) |

## Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Crate manifest (standalone workspace) |
| `main.rs` | Example source code |
| `bootstrap_data.jsonl` | Initial stock data (8 stocks) |
| `change.http` | REST client file for sending events |
| `run.sh` | Shell script to run the example |
| `README.md` | This file |

## Dependencies

- `drasi-lib` — Core library
- `drasi-source-http` — HTTP source component
- `drasi-reaction-dashboard` — Dashboard reaction component
- `drasi-bootstrap-scriptfile` — Script file bootstrap provider
