# Sui DeepBook Getting Started

This example runs a real-time **DeFi dashboard** for Sui DeepBook V3, powered by Drasi continuous queries and delivered to the browser via Server-Sent Events (SSE).

Three categories of Drasi queries feed the dashboard:

**Core queries** — simple projections:

| Query | Purpose |
|-------|---------|
| `event-feed` | Every DeepBook event with full payload |
| `pool-tracker` | Enrichment `Pool` nodes with base/quote assets |
| `trader-tracker` | Enrichment `Trader` nodes from event senders |

**Aggregation queries** — server-side continuous aggregations:

| Query | Purpose | Drasi features used |
|-------|---------|---------------------|
| `event-breakdown` | Event count by type | `count()` aggregation with grouping |
| `bid-ask-ratio` | Bids vs asks per pool | `CASE WHEN` + `sum()` |
| `fill-tracker` | Fill count per pool | Filtered `count()` with `WHERE` |

## Prerequisites

- Rust toolchain (workspace default)
- Internet access to a Sui RPC endpoint
- A modern web browser
- `curl` (for helper scripts)

## Quick Start

```bash
cd examples/lib/sui-deepbook-getting-started
./quickstart.sh
```

Or step-by-step:

```bash
# 1. Check RPC connectivity
./setup.sh

# 2. Build and run
cargo run
```

Then open **http://localhost:3000** in your browser.

## What You'll See

The terminal shows the server endpoints:

```
╔══════════════════════════════════════════════════════════════╗
║  DeepBook DeFi Dashboard                                   ║
╠══════════════════════════════════════════════════════════════╣
║  RPC:       https://fullnode.mainnet.sui.io:443
║  Package:   0x337f4f…ef497
║  SSE:       http://localhost:8080/events
║  Dashboard: http://localhost:3000
║  Enrichment: Pool + Trader + Order nodes enabled
╠══════════════════════════════════════════════════════════════╣
║  Open the dashboard URL in your browser.                   ║
║  Press Ctrl+C to stop.                                     ║
╚══════════════════════════════════════════════════════════════╝
```

The browser dashboard shows:

- **Stats bar** — total events, active pools, unique traders, total fills, throughput
- **Price ticker** — scrolling tape of latest fill prices per pool
- **Live Event Feed** — scrolling table of every DeepBook event with type, side (BID/ASK), pool, sender, price, and quantity
- **Pool Overview** — cards showing base/quote pair, event count, fill count, last fill price, and bid/ask ratio bar
- **Event Distribution** — donut chart breaking down events by type, powered by a server-side `count()` aggregation query
- **Top Traders** — leaderboard of most active traders by event count
- **Whale Alerts** — highlighted large trades above the quantity threshold

All data updates in real time via SSE — no page reloads needed.

## Architecture

```
Sui RPC ──► DeepBook Source ──► Drasi Queries ──► SSE Reaction ──► Browser Dashboard
              (poll 2s)          (continuous)       (port 8080)      (port 3000)
```

1. **Source** polls `suix_queryEvents` every 2 seconds and emits graph nodes
2. **Enrichment** adds `Pool`, `Trader`, and `Order` nodes linked to events
3. **Three Cypher queries** run continuously, emitting diffs (ADD/UPDATE/DELETE)
4. **SSE Reaction** streams query results to connected browsers at `/events`
5. **Dashboard server** serves the HTML dashboard at port 3000
6. **Browser JS** connects to SSE, dispatches by `queryId`, updates the UI

## Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `SUI_RPC_URL` | `https://fullnode.mainnet.sui.io:443` | Sui JSON-RPC endpoint |
| `DEEPBOOK_PACKAGE_ID` | Mainnet DeepBook V3 address | Package ID for filtering |
| `SSE_PORT` | `8080` | Port for the SSE event stream |
| `DASHBOARD_PORT` | `3000` | Port for the HTML dashboard |

### Using a Different Network

```bash
# Testnet
SUI_RPC_URL=https://fullnode.testnet.sui.io:443 \
DEEPBOOK_PACKAGE_ID=0x<testnet-package-id> \
cargo run

# Custom provider (higher rate limits)
SUI_RPC_URL=https://sui-mainnet.rpc.example.com cargo run
```

## The Cypher Queries

### Event Feed — all events in real time

```cypher
MATCH (e:DeepBookEvent)
RETURN
  e.entity_id    AS id,
  e.event_name   AS event_name,
  e.pool_id      AS pool_id,
  e.timestamp_ms AS timestamp,
  e.sender       AS sender,
  e.payload      AS payload
```

### Pool Tracker — discovered pools with asset info

```cypher
MATCH (p:Pool)
RETURN
  p.pool_id     AS pool_id,
  p.base_asset  AS base_asset,
  p.quote_asset AS quote_asset
```

### Trader Tracker — unique trader addresses

```cypher
MATCH (t:Trader)
RETURN t.address AS address
```

### Event Breakdown — server-side count by event type

```cypher
MATCH (e:DeepBookEvent)
RETURN e.event_name AS event_name, count(e) AS cnt
```

This aggregation query groups events by type and emits real-time count updates.
Each time a new event arrives, the count for that type increments and the dashboard
donut chart updates automatically.

### Bid/Ask Ratio — CASE WHEN aggregation per pool

```cypher
MATCH (e:DeepBookEvent)
WHERE e.payload.is_bid IS NOT NULL
RETURN e.pool_id AS pool_id,
  sum(CASE WHEN e.payload.is_bid = true THEN 1 ELSE 0 END) AS bids,
  sum(CASE WHEN e.payload.is_bid = false THEN 1 ELSE 0 END) AS asks
```

Showcases Drasi's support for `CASE WHEN` expressions inside aggregations.
The dashboard renders this as a bid/ask ratio bar on each pool card.

### Fill Tracker — filtered fill count per pool

```cypher
MATCH (e:DeepBookEvent)
WHERE e.event_name = 'OrderFilled'
RETURN e.pool_id AS pool_id, count(e) AS fill_count
```

## Helper Scripts

| Script | Purpose |
|--------|---------|
| `setup.sh` | Validates RPC endpoint health with a 60-second timeout |
| `quickstart.sh` | Runs setup → build → start in one command |
| `diagnose.sh` | Prints chain ID and scans recent events by package ID |
| `test-updates.sh` | Scans recent events and prints matching DeepBook events |

## Troubleshooting

### Dashboard shows "Connecting…" / "Reconnecting…"

- Verify the Rust process is running and printed the SSE endpoint URL
- Check that port 8080 is not blocked or in use
- Try opening `http://localhost:8080/events` directly — you should see SSE heartbeats

### No events appearing

DeepBook events are relatively infrequent. With `StartPosition::Now` (default), only events emitted **after** startup are captured. Wait a few minutes for market activity, or switch to `StartPosition::Beginning` in the code to replay history.

### RPC errors / timeouts

- Verify: `curl -s https://fullnode.mainnet.sui.io:443 -X POST -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"sui_getChainIdentifier","id":1,"params":[]}'`
- The public endpoint has rate limits — use a dedicated provider via `SUI_RPC_URL`
- Run `./diagnose.sh` for full diagnostics

### "DeepBook package changed"

If Mysten Labs deploys a new DeepBook package, override `DEEPBOOK_PACKAGE_ID` with the updated address.
