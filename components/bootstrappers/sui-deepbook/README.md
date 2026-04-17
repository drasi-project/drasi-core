# Sui DeepBook Bootstrap Provider

## Overview

The Sui DeepBook Bootstrap Provider loads historical DeepBook V3 events from the Sui blockchain and feeds them into a Drasi query as an initial dataset before the streaming source takes over. It uses the same `suix_queryEvents` JSON-RPC endpoint as the source but iterates through pages of historical events rather than polling continuously.

**Key Capabilities**:
- Paginated historical event loading via Sui JSON-RPC
- Shared filtering logic with the streaming source (package ID, event type, pool ID)
- Configurable page limits to control bootstrap duration
- Same start-position options as the source (beginning, now, timestamp)

## When to Use

Use the bootstrap provider when your query needs access to **historical** events that were emitted before the streaming source started. Common scenarios:

- **Order book reconstruction** — load all historical `OrderPlaced` events to build a snapshot of open orders
- **Analytics dashboards** — populate initial metrics from past events before switching to real-time updates
- **Back-testing** — replay a window of historical events through a Drasi query

If you only care about events going forward, you can skip the bootstrap entirely and set the source to `StartPosition::Now`.

## Configuration

### Builder Pattern

```rust
use drasi_bootstrap_sui_deepbook::SuiDeepBookBootstrapProvider;

let bootstrap = SuiDeepBookBootstrapProvider::builder()
    .with_rpc_endpoint("https://fullnode.mainnet.sui.io:443")
    .with_deepbook_package_id("0x337f4f4f6567fcd778d5454f27c16c70e2f274cc6377ea6249ddf491482ef497")
    .with_request_limit(100)
    .with_max_pages(50)
    .with_start_from_beginning()
    .build()?;
```

### Configuration Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `rpc_endpoint` | `String` | `https://fullnode.mainnet.sui.io:443` | Sui JSON-RPC endpoint URL |
| `deepbook_package_id` | `String` | Mainnet DeepBook V3 address | Package ID to filter events by |
| `request_limit` | `u16` | `100` | Max events per RPC page (1–1000) |
| `max_pages` | `u32` | `10` | Maximum number of RPC pages to fetch |
| `event_filters` | `Vec<String>` | `[]` | Event type substrings to include (empty = all) |
| `pools` | `Vec<String>` | `[]` | Pool IDs to include (empty = all) |
| `start_position` | `StartPosition` | `Beginning` | Where to begin loading historical events |

### Start Position

| Variant | Behaviour |
|---------|-----------|
| `StartPosition::Beginning` | Load from the earliest available event on chain |
| `StartPosition::Now` | Skip bootstrap entirely (returns 0 events) |
| `StartPosition::Timestamp(ms)` | Load from the beginning but skip events with `timestamp_ms < ms` |

> **Note**: The bootstrap defaults to `Beginning` while the streaming source defaults to `Now`. This is intentional — the bootstrap's purpose is to load history.

## Usage Examples

### 1. Full Historical Load + Live Streaming

```rust
use drasi_source_sui_deepbook::SuiDeepBookSource;
use drasi_bootstrap_sui_deepbook::SuiDeepBookBootstrapProvider;

let bootstrap = SuiDeepBookBootstrapProvider::builder()
    .with_max_pages(100)
    .with_start_from_beginning()
    .build()?;

let source = SuiDeepBookSource::builder("deepbook-source")
    .with_bootstrap_provider(bootstrap)
    .with_start_from_beginning()
    .build()?;
```

### 2. Load Only Recent History (Last 24 Hours)

```rust
use chrono::Utc;

let twenty_four_hours_ago = Utc::now().timestamp_millis() - 86_400_000;

let bootstrap = SuiDeepBookBootstrapProvider::builder()
    .with_max_pages(200)
    .with_start_from_timestamp(twenty_four_hours_ago)
    .build()?;
```

### 3. Bootstrap Specific Event Types for a Specific Pool

```rust
let bootstrap = SuiDeepBookBootstrapProvider::builder()
    .with_event_filters(vec![
        "OrderPlaced".into(),
        "OrderFilled".into(),
        "OrderCancelled".into(),
    ])
    .with_pools(vec!["0xpool_sui_usdc".into()])
    .with_max_pages(50)
    .build()?;
```

### 4. Skip Bootstrap Entirely

If you don't need historical data, either omit the bootstrap provider or use `StartPosition::Now`:

```rust
let bootstrap = SuiDeepBookBootstrapProvider::builder()
    .with_start_from_now()
    .build()?;
// bootstrap() will return Ok(0) immediately
```

## How It Works

1. The bootstrap provider is called once per query subscription
2. It pages through `suix_queryEvents` with ascending order (oldest first)
3. Each event passes through the same filtering pipeline as the streaming source:
   - Package ID check → Event type filter → Pool filter
4. Matching events are mapped to `SourceChange::Insert` records (all bootstrap events are inserts)
5. Events are sent to the query engine via the bootstrap channel
6. The bootstrap completes and the streaming source begins normal polling

## Controlling Bootstrap Size

The `max_pages` setting bounds how many RPC pages the bootstrap will fetch. Each page contains up to `request_limit` events. The total upper bound is `max_pages × request_limit` raw events (before filtering).

Since the source queries `{"All": []}` and filters client-side by `deepbook_package_id`, most pages will contain events from other packages. Plan for a higher `max_pages` than you might expect.

| Scenario | Suggested `max_pages` |
|----------|----------------------|
| Quick smoke test | `5–10` |
| Recent activity (hours) | `50–100` |
| Full historical load | `500–1000` |

## Testing

```bash
cargo test -p drasi-bootstrap-sui-deepbook
```
