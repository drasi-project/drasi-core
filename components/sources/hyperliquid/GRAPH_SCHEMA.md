# Hyperliquid Source – Graph Schema Reference

## 1. Overview

The Hyperliquid source models DeFi market data as a **Coin-centric labeled property graph**. Every market data entity (trade, mid-price, order book, liquidation, funding rate) is represented as a node that connects back to a central `Coin` node via a typed relationship. This star-shaped topology was chosen because:

- **Coin is the natural join key** – Hyperliquid's API organizes all data by coin/asset name, making `Coin` the canonical anchor.
- **Cross-entity correlation** – queries can traverse from any data point (e.g., a Trade) through the Coin to reach related data (e.g., FundingRate) using graph traversal instead of string joins.
- **Selective subscriptions** – each data stream (trades, order books, etc.) is independently toggleable, so the graph gracefully supports partial schemas where only some node types exist.

```
                 ┌───────────┐
                 │   Coin    │
                 │ (anchor)  │
                 └─────┬─────┘
          ┌────────┬───┼───┬──────────┐
          │        │   │   │          │
          ▼        ▼   ▼   ▼          ▼
       Trade   MidPrice │ Liquidation FundingRate
                   OrderBook
```

---

## 2. Node Types

### 2.1 Coin

The central anchor node representing a tradable asset. Created during bootstrap for every perpetual asset and every spot token.

| Property | Type | Always Present | Description | Example |
|---|---|---|---|---|
| `name` | String | ✅ | Ticker symbol of the asset | `"BTC"` |
| `sz_decimals` | Integer | ✅ | Number of decimals for size precision | `5` |
| `max_leverage` | Integer | ✅ | Maximum leverage (0 for spot tokens) | `50` |
| `market_type` | String | ✅ | `"perp"` for perpetual assets, `"spot"` for spot tokens | `"perp"` |

- **ID format:** `coin:{name}` — e.g., `coin:BTC`, `coin:ETH`, `coin:PURR`
- **Lifecycle:** Insert-only. Created once during bootstrap and never updated.
- **Source API:** `POST /info` with `{"type": "meta"}` (perps) and `{"type": "spotMeta"}` (spot tokens).
- **Notes:** If a token appears in both perp `meta` and `spotMeta`, it is created only once (from the perp metadata, with `market_type: "perp"`). Spot-only tokens get `max_leverage: 0`.

---

### 2.2 Trade

An individual trade execution event. A new node is inserted for every trade; trades are never updated.

| Property | Type | Always Present | Description | Example |
|---|---|---|---|---|
| `coin` | String | ✅ | Asset ticker | `"BTC"` |
| `side` | String | ✅ | Trade side (`"A"` for ask/sell, `"B"` for bid/buy) | `"B"` |
| `price` | Float | ✅ | Execution price | `67432.5` |
| `size` | Float | ✅ | Trade size in base asset units | `0.15` |
| `timestamp` | Integer | ✅ | Execution time (milliseconds since Unix epoch) | `1719500000000` |
| `tid` | Integer | ✅ | Unique trade ID assigned by Hyperliquid | `123456789` |
| `hash` | String | ❌ | Transaction hash (may be absent for some trades) | `"0xabc..."` |

- **ID format:** `trade:{coin}:{tid}` — e.g., `trade:BTC:123456789`
- **Lifecycle:** Insert-only. Each trade event creates a new node. Trades are never updated or deleted.
- **Source API:** WebSocket channel `trades` (streaming only; not bootstrapped).
- **Notes:** Trade deduplication uses persisted `tid` values when a `StateStoreProvider` is available, preventing duplicate inserts on WebSocket reconnect.

---

### 2.3 MidPrice

The current mid-market price for a coin. One node per coin; updated in-place on each tick.

| Property | Type | Always Present | Description | Example |
|---|---|---|---|---|
| `coin` | String | ✅ | Asset ticker | `"ETH"` |
| `price` | Float | ✅ | Mid-market price (average of best bid and best ask) | `3521.75` |
| `timestamp` | Integer | ✅ | Time of the price update (milliseconds since epoch) | `1719500000000` |

- **ID format:** `midprice:{coin}` — e.g., `midprice:BTC`, `midprice:ETH`
- **Lifecycle:** First appearance is an insert (creates the node + `PRICE_OF` relationship). All subsequent appearances are updates to the existing node.
- **Source API:** Bootstrap via `POST /info` with `{"type": "allMids"}`; streaming via WebSocket channel `allMids`.
- **Update frequency:** Sub-second during active trading. The `allMids` channel pushes a full snapshot of all mid-prices on every tick.

---

### 2.4 OrderBook

