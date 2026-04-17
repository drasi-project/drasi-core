# Sui DeepBook Source — Graph Schema Reference

This document describes the graph structure produced by the `drasi-source-sui-deepbook` source. Every on-chain DeepBook V3 event becomes a **node** in the Drasi graph. When enrichment is enabled (the default), the source also emits **Pool**, **Trader**, and **Order** nodes linked to events via relationships.

---

## Graph Overview

```
(:DeepBookEvent:OrderPlaced)-[:IN_POOL]->(:Pool)
(:DeepBookEvent:OrderPlaced)-[:SENT_BY]->(:Trader)
(:DeepBookEvent:OrderPlaced)-[:FOR_ORDER]->(:Order)
```

### Event Nodes

```
(:DeepBookEvent:OrderPlaced   { entity_id, event_name, module, … })
(:DeepBookEvent:OrderFilled   { entity_id, event_name, module, … })
(:DeepBookEvent:OrderCancelled{ entity_id, event_name, module, … })
(:DeepBookEvent:PriceAdded    { entity_id, event_name, module, … })
(:DeepBookEvent:BalanceEvent  { entity_id, event_name, module, … })
(:DeepBookEvent:FlashLoanBorrowed { … })
   …and any other event type emitted by the DeepBook package
```

Every node carries **two labels**:

| Label | Meaning |
|-------|---------|
| `DeepBookEvent` | Constant on every node — use this for broad queries |
| _secondary_ | The short event name (e.g. `OrderPlaced`, `BalanceEvent`) — use this for targeted queries |

---

## Properties

### Fixed Properties (present on every node)

