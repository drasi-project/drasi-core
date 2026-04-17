# Sui DeepBook V3 Source

## Overview

The Sui DeepBook Source is a Change Data Capture (CDC) plugin for Drasi that streams events from [DeepBook V3](https://deepbook.tech/) — the native central-limit order book (CLOB) on the [Sui blockchain](https://sui.io/). It supports two transport modes:

- **gRPC checkpoint streaming** (default, recommended): Push-based real-time streaming via the Sui gRPC API with sub-second latency
- **JSON-RPC polling** (legacy fallback): Pull-based polling via `suix_queryEvents` — **deprecated**, scheduled for deactivation July 2026

Events are transformed into Drasi `SourceChange` events for continuous query processing.

**Key Capabilities**:
- Real-time event streaming via gRPC checkpoint streaming (push-based, ~300ms latency)
- Legacy JSON-RPC polling fallback with configurable interval
- BCS deserialization of known DeepBook V3 event types (PriceAdded, OrderFilled, FlashLoanBorrowed, etc.)
- Automatic cursor persistence and resume via `StateStoreProvider`
- Client-side filtering by event type and pool ID
- Configurable start position (beginning of chain, current tip, or specific timestamp)
- Automatic reconnection with exponential backoff on transport errors
- Full event payload projection into queryable properties
- **Graph enrichment**: Pool, Trader, and Order nodes linked to events via relationships

**Use Cases**:
- Real-time order book monitoring (new orders, fills, cancellations)
- Liquidity analytics across DeepBook pools
- Trading signal generation from on-chain activity
- DeFi dashboards tracking pool health and volume
- Alerting on large trades or unusual order patterns
- Historical event replay for back-testing strategies

## Architecture

### Components

The source consists of five modules:

1. **Config** (`config.rs`): Builder-pattern configuration with validation — transport mode, RPC endpoint, package ID, poll interval, request limits, filters, and start position
2. **gRPC** (`grpc.rs`): gRPC client wrapping `sui-rpc::Client` for checkpoint streaming, BCS deserialization of known DeepBook event types, and checkpoint-to-events extraction
3. **RPC** (`rpc.rs`): HTTP JSON-RPC client wrapping `suix_queryEvents` with cursor-based pagination, timeout, and error handling (legacy transport)
4. **Mapping** (`mapping.rs`): Transforms raw Sui events into Drasi `SourceChange` records with entity ID derivation, operation classification, label assignment, and property projection
5. **Descriptor** (`descriptor.rs`): Plugin descriptor for dynamic plugin loading via the Drasi plugin SDK

**Note**: Bootstrap functionality is provided by the separate `drasi-bootstrap-sui-deepbook` crate via the pluggable bootstrap provider pattern.

### Data Flow

**gRPC Transport (default)**:
```
Sui gRPC API  →  SuiGrpcClient  →  Package-ID Filter  →  BCS Deserialize  →  Event/Pool Filter  →  Mapping  →  SourceChange
(subscribe_checkpoints)  ↓                                                                                           ↓
                     Cursor Mgmt                                                                                Dispatcher → Queries
                     (checkpoint seq)                                                                                ↓
                         ↓                                                                                     Enrichment Nodes
                     StateStore                                                                              (Pool, Trader, Order)
```

**JSON-RPC Transport (legacy)**:
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

- **Sui RPC Access**: A Sui full-node endpoint with gRPC support (for default transport) or JSON-RPC (for legacy transport). The default Sui mainnet endpoint (`https://fullnode.mainnet.sui.io:443`) supports both protocols but may have rate limits. For production use, consider a dedicated provider (Shinami, BlastAPI, etc.).
- **Network Connectivity**: Outbound HTTPS/gRPC to the endpoint (port 443).
- **DeepBook Package ID**: The address of the DeepBook V3 package. The default targets Sui mainnet.

## Configuration

### Builder Pattern (Recommended)

```rust
use drasi_source_sui_deepbook::{SuiDeepBookSource, Transport};

// gRPC transport (default, recommended)
let source = SuiDeepBookSource::builder("deepbook-source")
    .with_rpc_endpoint("https://fullnode.mainnet.sui.io:443")
    .with_deepbook_package_id("0x337f4f4f6567fcd778d5454f27c16c70e2f274cc6377ea6249ddf491482ef497")
    .with_start_from_now()
    .build()?;

// JSON-RPC transport (legacy fallback)
let source = SuiDeepBookSource::builder("deepbook-source")
    .with_transport(Transport::JsonRpc)
    .with_poll_interval_ms(2_000)
    .with_request_limit(50)
    .with_start_from_now()
    .build()?;
```

### Configuration Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `transport` | `Transport` | `Grpc` | Transport mode: `Grpc` (recommended) or `JsonRpc` (legacy) |
| `rpc_endpoint` | `String` | `https://fullnode.mainnet.sui.io:443` | Sui endpoint URL (used for both gRPC and JSON-RPC) |
| `grpc_endpoint` | `Option<String>` | `None` | Override gRPC endpoint (falls back to `rpc_endpoint`) |
| `deepbook_package_id` | `String` | Mainnet DeepBook V3 address | Package ID to filter events by |
| `poll_interval_ms` | `u64` | `2000` | Milliseconds between poll cycles (JSON-RPC only) |
| `request_limit` | `u16` | `100` | Max events per RPC page, 1–1000 (JSON-RPC only) |
| `event_filters` | `Vec<String>` | `[]` | Event type substrings to include (empty = all) |
| `pools` | `Vec<String>` | `[]` | Pool IDs to include (empty = all) |
| `start_position` | `StartPosition` | `Now` | Where to begin consuming events |
| `enable_pool_nodes` | `bool` | `true` | Emit `:Pool` nodes with metadata from `sui_getObject` |
| `enable_trader_nodes` | `bool` | `true` | Emit `:Trader` nodes from event senders |
| `enable_order_nodes` | `bool` | `true` | Emit `:Order` nodes from event order_ids |
| `lookback_events` | `u16` | `0` | Recent historical events to fetch on startup (JSON-RPC only) |

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

### 7. Graph Traversal: Orders for a Trading Pair (enrichment)

```cypher
MATCH (e:OrderPlaced)-[:IN_POOL]->(p:Pool)
WHERE p.base_asset CONTAINS 'SUI' AND p.quote_asset CONTAINS 'USDC'
RETURN e.payload.price, e.payload.size, p.tick_size
```

### 8. Events by Trader (enrichment)

```cypher
MATCH (e:DeepBookEvent)-[:SENT_BY]->(t:Trader)
WHERE t.address = '0x1a2b3c…full_address…'
RETURN e.event_name, e.entity_id, e.timestamp_ms
```

### 9. Order Lifecycle via Graph Traversal (enrichment)

```cypher
MATCH (e:DeepBookEvent)-[:FOR_ORDER]->(o:Order)
WHERE o.order_id = '42'
RETURN e.event_name, e.change_type, e.timestamp_ms
```

### 10. Using Bootstrap for Historical Replay

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

When a `StateStoreProvider` is injected (automatically by `DrasiLib`), the source persists its cursor:
- **gRPC transport**: Persists the checkpoint sequence number under key `grpc_checkpoint_seq`
- **JSON-RPC transport**: Persists the Sui event cursor under key `cursor`

On restart, the source resumes exactly where it left off — no events are lost or duplicated. Without a state store, the source falls back to the configured `start_position` on each startup.

## Event Filtering Pipeline

Filtering happens in three stages — broadest first:

1. **Package ID** — only events from the configured `deepbook_package_id` are kept
2. **Event type** — if `event_filters` is non-empty, the event's type name must contain or end with one of the filter strings (case-insensitive)
3. **Pool ID** — if `pools` is non-empty, the event must contain a `pool_id`/`poolId`/`pool` field matching one of the configured IDs

## Error Handling

- **gRPC transport**: On stream disconnection or errors, reconnects with exponential backoff (1s → 2s → 4s → … up to 30s). After 20 consecutive failures, transitions to `Error` status.
- **JSON-RPC transport**: Transient RPC failures are retried with the configured `poll_interval_ms` as back-off. After 10 consecutive failures, transitions to `Error` status.
- Corrupted persisted cursors are automatically cleared and the source restarts from the configured `start_position`.

## Testing

```bash
# Unit tests (22 tests including gRPC event extraction)
cargo test -p drasi-source-sui-deepbook

# Integration test — JSON-RPC transport (requires mainnet connectivity)
cargo test -p drasi-source-sui-deepbook --test integration_test -- --ignored --nocapture test_sui_deepbook_change_detection_end_to_end

# Integration test — gRPC transport (requires mainnet connectivity)
cargo test -p drasi-source-sui-deepbook --test integration_test -- --ignored --nocapture test_grpc_transport_receives_events
```

## Limitations

- **BCS deserialization**: The gRPC transport uses BCS-encoded event data. Known DeepBook V3 event types (PriceAdded, OrderFilled, FlashLoanBorrowed, ReferralClaimed, OrderPlaced/Canceled/Modified, PoolCreated) are fully deserialized. Unknown event types are emitted with metadata but an empty payload.
- **JSON-RPC deprecation**: The Sui JSON-RPC API is scheduled for deactivation in July 2026. The `JsonRpc` transport is provided as a legacy fallback only.
- **Pool metadata via RPC**: When `enable_pool_nodes` is true, the source calls `sui_getObject` via JSON-RPC once per unique pool_id to fetch metadata (base_asset, quote_asset, etc.). This adds ~20-50 RPC calls on startup for DeepBook's active pools, then results are cached.