Top-of-book summary for a coin's L2 order book. One node per coin; updated in-place.

| Property | Type | Always Present | Description | Example |
|---|---|---|---|---|
| `coin` | String | ✅ | Asset ticker | `"BTC"` |
| `best_bid_price` | Float | ✅ | Highest bid price | `67430.0` |
| `best_bid_size` | Float | ✅ | Size at the best bid level | `2.5` |
| `best_ask_price` | Float | ✅ | Lowest ask price | `67435.0` |
| `best_ask_size` | Float | ✅ | Size at the best ask level | `1.8` |
| `bid_depth` | Integer | ✅ | Number of bid price levels in the book | `20` |
| `ask_depth` | Integer | ✅ | Number of ask price levels in the book | `20` |
| `timestamp` | Integer | ✅ | Time of the book snapshot (milliseconds since epoch) | `1719500000000` |

- **ID format:** `orderbook:{coin}` — e.g., `orderbook:BTC`
- **Lifecycle:** First appearance is an insert (creates the node + `BOOK_OF` relationship). All subsequent appearances are updates. Skipped entirely if either bid or ask side is empty.
- **Source API:** Bootstrap via `POST /info` with `{"type": "l2Book", "coin": "<COIN>"}` (per coin); streaming via WebSocket channel `l2Book`.
- **Update frequency:** Very high during active trading; every order book change triggers an update.

---

### 2.5 Liquidation

A liquidation event. A new node is inserted for each liquidation; liquidations are never updated.

| Property | Type | Always Present | Description | Example |
|---|---|---|---|---|
| `coin` | String | ✅ | Asset ticker | `"SOL"` |
| `side` | String | ✅ | Side of the liquidation (`"A"` or `"B"`) | `"A"` |
| `price` | Float | ✅ | Liquidation price | `142.35` |
| `size` | Float | ✅ | Liquidated position size | `500.0` |
| `timestamp` | Integer | ✅ | Liquidation time (milliseconds since epoch) | `1719500000000` |
| `hash` | String | ❌ | Transaction hash (may be absent) | `"0xdef..."` |

- **ID format:**
  - If `hash` is present: `liquidation:{coin}:{hash}` — e.g., `liquidation:SOL:0xdef...`
  - If `hash` is absent: `liquidation:{coin}:{timestamp}` — e.g., `liquidation:SOL:1719500000000`
- **Lifecycle:** Insert-only. Each liquidation event creates a new node.
- **Source API:** WebSocket channel `liquidations` (streaming only; not bootstrapped).

---

### 2.6 FundingRate

Current funding rate and related metrics for a perpetual asset. One node per coin; updated in-place.

| Property | Type | Always Present | Description | Example |
|---|---|---|---|---|
| `coin` | String | ✅ | Asset ticker | `"BTC"` |
| `rate` | Float | ✅ | Current funding rate | `0.0001` |
| `premium` | Float | ✅ | Funding premium | `0.00005` |
| `mark_price` | Float | ✅ | Current mark price | `67432.0` |
| `open_interest` | Float | ✅ | Total open interest (notional) | `5000000.0` |
| `volume_24h` | Float | ✅ | 24-hour notional trading volume | `150000000.0` |
| `timestamp` | Integer | ✅ | Time of the data snapshot (milliseconds since epoch) | `1719500000000` |

- **ID format:** `funding:{coin}` — e.g., `funding:BTC`
- **Lifecycle:** First appearance is an insert (creates the node + `FUNDING_OF` relationship). Subsequent polls update the existing node. Redundant updates are suppressed when a `StateStoreProvider` is available (the previous snapshot is compared, and identical data is not re-emitted).
- **Source API:** Bootstrap and polling via `POST /info` with `{"type": "metaAndAssetCtxs"}`.
- **Update frequency:** Polled at the configured `funding_poll_interval_secs` (default: 60 seconds).

---

### 2.7 SpotPair

A spot trading pair definition linking two or more tokens. Created during bootstrap only.

| Property | Type | Always Present | Description | Example |
|---|---|---|---|---|
| `name` | String | ✅ | Pair name | `"PURR/USDC"` |
| `tokens` | List\<String\> | ✅ | Resolved token names in the pair | `["PURR", "USDC"]` |

- **ID format:** `spotpair:{name}` — e.g., `spotpair:PURR/USDC`
- **Lifecycle:** Insert-only. Created once during bootstrap from spot metadata.
- **Source API:** `POST /info` with `{"type": "spotMeta"}`.
- **Notes:** The `tokens` property contains resolved token names (looked up from token indices in the raw API response), not raw index values.

---

## 3. Relationship Types