| Property | Type | Description | Example |
|----------|------|-------------|---------|
| `entity_id` | `String` | Unique entity identifier (see [Entity ID Rules](#entity-id-rules)) | `order:42` |
| `tx_digest` | `String` | Sui transaction digest that emitted the event | `"7fa2b3c…"` |
| `event_seq` | `String` | Sequence number of the event within its transaction | `"0"` |
| `package_id` | `String` | Move package address that emitted the event | `"0x337f4f…"` |
| `transaction_module` | `String` | Move module from the transaction context | `"pool"` |
| `sender` | `String` | Full address of the transaction sender | `"0x1a2b3c…"` |
| `sender_short` | `String` | Truncated sender for display (`0xABCD…WXYZ`) | `"0x1a2b…3c4d"` |
| `event_type` | `String` | Full qualified Move type path | `"0x337f…::events::OrderPlaced"` |
| `event_name` | `String` | Short name extracted from the type path | `"OrderPlaced"` |
| `module` | `String` | Move module that emitted the event | `"events"` |
| `change_type` | `String` | Operation classification: `insert`, `update`, or `delete` | `"insert"` |
| `timestamp_ms` | `Integer` | On-chain timestamp in milliseconds since epoch | `1772923888171` |
| `payload` | `Object` | The full `parsedJson` from the event as a nested object | `{size: "1000", …}` |

### Conditional Properties (present when the event payload contains the relevant field)

| Property | Type | Present When | Description |
|----------|------|-------------|-------------|
| `order_id` | `String` | Payload has `order_id` or `orderId` | Order identifier |
| `pool_id` | `String` | Payload has `pool_id`, `poolId`, or `pool` | Full pool address |
| `pool_id_short` | `String` | Same as `pool_id` | Truncated pool address for display |

### Accessing Payload Fields

The full event payload is stored as a nested `payload` object. Use Cypher dot-notation to access individual fields:

```cypher
-- Access nested payload fields directly
RETURN e.payload.price, e.payload.size, e.payload.is_bid
```

For example, if the on-chain event contains:

```json
{
  "order_id": "42",
  "price": "2340",
  "size": "1000",
  "is_bid": true,
  "pool_id": "0xabc…"
}
```

You access these as `e.payload.order_id`, `e.payload.price`, `e.payload.size`, etc.

> **Note**: Numeric strings from Sui (e.g. `"2340"`) remain strings. Use Cypher's `toInteger()` or `toFloat()` for numeric comparisons.

---

## Enrichment Nodes

When enrichment is enabled (the default), the source emits additional graph nodes for pools, traders, and orders, linked to event nodes via relationships. Each enrichment node is created once per unique identifier and cached in-memory.

### Pool Node (`:Pool`)

Created via `sui_getObject` RPC call on first encounter of a `pool_id`.

| Property | Type | Description | Example |
|----------|------|-------------|---------|
| `pool_id` | `String` | Full pool object address | `"0xabc…"` |
| `pool_id_short` | `String` | Truncated pool address | `"0xabc1…ef23"` |
| `base_asset` | `String` | Base asset Move type (from type params) | `"0x2::sui::SUI"` |
| `quote_asset` | `String` | Quote asset Move type (from type params) | `"0xabc::usdc::USDC"` |
| `tick_size` | `String` | Minimum price increment (raw units) | `"1000000"` |
| `lot_size` | `String` | Minimum order quantity (raw units) | `"100000000"` |
| `min_size` | `String` | Minimum trade size (raw units) | `"500000000"` |

**Entity ID**: `pool_meta:{pool_id}`

### Trader Node (`:Trader`)

Derived from `event.sender` — no additional RPC calls.

| Property | Type | Description | Example |
|----------|------|-------------|---------|
| `address` | `String` | Full sender address | `"0x1a2b…"` |
| `address_short` | `String` | Truncated address | `"0x1a2b…3c4d"` |

**Entity ID**: `trader:{address}`

### Order Node (`:Order`)

Derived from `order_id` in event payload — no additional RPC calls.

| Property | Type | Description | Example |
|----------|------|-------------|---------|
| `order_id` | `String` | Order identifier | `"42"` |
| `order_id_short` | `String` | Truncated order ID | `"42"` |
| `pool_id` | `String` | Pool where order was placed (if available) | `"0xabc…"` |

**Entity ID**: `order_meta:{order_id}`

### Relationships

| Relationship | Direction | Present When |
|-------------|-----------|-------------|
| `IN_POOL` | `(:DeepBookEvent)-[:IN_POOL]->(:Pool)` | Event has a `pool_id` |
| `SENT_BY` | `(:DeepBookEvent)-[:SENT_BY]->(:Trader)` | Always (every event has a sender) |
| `FOR_ORDER` | `(:DeepBookEvent)-[:FOR_ORDER]->(:Order)` | Event has an `order_id` |

**Relationship Entity IDs**: `rel:in_pool:{event_entity_id}`, `rel:sent_by:{event_entity_id}`, `rel:for_order:{event_entity_id}`

### Enrichment Configuration

Enrichment is controlled by three boolean config flags (all default to `true`):

| Config Field | Controls | Default |
|-------------|----------|---------|
| `enable_pool_nodes` | `:Pool` nodes + `IN_POOL` relationships | `true` |
| `enable_trader_nodes` | `:Trader` nodes + `SENT_BY` relationships | `true` |
| `enable_order_nodes` | `:Order` nodes + `FOR_ORDER` relationships | `true` |

---

## Entity ID Rules

The `entity_id` determines how nodes are identified and how updates/deletes correlate with prior inserts. The derivation follows a priority chain:

| Priority | Condition | Entity ID Format | Example |
|----------|-----------|------------------|---------|
| 1 (highest) | Payload has `order_id` or `orderId` | `order:{order_id}` | `order:42` |
| 2 | Payload has `pool_id`, `poolId`, or `pool` | `pool:{pool_id}` | `pool:0xabc…` |
| 3 (fallback) | Neither field present | `event:{tx_digest}:{event_seq}` | `event:0x7fa…:0` |

**Implications for queries:**
- Order-related events (`OrderPlaced`, `OrderFilled`, `OrderCancelled`) sharing the same `order_id` will target the same graph node, enabling the Drasi query engine to track the order lifecycle as ADD → UPDATE → DELETE.
- Pool-level events without an `order_id` are grouped by pool address.
- Events with neither field get a unique per-transaction ID and are never updated or deleted.

---

## Operation Classification

The `change_type` property and the `SourceChange` variant (Insert/Update/Delete) are determined by keyword matching on the event's full type name:

| Keywords in Event Type | Operation | SourceChange | `change_type` |
|------------------------|-----------|--------------|---------------|
| `cancel`, `delete`, `remove` | Delete | `SourceChange::Delete` | `"delete"` |
| `fill`, `update`, `modify`, `amend` | Update | `SourceChange::Update` | `"update"` |
| _(everything else)_ | Insert | `SourceChange::Insert` | `"insert"` |

Matching is **case-insensitive** and uses **substring contains**.

---

## Known DeepBook V3 Event Types

These are the event types observed on Sui mainnet from the DeepBook V3 package:

### Order Events (module: `events`)

| Event Type | Operation | Entity ID | Description |
|------------|-----------|-----------|-------------|
| `OrderPlaced` | Insert | `order:{order_id}` | A new limit or market order was submitted to the book |
| `OrderFilled` | Update | `order:{order_id}` | An order was partially or fully matched |
| `OrderCancelled` | Delete | `order:{order_id}` | An order was cancelled by the user or by the system (IOC/FOK) |
| `OrderModified` | Update | `order:{order_id}` | An existing order was amended (price or size change) |

**Typical payload fields**: `order_id`, `pool_id`, `price`, `size`, `is_bid`, `quantity`, `maker_fee`, `taker_fee`, `expire_timestamp`, `side`

### Price Events (module: `deep_price`)

| Event Type | Operation | Entity ID | Description |
|------------|-----------|-----------|-------------|
| `PriceAdded` | Insert | `pool:{pool_id}` | A new reference price was recorded for a pool |

**Typical payload fields**: `pool_id`, `price`, `size`

### Balance Events (module: `balance_manager`)

| Event Type | Operation | Entity ID | Description |
|------------|-----------|-----------|-------------|
| `BalanceEvent` | Insert | `event:{tx}:{seq}` | A balance change occurred in a user's BalanceManager |
| `DeepBookReferralSetEvent` | Insert | `event:{tx}:{seq}` | A referral was set for a balance manager |

**Typical payload fields**: `amount`, `balance`, `fee`

### Flash Loan Events (module: `vault`)

| Event Type | Operation | Entity ID | Description |
|------------|-----------|-----------|-------------|
| `FlashLoanBorrowed` | Insert | `event:{tx}:{seq}` | A flash loan was initiated from a pool's vault |

**Typical payload fields**: `amount`, `pool_id`

> **Note**: DeepBook may emit additional event types not listed here. All events from the configured `deepbook_package_id` are captured regardless of type.

---

## Query Examples

### 1. All Events (broad)

```cypher
MATCH (e:DeepBookEvent)
RETURN e.event_name, e.entity_id, e.change_type, e.timestamp_ms
```

### 2. Order Lifecycle Tracking

```cypher
MATCH (e:DeepBookEvent)
WHERE e.order_id IS NOT NULL
RETURN e.order_id, e.event_name, e.change_type, e.timestamp_ms
```

As an order progresses, the Drasi continuous query emits:
```
ADD    order:42  OrderPlaced   (change_type = "insert")
UPDATE order:42  OrderFilled   (change_type = "update")
DELETE order:42  OrderCancelled (change_type = "delete")
```

### 3. Specific Event Type by Label

```cypher
MATCH (e:OrderPlaced)
RETURN e.entity_id, e.pool_id, e.payload.price, e.payload.size, e.payload.is_bid
```

### 4. Filter by Pool

```cypher
MATCH (e:DeepBookEvent)
WHERE e.pool_id = '0xabc123…'
RETURN e.event_name, e.entity_id, e.sender_short, e.timestamp_ms
```

### 5. Large Orders (using payload fields)

```cypher
MATCH (e:OrderPlaced)
WHERE toInteger(e.payload.size) > 1000000
RETURN e.order_id, e.pool_id_short, e.payload.price, e.payload.size, e.sender_short
```

### 6. Balance Changes by Sender

```cypher
MATCH (e:BalanceEvent)
WHERE e.sender = '0x1a2b3c…full_address…'
RETURN e.entity_id, e.payload.amount, e.payload.balance, e.timestamp_ms
```

### 7. Cross-Event Join on Pool ID

```cypher
MATCH (order:OrderPlaced), (price:PriceAdded)
WHERE order.pool_id = price.pool_id
RETURN order.order_id, order.payload.price AS order_price, price.payload.price AS ref_price
```

### 8. Aggregate Events per Pool

```cypher
MATCH (e:DeepBookEvent)
WHERE e.pool_id IS NOT NULL
RETURN e.pool_id_short, e.event_name, count(e) AS event_count
```

### 9. Graph Traversal: Events for a Trading Pair (enrichment)

```cypher
MATCH (e:OrderPlaced)-[:IN_POOL]->(p:Pool)
WHERE p.base_asset CONTAINS 'SUI' AND p.quote_asset CONTAINS 'USDC'
RETURN e.payload.price, e.payload.size, p.tick_size
```

### 10. All Events by a Specific Trader (enrichment)

```cypher
MATCH (e:DeepBookEvent)-[:SENT_BY]->(t:Trader)
WHERE t.address = '0x1a2b3c…full_address…'
RETURN e.event_name, e.entity_id, e.timestamp_ms
```

### 11. Full Order Lifecycle via Graph Traversal (enrichment)

```cypher
MATCH (e:DeepBookEvent)-[:FOR_ORDER]->(o:Order)
WHERE o.order_id = '42'
RETURN e.event_name, e.change_type, e.timestamp_ms
```

### 12. Aggregate Events per Pool with Metadata (enrichment)

```cypher
MATCH (e:DeepBookEvent)-[:IN_POOL]->(p:Pool)
RETURN p.base_asset, p.quote_asset, count(e) AS events
```

### 13. Cross-Entity: Orders in a Specific Market (enrichment)

```cypher
MATCH (e:DeepBookEvent)-[:IN_POOL]->(p:Pool), (e)-[:FOR_ORDER]->(o:Order)
WHERE p.base_asset CONTAINS 'SUI'
RETURN o.order_id, e.event_name, e.payload.price
```

### 14. Pool Metadata Lookup (enrichment)

```cypher
MATCH (p:Pool)
RETURN p.pool_id_short, p.base_asset, p.quote_asset, p.tick_size, p.lot_size
```

---

## Filtering Pipeline

Events pass through three filter stages before becoming graph nodes:

```
Sui JSON-RPC → ① Package ID → ② Event Type → ③ Pool ID → Graph Node
```

| Stage | Config Field | Behaviour |
|-------|-------------|-----------|
| ① Package ID | `deepbook_package_id` | Only events from this package are kept. Always active. |
| ② Event Type | `event_filters` | If non-empty, event type must contain or end with one of the filter strings (case-insensitive). |
| ③ Pool ID | `pools` | If non-empty, the event must have a `pool_id`/`poolId`/`pool` field matching one of the listed IDs. |

Events that fail any stage are silently dropped and do not appear in the graph.

---

## Type Mapping (Sui → Drasi)

| Sui JSON Type | Drasi `ElementValue` | Notes |
|---------------|---------------------|-------|
| JSON string | `String` | Most Sui numeric values arrive as strings |
| JSON number (integer) | `Integer` (i64) | |
| JSON number (float) | `Float` (f64) | |
| JSON boolean | `Bool` | |
| JSON null | `Null` | |
| JSON array | `List` | Recursively mapped |
| JSON object | `Object` | Recursively mapped into `ElementPropertyMap` |

---

## Bootstrap vs. Streaming Behaviour

| Aspect | Bootstrap Provider | Streaming Source |
|--------|-------------------|-----------------|
| Operation | Always `Insert` | Classified by event name |
| `change_type` | Always `"insert"` | `"insert"`, `"update"`, or `"delete"` |
| Use case | Loading historical events | Real-time change detection |

The bootstrap treats every historical event as an insert because the Drasi query engine needs a baseline graph before it can compute diffs.
