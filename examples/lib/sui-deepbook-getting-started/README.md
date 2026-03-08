# Sui DeepBook Getting Started

This example connects to the Sui mainnet JSON-RPC and streams live DeepBook V3 events through a Drasi continuous query. Every event is logged to the console via a `LogReaction` showing the entity ID, event type, and operation.

## Prerequisites

- Rust toolchain (workspace default)
- Internet access to a Sui RPC endpoint
- `curl` and `jq` (for helper scripts)

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

## What You'll See

Once running, the console shows a styled live feed of DeepBook events with colour-coded actions and extracted trading data:

```
╔══════════════════════════════════════════════════════════════╗
║  Sui DeepBook Live Event Monitor                             ║
╠══════════════════════════════════════════════════════════════╣
║  RPC:     https://fullnode.mainnet.sui.io:443
║  Package: 0x337f4f…ef497
║  Mode:    Live streaming (start from now)
╠══════════════════════════════════════════════════════════════╣
║  Waiting for DeepBook events… (Ctrl+C to stop)
╚══════════════════════════════════════════════════════════════╝

  ▶ ADD  PriceAdded (deep_price)
        entity: pool:0xab…abcd  sender: 0x1a…2b3c  time: 14:32:08
        pool: 0xab…abcd  price=2850000000  size=1500000

  ▶ ADD  BalanceEvent (balance_manager)
        entity: event:0x7fa…:0  sender: 0x9d…ef01  time: 14:32:08
        amount=500000000  balance=12000000000

  ▶ ADD  OrderPlaced (events)
        entity: order:42  sender: 0x1a…2b3c  time: 14:32:09
        pool: 0xab…abcd  price=2340  size=1000  side=BID

  ⟳ UPD  OrderFilled (events)
        entity: order:42  sender: 0x1a…2b3c  time: 14:32:11
        pool: 0xab…abcd  price=2340  size=800  fee=120
        transition: insert → update

  ✕ DEL  OrderCancelled (events)
        entity: order:42  sender: 0x1a…2b3c  time: 14:32:15
```

### Reading the Output

**Action icons** indicate what happened:

| Icon | Colour | Meaning |
|------|--------|---------|
| `▶ ADD` | Green | New event appeared (order placed, price update, balance change) |
| `⟳ UPD` | Yellow | An existing entity was modified (order filled, amended) |
| `✕ DEL` | Red | An entity was removed (order cancelled, position closed) |

**Event details** are broken into lines:

| Line | Content |
|------|---------|
| 1st | **Event name** and Move module (e.g. `PriceAdded (deep_price)`) |
| 2nd | Entity ID, sender (truncated), and on-chain timestamp |
| 3rd | Pool (truncated) + payload highlights (price, size, amount, side, fees) |

**Payload highlights** are automatically extracted from common DeepBook fields:

| Field | Display | Example |
|-------|---------|---------|
| `price` | `price=…` | `price=2340` |
| `size` / `quantity` | `size=…` / `qty=…` | `size=1000` |
| `amount` / `balance` | `amount=…` / `balance=…` | `amount=500000000` |
| `fee` / `maker_fee` / `taker_fee` | `fee=…` | `fee=120` |
| `is_bid` / `side` | `side=BID` or `side=ASK` | `side=BID` |

## Configuration

Environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `SUI_RPC_URL` | `https://fullnode.mainnet.sui.io:443` | Sui JSON-RPC endpoint |
| `DEEPBOOK_PACKAGE_ID` | Mainnet DeepBook V3 address | Package ID for filtering |
| `MAX_PAGES` | `100` | Pages to scan in `diagnose.sh` / `test-updates.sh` |

### Using a Different Network

```bash
# Testnet
SUI_RPC_URL=https://fullnode.testnet.sui.io:443 \
DEEPBOOK_PACKAGE_ID=0x<testnet-package-id> \
cargo run

# Custom provider (higher rate limits)
SUI_RPC_URL=https://sui-mainnet.rpc.example.com \
cargo run
```

## The Cypher Query

The example runs this continuous query:

```cypher
MATCH (e:DeepBookEvent)
RETURN
  e.entity_id     AS entity_id,
  e.event_name    AS event_name,
  e.module        AS module,
  e.change_type   AS change_type,
  e.pool_id_short AS pool,
  e.sender_short  AS sender,
  e.timestamp_ms  AS timestamp_ms,
  e.order_id      AS order_id,
  e.payload       AS payload
```

The example uses `ApplicationReaction` with a custom formatting loop (not `LogReaction`) to render structured, colour-coded output. You can modify `main.rs` to try more targeted queries. For example, to only track orders in a specific pool:

```cypher
MATCH (e:DeepBookEvent)
WHERE e.pool_id = '0xabc123…' AND e.order_id IS NOT NULL
RETURN e.order_id, e.event_name, e.change_type, e.sender_short
```

## Helper Scripts

| Script | Purpose |
|--------|---------|
| `setup.sh` | Validates RPC endpoint health with a 60-second timeout |
| `quickstart.sh` | Runs setup → build → start in one command |
| `diagnose.sh` | Prints chain ID and scans recent events paginated by package ID |
| `test-updates.sh` | Scans recent events and prints matching DeepBook events as JSON |

### Diagnosing Connectivity

```bash
./diagnose.sh
```

Output:
```
--- Chain Info ---
{"jsonrpc":"2.0","result":"35834a8a"}
--- Scanning events (up to 100 pages) ---
  Page 1: 50 events returned
  Page 2: 50 events returned
  ...
  Page 47: 50 events returned — FOUND 3 matching events!
    Type: 0x337…::deep_price::PriceAdded
    Type: 0x337…::balance_manager::BalanceEvent
    Type: 0x337…::balance_manager::BalanceEvent
```

## How It Works

1. **Source** polls `suix_queryEvents` every 2 seconds
2. Raw events are filtered to only those from the DeepBook package
3. Each matching event becomes a `DeepBookEvent` node in the Drasi graph
4. The Cypher query runs continuously and emits diffs (ADD/UPDATE/DELETE)
5. The `LogReaction` formats each diff using Handlebars templates and prints to stdout

## Troubleshooting

### No output / "No events found"

DeepBook events are relatively infrequent. The `StartPosition::Now` setting (default in this example) means the source only captures events emitted **after** it starts. Wait a few minutes, or switch to `StartPosition::Beginning` to replay historical events:

```rust
.with_start_from_beginning()
```

### RPC errors / timeouts

- Verify the endpoint is reachable: `curl -s https://fullnode.mainnet.sui.io:443 -X POST -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"sui_getChainIdentifier","id":1,"params":[]}'`
- The public mainnet endpoint has rate limits. Switch to a dedicated provider via `SUI_RPC_URL`.
- Run `./diagnose.sh` for a full connectivity check.

### "DeepBook package changed"

If Mysten Labs deploys a new DeepBook package, override `DEEPBOOK_PACKAGE_ID` with the updated address.
