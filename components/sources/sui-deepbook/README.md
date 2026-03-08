# Sui DeepBook V3 Source

## Overview

The Sui DeepBook Source is a Change Data Capture (CDC) plugin for Drasi that streams events from [DeepBook V3](https://deepbook.tech/) — the native central-limit order book (CLOB) on the [Sui blockchain](https://sui.io/). It polls the Sui JSON-RPC endpoint (`suix_queryEvents`) for on-chain events emitted by the DeepBook package and transforms them into Drasi `SourceChange` events for continuous query processing.

**Key Capabilities**:
- Real-time event streaming via Sui JSON-RPC polling
- Automatic cursor persistence and resume via `StateStoreProvider`
- Client-side filtering by event type and pool ID
- Configurable start position (beginning of chain, current tip, or specific timestamp)
- Automatic retry with configurable back-off on transient RPC errors
- Full event payload projection into queryable properties

**Use Cases**:
- Real-time order book monitoring (new orders, fills, cancellations)
- Liquidity analytics across DeepBook pools
- Trading signal generation from on-chain activity
- DeFi dashboards tracking pool health and volume
- Alerting on large trades or unusual order patterns
- Historical event replay for back-testing strategies

## Architecture

### Components

The source consists of four modules:

1. **Config** (`config.rs`): Builder-pattern configuration with validation — RPC endpoint, package ID, poll interval, request limits, filters, and start position
2. **RPC** (`rpc.rs`): HTTP JSON-RPC client wrapping `suix_queryEvents` with cursor-based pagination, timeout, and error handling
3. **Mapping** (`mapping.rs`): Transforms raw Sui events into Drasi `SourceChange` records with entity ID derivation, operation classification, label assignment, and property projection
4. **Descriptor** (`descriptor.rs`): Plugin descriptor for dynamic plugin loading via the Drasi plugin SDK

**Note**: Bootstrap functionality is provided by the separate `drasi-bootstrap-sui-deepbook` crate via the pluggable bootstrap provider pattern.

### Data Flow

```
Sui JSON-RPC  →  SuiRpcClient  →  Package-ID Filter  →  Event/Pool Filter  →  Mapping  →  SourceChange
(suix_queryEvents)    ↓                                                                         ↓
                  Cursor Mgmt                                                             Dispatcher → Queries
                      ↓
                  StateStore
```

### How Events Become Graph Nodes

> **📖 Full schema reference**: See [GRAPH_SCHEMA.md](GRAPH_SCHEMA.md) for the complete graph schema including all properties, entity ID rules, operation classification, known event types, type mappings, and query examples.

Each DeepBook event becomes a `Node` element in the Drasi graph with:

| Property | Source | Example |
|----------|--------|---------|
| `entity_id` | Derived from `order_id`, `pool_id`, or `tx:seq` | `order:42`, `pool:0xabc` |
| `event_type` | Full Move type path | `0x337…::events::OrderPlaced` |
| `event_name` | Short name from the type path | `OrderPlaced` |
| `module` | Move module that emitted the event | `deep_price`, `balance_manager` |
| `change_type` | Classified from event name | `insert`, `update`, `delete` |
| `tx_digest` | Transaction that emitted the event | `0x7fa2…` |
| `event_seq` | Sequence within the transaction | `0` |
| `package_id` | Originating package address | `0x337f…` |
| `sender` | Transaction sender address (full) | `0x1a2b3c…` |
| `sender_short` | Truncated sender for display | `0x1a2b…3c4d` |
| `timestamp_ms` | On-chain timestamp (milliseconds) | `1772923888171` |
| `order_id` | Extracted when present | `42` |
| `pool_id` | Pool address (full, when present) | `0xabc123…` |
| `pool_id_short` | Truncated pool address for display | `0xabc1…2345` |
| `payload` | Full `parsedJson` as nested object | `{size: "1000", …}` |
| `payload` | Nested object with all event fields | `e.payload.size`, `e.payload.price` |

Every node receives two labels:
- `DeepBookEvent` (constant, useful for broad queries)
- A secondary label derived from the event name, e.g. `OrderPlaced`, `BalanceEvent`

### Operation Classification

Events are classified based on keywords in their type name:

| Keyword(s) | Operation | SourceChange |
|------------|-----------|--------------|
| `cancel`, `delete`, `remove` | Delete | `SourceChange::Delete` |
| `fill`, `update`, `modify`, `amend` | Update | `SourceChange::Update` |
| Everything else | Insert | `SourceChange::Insert` |

## Prerequisites

- **Sui RPC Access**: A Sui full-node JSON-RPC endpoint. The default Sui mainnet endpoint (`https://fullnode.mainnet.sui.io:443`) works but may have rate limits. For production use, consider a dedicated provider (Shinami, BlastAPI, etc.).
- **Network Connectivity**: Outbound HTTPS to the RPC endpoint.
- **DeepBook Package ID**: The address of the DeepBook V3 package. The default targets Sui mainnet.

## Configuration

### Builder Pattern (Recommended)

```rust
use drasi_source_sui_deepbook::SuiDeepBookSource;

let source = SuiDeepBookSource::builder("deepbook-source")
    .with_rpc_endpoint("https://fullnode.mainnet.sui.io:443")
    .with_deepbook_package_id("0x337f4f4f6567fcd778d5454f27c16c70e2f274cc6377ea6249ddf491482ef497")
    .with_poll_interval_ms(2_000)
    .with_request_limit(50)
    .with_start_from_now()
    .build()?;
```

### Configuration Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `rpc_endpoint` | `String` | `https://fullnode.mainnet.sui.io:443` | Sui JSON-RPC endpoint URL |
| `deepbook_package_id` | `String` | Mainnet DeepBook V3 address | Package ID to filter events by |
| `poll_interval_ms` | `u64` | `2000` | Milliseconds between poll cycles |
| `request_limit` | `u16` | `100` | Max events per RPC page (1–1000) |
| `event_filters` | `Vec<String>` | `[]` | Event type substrings to include (empty = all) |
| `pools` | `Vec<String>` | `[]` | Pool IDs to include (empty = all) |
| `start_position` | `StartPosition` | `Now` | Where to begin consuming events |

### Start Position

| Variant | Behaviour |
|---------|-----------|
| `StartPosition::Now` | Fetch the latest cursor and only process new events going forward |
| `StartPosition::Beginning` | Start from the earliest available event on chain |
| `StartPosition::Timestamp(ms)` | Start from the beginning but skip events with `timestamp_ms < ms` |

## Query Examples

### 1. Stream All DeepBook Events

```cypher
MATCH (e:DeepBookEvent)
RETURN e.entity_id, e.event_type, e.change_type, e.timestamp_ms
```

### 2. Monitor a Specific Pool

```cypher
MATCH (e:DeepBookEvent)
WHERE e.pool_id = '0xabc123…'
RETURN e.entity_id, e.event_type, e.payload.size, e.payload.price
```

### 3. Track Order Lifecycle

```cypher
MATCH (e:DeepBookEvent)
WHERE e.order_id IS NOT NULL
RETURN e.order_id, e.change_type, e.event_type, e.timestamp_ms
```

This query surfaces `insert` → `update` → `delete` transitions as orders are placed, filled, and cancelled.

### 4. Large-Trade Alert

```cypher
MATCH (e:OrderPlaced)
WHERE e.payload.size IS NOT NULL
RETURN e.entity_id, e.pool_id, e.payload.size, e.payload.price, e.sender
```

Pair with a reaction (e.g. webhook, Slack) to alert when new orders appear.

### 5. Filter to Specific Event Types (Builder-Side)

If you only care about order placement and cancellation, configure the source to filter before the query:

```rust
let source = SuiDeepBookSource::builder("deepbook-orders")
    .with_event_filters(vec![
        "OrderPlaced".into(),
        "OrderCancelled".into(),
    ])
    .with_start_from_beginning()
    .build()?;
```

Then query:

```cypher
MATCH (e:DeepBookEvent)
RETURN e.entity_id, e.event_type, e.change_type
```

### 6. Multi-Pool Monitoring

```rust
let source = SuiDeepBookSource::builder("multi-pool")
    .with_pools(vec![
        "0xpool_sui_usdc".into(),
        "0xpool_deep_sui".into(),
    ])
    .build()?;
```

```cypher
MATCH (e:DeepBookEvent)
RETURN e.pool_id, e.event_type, count(e) AS event_count
```

### 7. Using Bootstrap for Historical Replay

Load the last 50 pages of historical events before switching to live streaming:

```rust
use drasi_bootstrap_sui_deepbook::SuiDeepBookBootstrapProvider;

let bootstrap = SuiDeepBookBootstrapProvider::builder()
    .with_max_pages(50)
    .with_start_from_beginning()
    .build()?;

let source = SuiDeepBookSource::builder("deepbook-with-history")
    .with_bootstrap_provider(bootstrap)
    .with_start_from_beginning()
    .build()?;
```

## State Management

When a `StateStoreProvider` is injected (automatically by `DrasiLib`), the source persists the Sui event cursor under the key `cursor` in its state partition. On restart, polling resumes exactly where it left off — no events are lost or duplicated.

Without a state store, the source falls back to the configured `start_position` on each startup.

## Event Filtering Pipeline

Filtering happens in three stages — broadest first:

1. **Package ID** — only events from the configured `deepbook_package_id` are kept
2. **Event type** — if `event_filters` is non-empty, the event's type name must contain or end with one of the filter strings (case-insensitive)
3. **Pool ID** — if `pools` is non-empty, the event must contain a `pool_id`/`poolId`/`pool` field matching one of the configured IDs

## Error Handling

- Transient RPC failures are retried with the configured `poll_interval_ms` as back-off
- After 10 consecutive failures the source transitions to `Error` status
- Corrupted persisted cursors are automatically cleared and the source restarts from the configured `start_position`

## Testing

```bash
# Unit tests
cargo test -p drasi-source-sui-deepbook

# Integration test (mock RPC, verifies INSERT/UPDATE/DELETE + package-ID filtering)
cargo test -p drasi-source-sui-deepbook --test integration_test -- --ignored --nocapture
```

## Limitations

- **Polling, not push**: The Sui JSON-RPC does not support server-push subscriptions for `suix_queryEvents`. The source polls at the configured interval.
- **All-events query**: Sui full-nodes do not support the `{"Package": "0x…"}` event query filter. The source queries `{"All": []}` and filters client-side by package ID. This is bandwidth-inefficient on very busy networks; using a dedicated RPC provider helps.
- **No relationship edges**: All events are modelled as independent nodes. If you need edges, use a query-time join on shared keys like `order_id` or `pool_id`.
