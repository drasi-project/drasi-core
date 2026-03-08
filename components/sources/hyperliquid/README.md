# Hyperliquid Source

A Drasi source that connects to [Hyperliquid](https://hyperliquid.xyz)'s public APIs to stream real-time DeFi perpetual and spot market data as a continuously-queryable graph. No API keys required.

## What You Can Build

- **Price alert bots** — trigger when BTC crosses $100k or ETH drops 5% in a minute
- **Whale watchers** — detect trades over $1M in real time
- **Spread monitors** — alert when bid-ask spread widens beyond a threshold
- **Funding rate dashboards** — track which coins have extreme funding
- **Liquidation feeds** — monitor liquidation cascades across assets
- **Cross-asset correlation** — join trades, prices, and funding through graph traversal

## Quick Start

```rust
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_log::LogReaction;
use drasi_source_hyperliquid::{HyperliquidSource, HyperliquidNetwork};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Create a source streaming BTC and ETH data
    let source = HyperliquidSource::builder("hl")
        .with_network(HyperliquidNetwork::Mainnet)
        .with_coins(vec!["BTC", "ETH"])
        .with_mid_prices(true)
        .with_trades(true)
        .start_from_now()
        .build()?;

    // Alert when BTC price changes
    let query = Query::cypher("btc-price")
        .query(r#"
            MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin {name: "BTC"})
            RETURN c.name AS coin, m.price AS price
        "#)
        .from_source("hl")
        .auto_start(true)
        .enable_bootstrap(true)
        .build();

    let reaction = LogReaction::builder("log")
        .from_query("btc-price")
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build().await?,
    );

    core.start().await?;
    println!("Streaming... press Ctrl+C to stop");
    tokio::signal::ctrl_c().await?;
    core.stop().await?;
    Ok(())
}
```

## Graph Model

All data flows through a **Coin-centric** graph. Every market data entity connects back to a `Coin` node, enabling graph traversal queries that correlate across data types.

```
                          ┌───────────┐
                          │   Coin    │
                          │ (anchor)  │
                          └─────┬─────┘
                 ┌────────┬─────┼─────┬──────────┐
                 │        │     │     │           │
                 ▼        ▼     ▼     ▼           ▼
              Trade   MidPrice  │  Liquidation  FundingRate
                          OrderBook
```

See [GRAPH_SCHEMA.md](GRAPH_SCHEMA.md) for complete property tables, ID formats, and relationship details.

## Example Queries

### 🐋 Whale Detection — Large Trades in Real Time

Continuously alert on trades over $100k notional value:

```cypher
MATCH (t:Trade)-[:TRADED_ON]->(c:Coin)
WHERE t.price * t.size > 100000
RETURN c.name AS coin, t.side AS side,
       t.price AS price, t.size AS size,
       t.price * t.size AS notional
```

Every time a whale trade lands, your reaction fires with the coin, direction, and size.

### 📊 Spread Monitor — Bid-Ask Spread Alert

Alert when any coin's spread widens beyond 0.1%:

```cypher
MATCH (ob:OrderBook)-[:BOOK_OF]->(c:Coin),
      (m:MidPrice)-[:PRICE_OF]->(c)
WHERE (ob.best_ask_price - ob.best_bid_price) / m.price > 0.001
RETURN c.name AS coin,
       ob.best_bid_price AS bid, ob.best_ask_price AS ask,
       ob.best_ask_price - ob.best_bid_price AS spread,
       m.price AS mid_price
```

### 💰 Funding Arbitrage Scanner

Find coins with extreme funding rates (potential arbitrage opportunity):

```cypher
MATCH (f:FundingRate)-[:FUNDING_OF]->(c:Coin),
      (m:MidPrice)-[:PRICE_OF]->(c)
WHERE f.rate > 0.01 OR f.rate < -0.01
RETURN c.name AS coin, f.rate AS funding_rate,
       f.mark_price AS mark, m.price AS mid,
       f.mark_price - m.price AS basis,
       f.open_interest AS oi
```

### 🔥 Liquidation Cascade Detector

Monitor liquidation events alongside funding rates to spot cascades:

```cypher
MATCH (l:Liquidation)-[:LIQUIDATED_ON]->(c:Coin),
      (f:FundingRate)-[:FUNDING_OF]->(c)
RETURN c.name AS coin, l.side AS liq_side,
       l.price AS liq_price, l.size AS liq_size,
       f.rate AS funding, f.open_interest AS oi
```

### 📈 Multi-Asset Price Dashboard

Track real-time prices for your watchlist with leverage info:

```cypher
MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin)
RETURN c.name AS coin, c.max_leverage AS leverage,
       c.market_type AS type, m.price AS price
```

### 🔗 Cross-Entity Correlation — Price vs Mark vs Funding

Join mid-price, order book, and funding through the Coin node in one query:

```cypher
MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin {name: "ETH"}),
      (ob:OrderBook)-[:BOOK_OF]->(c),
      (f:FundingRate)-[:FUNDING_OF]->(c)
RETURN m.price AS mid_price,
       ob.best_bid_price AS bid, ob.best_ask_price AS ask,
       f.mark_price AS mark, f.rate AS funding,
       f.open_interest AS open_interest
```

### 🏦 Open Interest Monitor

Alert when a coin's open interest crosses a threshold:

```cypher
MATCH (f:FundingRate)-[:FUNDING_OF]->(c:Coin)
WHERE f.open_interest > 10000000
RETURN c.name AS coin, f.open_interest AS oi, f.volume_24h AS vol_24h
```

### ⚡ High-Leverage Coin Scan

Find all coins offering 50x+ leverage with their current metrics:

```cypher
MATCH (f:FundingRate)-[:FUNDING_OF]->(c:Coin),
      (m:MidPrice)-[:PRICE_OF]->(c)
WHERE c.max_leverage >= 50
RETURN c.name AS coin, c.max_leverage AS leverage,
       m.price AS price, f.rate AS funding, f.volume_24h AS volume
```

## Configuration

```rust
let source = HyperliquidSource::builder("my-source")
    .with_network(HyperliquidNetwork::Mainnet)    // or Testnet, or Custom { rest_url, ws_url }
    .with_coins(vec!["BTC", "ETH", "SOL"])         // or .with_all_coins()
    .with_trades(true)                             // trade events (high volume)
    .with_order_book(true)                         // L2 book snapshots (high volume)
    .with_mid_prices(true)                         // mid-market prices (moderate)
    .with_funding_rates(true)                      // funding rate polling (low volume)
    .with_liquidations(true)                       // liquidation events (sporadic)
    .with_funding_poll_interval_secs(30)           // poll every 30s instead of default 60s
    .start_from_beginning()                        // or .start_from_now() / .start_from_timestamp(ms)
    .build()?;
```

### Config Reference

| Field | Type | Default | Description |
|---|---|---|---|
| `network` | `HyperliquidNetwork` | `Mainnet` | API environment |
| `coins` | `CoinSelection` | `All` | `Specific { coins }` or `All` |
| `enable_trades` | `bool` | `false` | Trade event stream |
| `enable_order_book` | `bool` | `false` | L2 order book stream |
| `enable_mid_prices` | `bool` | `true` | Mid-price stream |
| `enable_funding_rates` | `bool` | `false` | Funding rate polling |
| `enable_liquidations` | `bool` | `false` | Liquidation event stream |
| `funding_poll_interval_secs` | `u64` | `60` | Funding poll interval |
| `initial_cursor` | `InitialCursor` | `StartFromNow` | Where to start streaming |

## Bootstrap & Streaming

**Bootstrap** (REST): On startup, the source fetches a snapshot of all current data via Hyperliquid's `/info` REST endpoints. Coin and SpotPair nodes are always bootstrapped. MidPrice, FundingRate, and OrderBook are bootstrapped only when their respective streams are enabled.

**Streaming** (WebSocket + REST polling): After bootstrap, the source opens a WebSocket connection and subscribes to enabled channels. Funding rates are polled via REST at the configured interval.

| Data | Bootstrap | Streaming | Update Pattern |
|---|---|---|---|
| Coin | ✅ Always | — | Insert once, never updated |
| SpotPair | ✅ Always | — | Insert once, never updated |
| MidPrice | ✅ If enabled | WebSocket `allMids` | Insert first, then update |
| OrderBook | ✅ If enabled | WebSocket `l2Book` | Insert first, then update |
| FundingRate | ✅ If enabled | REST poll | Insert first, then update |
| Trade | — | WebSocket `trades` | Insert per event (append-only) |
| Liquidation | — | WebSocket `liquidations` | Insert per event (append-only) |

## State Persistence

When a `StateStoreProvider` is available:
- **Trade deduplication** — persists last seen `tid` per coin; prevents duplicates on WebSocket reconnect
- **Funding snapshots** — persists last funding state; skips redundant updates when data hasn't changed

## Testing

```bash
# Unit tests (no network)
cargo test -p drasi-source-hyperliquid

# Integration test against live mainnet (requires internet)
cargo test -p drasi-source-hyperliquid -- --ignored --nocapture
```

## Troubleshooting

| Symptom | Check |
|---|---|
| REST API unreachable | Verify connectivity to `https://api.hyperliquid.xyz/info` |
| WebSocket errors | `wscat -c wss://api.hyperliquid.xyz/ws` |
| No changes detected | Set `RUST_LOG=debug`, confirm channels are enabled |
| Too much data | Reduce coin list or disable high-volume streams (`trades`, `l2Book`) |
| Duplicate events on restart | Ensure a `StateStoreProvider` is configured |

## Limitations

- No historical replay — WebSocket starts from current data
- Funding rates are polled, not pushed in real time
- Subscribing to all trades + order books for all coins is very high volume
- SpotPair nodes have no relationships (standalone metadata)