All relationships are property-less edges that connect data nodes to their parent `Coin` node. Relationships point **from** the data node **to** the `Coin` node.

### 3.1 TRADED_ON

| Attribute | Value |
|---|---|
| **Pattern** | `(Trade)-[:TRADED_ON]->(Coin)` |
| **ID format** | `traded_on:trade:{coin}:{tid}` |
| **Example ID** | `traded_on:trade:BTC:123456789` |
| **Created** | Streaming – inserted alongside each new `Trade` node |
| **Cardinality** | Many-to-one (many trades per coin) |

### 3.2 PRICE_OF

| Attribute | Value |
|---|---|
| **Pattern** | `(MidPrice)-[:PRICE_OF]->(Coin)` |
| **ID format** | `price_of:midprice:{coin}` |
| **Example ID** | `price_of:midprice:BTC` |
| **Created** | Bootstrap or first streaming insert – created once with the initial `MidPrice` node |
| **Cardinality** | One-to-one (one mid-price per coin) |

### 3.3 BOOK_OF

| Attribute | Value |
|---|---|
| **Pattern** | `(OrderBook)-[:BOOK_OF]->(Coin)` |
| **ID format** | `book_of:orderbook:{coin}` |
| **Example ID** | `book_of:orderbook:ETH` |
| **Created** | Bootstrap or first streaming insert – created once with the initial `OrderBook` node |
| **Cardinality** | One-to-one (one order book per coin) |

### 3.4 LIQUIDATED_ON

| Attribute | Value |
|---|---|
| **Pattern** | `(Liquidation)-[:LIQUIDATED_ON]->(Coin)` |
| **ID format** | `liquidated_on:{liquidation_node_id}` |
| **Example ID** | `liquidated_on:liquidation:SOL:1719500000000` or `liquidated_on:liquidation:SOL:0xdef...` |
| **Created** | Streaming – inserted alongside each new `Liquidation` node |
| **Cardinality** | Many-to-one (many liquidations per coin) |

### 3.5 FUNDING_OF

| Attribute | Value |
|---|---|
| **Pattern** | `(FundingRate)-[:FUNDING_OF]->(Coin)` |
| **ID format** | `funding_of:funding:{coin}` |
| **Example ID** | `funding_of:funding:BTC` |
| **Created** | Bootstrap or first poll – created once with the initial `FundingRate` node |
| **Cardinality** | One-to-one (one funding rate per coin) |

---

## 4. Graph Visualization

```
                              ┌─────────────┐
                              │  SpotPair   │
                              │ "PURR/USDC" │
                              └─────────────┘
                              (standalone - no
                               relationships)

    ┌──────────────┐    TRADED_ON     ┌──────────┐    PRICE_OF     ┌──────────────┐
    │    Trade     │ ───────────────► │          │ ◄─────────────── │   MidPrice   │
    │ (insert-only)│                  │          │                  │  (updated)   │
    └──────────────┘                  │          │                  └──────────────┘
                                      │   Coin   │
    ┌──────────────┐  LIQUIDATED_ON   │ (anchor) │   BOOK_OF       ┌──────────────┐
    │ Liquidation  │ ───────────────► │          │ ◄─────────────── │  OrderBook   │
    │ (insert-only)│                  │          │                  │  (updated)   │
    └──────────────┘                  │          │                  └──────────────┘
                                      │          │
                                      │          │   FUNDING_OF    ┌──────────────┐
                                      │          │ ◄─────────────── │ FundingRate  │
                                      └──────────┘                  │  (updated)   │
                                                                    └──────────────┘

    Legend:
      ──────►  Relationship direction (source → target)
      (insert-only)  = New node per event, never updated
      (updated)      = Single node per coin, updated in-place
      (anchor)       = Created at bootstrap, never updated
```

---

## 5. Bootstrap vs Streaming

### Bootstrap Phase (REST API)

During bootstrap, the source calls Hyperliquid's `/info` REST endpoint to build the initial graph snapshot. Nodes are sent before relationships to ensure referential integrity.

| Step | REST Request Type | Entities Created | Condition |
|---|---|---|---|
| 1 | `meta` | `Coin` nodes (perp assets) | Always |
| 2 | `spotMeta` | `Coin` nodes (spot tokens) + `SpotPair` nodes | Always |
| 3 | `allMids` | `MidPrice` nodes + `PRICE_OF` relationships | Always (filtered by coin selection) |
| 4 | `metaAndAssetCtxs` | `FundingRate` nodes + `FUNDING_OF` relationships | Always (filtered by coin selection) |
| 5 | `l2Book` (per coin) | `OrderBook` nodes + `BOOK_OF` relationships | Always (filtered by coin selection) |

**Notes:**
- Steps 1–2 create `Coin` and `SpotPair` nodes regardless of which data streams are enabled. This ensures the anchor nodes are always present.
- Steps 3–5 create the singleton data nodes and their relationships. These nodes are then updated by the streaming phase.
- `Trade` and `Liquidation` nodes are **never bootstrapped** — they only appear from streaming.

### Streaming Phase (WebSocket + REST Polling)

After bootstrap, the source opens a WebSocket connection and optionally starts a funding rate poll loop.

| Channel / Method | Entity | Behavior | Controlled By |
|---|---|---|---|
| WS `trades` | `Trade` + `TRADED_ON` | Insert new node + relationship per event | `enable_trades` |
| WS `l2Book` | `OrderBook` | Update existing node (or insert + `BOOK_OF` on first) | `enable_order_book` |
| WS `allMids` | `MidPrice` | Update existing node (or insert + `PRICE_OF` on first) | `enable_mid_prices` |
| WS `liquidations` | `Liquidation` + `LIQUIDATED_ON` | Insert new node + relationship per event | `enable_liquidations` |
| REST poll `metaAndAssetCtxs` | `FundingRate` | Update existing node (or insert + `FUNDING_OF` on first) | `enable_funding_rates` |

### Insert vs Update Transition

Singleton nodes (`MidPrice`, `OrderBook`, `FundingRate`) use an `InitializedEntities` tracker:
1. **First time a coin is seen** → `SourceChange::Insert` for the node and its relationship.
2. **Subsequent updates** → `SourceChange::Update` for the node only (relationship is already established).

This means if a `MidPrice` was bootstrapped, the first streaming update for that coin will be an `Update`. If a coin was not present during bootstrap but appears in streaming, the first streaming event will be an `Insert`.

---

## 6. ID Convention Reference

| Entity | ID Format | Example | Unique By |
|---|---|---|---|
| Coin | `coin:{name}` | `coin:BTC` | Asset name |
| Trade | `trade:{coin}:{tid}` | `trade:BTC:123456789` | Coin + trade ID |
| MidPrice | `midprice:{coin}` | `midprice:ETH` | Coin (singleton) |
| OrderBook | `orderbook:{coin}` | `orderbook:BTC` | Coin (singleton) |
| Liquidation | `liquidation:{coin}:{hash\|timestamp}` | `liquidation:SOL:0xdef...` | Coin + hash (or timestamp) |
| FundingRate | `funding:{coin}` | `funding:BTC` | Coin (singleton) |
| SpotPair | `spotpair:{name}` | `spotpair:PURR/USDC` | Pair name |
| TRADED_ON | `traded_on:trade:{coin}:{tid}` | `traded_on:trade:BTC:123456789` | Source trade ID |
| PRICE_OF | `price_of:midprice:{coin}` | `price_of:midprice:BTC` | Source mid-price ID |
| BOOK_OF | `book_of:orderbook:{coin}` | `book_of:orderbook:ETH` | Source order book ID |
| LIQUIDATED_ON | `liquidated_on:{liquidation_node_id}` | `liquidated_on:liquidation:SOL:0xdef...` | Source liquidation ID |
| FUNDING_OF | `funding_of:funding:{coin}` | `funding_of:funding:BTC` | Source funding ID |

**Pattern:** Relationship IDs are formed as `{relation_prefix}:{source_node_id}`, where the relation prefix is the lowercase/underscore form of the relationship type (e.g., `traded_on`, `price_of`, `book_of`, `liquidated_on`, `funding_of`).

---

## 7. Example Cypher Queries

### Price Monitoring

```cypher
// Get the current mid-price for BTC
MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin {name: "BTC"})
RETURN m.price, m.timestamp

// Find coins where mid-price exceeds a threshold
MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin)
WHERE m.price > 1000
RETURN c.name, m.price

// Monitor the bid-ask spread from the order book
MATCH (ob:OrderBook)-[:BOOK_OF]->(c:Coin {name: "ETH"})
RETURN c.name, ob.best_ask_price - ob.best_bid_price AS spread,
       ob.best_bid_price, ob.best_ask_price

// Get mid-prices for all coins
MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin)
RETURN c.name, m.price, m.timestamp
```

### Trade Analysis

```cypher
// Calculate notional value of trades
MATCH (t:Trade)-[:TRADED_ON]->(c:Coin {name: "ETH"})
RETURN t.tid, t.price * t.size AS notional, t.side, t.timestamp

// Count buys vs sells
MATCH (t:Trade)-[:TRADED_ON]->(c:Coin {name: "BTC"})
RETURN t.side, count(t) AS trade_count, sum(t.size) AS total_volume

// Find large trades (whale detection)
MATCH (t:Trade)-[:TRADED_ON]->(c:Coin)
WHERE t.size * t.price > 100000
RETURN c.name, t.price, t.size, t.side, t.timestamp

// Get trades for a specific coin
MATCH (t:Trade)-[:TRADED_ON]->(c:Coin {name: "BTC"})
RETURN t.price, t.size, t.side, t.timestamp, t.tid
```

### Cross-Entity Correlation

```cypher
// Compare mid-price with order book for a coin
MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin {name: "BTC"}),
      (ob:OrderBook)-[:BOOK_OF]->(c)
RETURN c.name, m.price AS mid_price,
       ob.best_bid_price, ob.best_ask_price,
       ob.best_ask_price - ob.best_bid_price AS spread

// Compare mark price (from funding) with mid-price
MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin),
      (f:FundingRate)-[:FUNDING_OF]->(c)
RETURN c.name, m.price AS mid_price, f.mark_price,
       f.mark_price - m.price AS basis

// Find liquidations alongside current funding rates
MATCH (l:Liquidation)-[:LIQUIDATED_ON]->(c:Coin),
      (f:FundingRate)-[:FUNDING_OF]->(c)
RETURN c.name, l.price AS liquidation_price, l.size, l.side,
       f.rate AS funding_rate, f.open_interest
```

### Aggregation Queries

```cypher
// Total open interest across all coins
MATCH (f:FundingRate)-[:FUNDING_OF]->(c:Coin)
RETURN sum(f.open_interest) AS total_oi

// Average funding rate across all coins
MATCH (f:FundingRate)-[:FUNDING_OF]->(c:Coin)
RETURN avg(f.rate) AS avg_funding_rate, count(c) AS coin_count

// High volume coins
MATCH (f:FundingRate)-[:FUNDING_OF]->(c:Coin)
WHERE f.volume_24h > 50000000
RETURN c.name, f.volume_24h

// Order book depth summary across coins
MATCH (ob:OrderBook)-[:BOOK_OF]->(c:Coin)
RETURN c.name, ob.bid_depth, ob.ask_depth,
       ob.bid_depth + ob.ask_depth AS total_levels
```

### Filtering by Coin Metadata

```cypher
// Get mid-prices only for perpetual assets
MATCH (m:MidPrice)-[:PRICE_OF]->(c:Coin {market_type: "perp"})
RETURN c.name, m.price

// Find high-leverage coins with their funding rates
MATCH (f:FundingRate)-[:FUNDING_OF]->(c:Coin)
WHERE c.max_leverage >= 50
RETURN c.name, c.max_leverage, f.rate, f.mark_price

// Get spot-only coins
MATCH (c:Coin {market_type: "spot"})
RETURN c.name, c.sz_decimals

// List all spot trading pairs with their constituent tokens
MATCH (sp:SpotPair)
RETURN sp.name, sp.tokens
```

---

## 8. Data Freshness

| Entity | Update Mechanism | Typical Latency | Frequency |
|---|---|---|---|
| **Coin** | Bootstrap only | One-time at startup | Never updated after creation |
| **SpotPair** | Bootstrap only | One-time at startup | Never updated after creation |
| **MidPrice** | WebSocket `allMids` | Sub-second | Every market tick (hundreds per second during active trading) |
| **OrderBook** | WebSocket `l2Book` | Sub-second | Every order book change (very high frequency) |
| **Trade** | WebSocket `trades` | Sub-second | Every trade execution (frequency varies by coin) |
| **Liquidation** | WebSocket `liquidations` | Sub-second | Per liquidation event (sporadic, depends on market volatility) |
| **FundingRate** | REST polling `metaAndAssetCtxs` | Configurable | Every `funding_poll_interval_secs` (default: 60s) |

### Latency Notes

- **WebSocket streams** deliver data with minimal latency (typically under 100ms from Hyperliquid's servers). Actual latency depends on network conditions to `wss://api.hyperliquid.xyz/ws`.
- **Funding rate polling** adds up to `funding_poll_interval_secs` of staleness. Redundant updates are suppressed when the snapshot hasn't changed (requires a `StateStoreProvider`).
- **Bootstrap data** represents a point-in-time snapshot taken at source startup. Once streaming begins, singleton nodes (`MidPrice`, `OrderBook`, `FundingRate`) are updated to reflect real-time values.
- **Initial cursor** (`StartFromNow`, `StartFromBeginning`, `StartFromTimestamp`) controls whether streaming events received before a certain timestamp are filtered out. This affects when the first streaming updates appear but does not affect bootstrap data.
