# Orders State Machine Example

A self-driving order-lifecycle demo built on the Drasi **state machine source**.
It ties four components together and is verified end-to-end (orders animate all
the way from NEW to DELIVERED on a live dashboard).

| Component | Role |
|-----------|------|
| **Postgres source** (`orders-db`) | Streams changes to `orders`, `payments`, `stock_picks`, `shipments` via logical replication, with a Postgres bootstrap provider for the initial snapshot. |
| **6 stage queries** | `draft-orders`, `confirmed-orders`, `paid-orders`, `picked-orders`, `shipped-orders`, `delivered-orders` — each a single-entity query returning the owning `order_id`. |
| **State machine source** (`order-state-source`) | Subscribes to the stage queries and correlates them by `order_id` into the states `NEW → CONFIRMED → PAID → PICKED → SHIPPED → DELIVERED`, exposing each order's live state as its own source. |
| **Stored-procedure reaction** (`order-advancer`) | When an order enters a stage, calls `schedule_advance(orderId)` to schedule the next step. |
| **Dashboard reaction** | Visualizes the live order states on a web UI at `:3002`. |

```text
Postgres (orders, payments,   ──▶ orders-db ──▶ 6 stage queries ──┬──▶ state machine source (order-state-source) ──▶ dashboard queries ──▶ dashboard (:3002)
          stock_picks, shipments)                                  └──▶ stored-proc reaction ──▶ CALL schedule_advance(...)
       ▲                                                                                    │
       └──────────────────── pacing driver: SELECT advance_due_orders() ◀───────────────────┘
```

## The state machine is a *source*

The state machine is a single Drasi **source**. Unlike an ordinary source that
ingests data from an external system, it is driven by the results of the six
stage queries: it uses the source query-subscription API (the same mechanism
reactions use to consume query results). It correlates those results into a
per-order state, persists that state, and re-emits it as graph nodes — so its
own id (`order-state-source`) is what the dashboard queries subscribe to. There
is no separate companion component.

## Entities (mirrors the classic Order / Payment / StockPick / Shipment model)

| Stage     | Relational fact                          | Stage query |
|-----------|------------------------------------------|-------------|
| NEW       | `orders` row with `is_draft = 1`         | `MATCH (o:orders) WHERE o.is_draft = 1 RETURN o.id AS orderId, ...` |
| CONFIRMED | `orders` row with `is_draft = 0`         | `MATCH (o:orders) WHERE o.is_draft = 0 RETURN o.id AS orderId` |
| PAID      | settled `payments` row                   | `MATCH (p:payments) WHERE p.status = 'settled' RETURN p.order_id AS orderId` |
| PICKED    | `stock_picks` row                        | `MATCH (s:stock_picks) RETURN s.order_id AS orderId` |
| SHIPPED   | `shipments` row                          | `MATCH (h:shipments) RETURN h.order_id AS orderId` |
| DELIVERED | `shipments` row with `delivered = 1`     | `MATCH (h:shipments) WHERE h.delivered = 1 RETURN h.order_id AS orderId` |

The stages are **separate entities/tables**, exactly like the original relational
model. Because drasi-core's Postgres source produces nodes (not foreign-key
relationships), each stage query returns the owning `order_id` and the **state
machine does the correlation by key** — which is precisely what it is for. Most
advancement is INSERT-driven (settled payments, stock picks, shipments), which the
source delivers as clean node additions.

## How the self-driving loop works

1. A new order is inserted (`is_draft = 1`). `draft-orders` matches → the state
   machine sets it **NEW**; the stored-proc reaction schedules an advancement
   (`orders.next_stage_at = now() + 1s`).
2. The **pacing driver** (a 2 Hz loop in `main.rs`) calls `advance_due_orders()`,
   which advances **one** due order by one stage (flip `is_draft`, or insert a
   payment / stock pick / shipment, or set `delivered`), touching the order row at
   most once so each CDC transaction carries a single clean change.
3. That change flows back through Postgres CDC → the next stage query matches →
   the state machine transitions the order and the stored-proc reaction schedules
   the next step.
4. Steps 2–3 repeat until the order is **DELIVERED** (~1–2 s per stage). A seeder
   task inserts a fresh order every 8 seconds, keeping only a couple of orders in
   flight at once.

## Durability

The state machine source persists each order's state to a durable
[`redb`](../../../components/state_stores/redb) state store at `./data/state.redb`.
On restart, state is reloaded and downstream subscribers are bootstrapped from it.

## Running

Requires Docker (for Postgres) and Rust.

```bash
cd examples/lib/orders-state-machine
./run.sh
```

`run.sh` starts Postgres with logical replication, waits for it to be healthy, then
runs the app. Open <http://localhost:3002> (or <http://127.0.0.1:3002> if your
browser resolves `localhost` to IPv6 — the server binds IPv4) to watch orders
progress through their lifecycle.

To run the steps manually:

```bash
docker compose up -d          # Postgres on host port :5442 (container 5432)
cargo run                     # Drasi + dashboard on :3002
docker compose down -v        # tear down the database when finished
```

## Files

| File | Description |
|------|-------------|
| `main.rs` | Wires all components and the pacing/seeder tasks. |
| `db/init.sql` | Orders/payments/stock_picks/shipments schema, advancement stored procedures, the `drasi_pub` publication (no seed rows). |
| `docker-compose.yml` | Postgres 16 with `wal_level=logical`. |
| `run.sh` | Convenience launcher. |

## Notes

- **The state machine is configured declaratively.** In `main.rs` the source is
  defined as literal YAML (the `ORDER_STATE_MACHINE` constant) and deserialized into
  the config — exactly the shape you would put in a Drasi YAML file (`entityLabel`,
  `keyField`, and a `states` list with `enter` conditions). Note there is no
  `sourceId`: the state machine is a single source, and its own id is what
  downstream queries read.
- **Integer flags, not booleans.** `orders.is_draft` and `shipments.delivered` are
  modelled as `SMALLINT` (0/1) rather than `BOOLEAN`. The Drasi Postgres CDC source
  currently mis-decodes streamed `BOOLEAN` values (they always decode as `true`),
  so a boolean flag flip would not be observed on the change stream. Integers stream
  correctly. Tracked upstream in
  [drasi-project/drasi-core#611](https://github.com/drasi-project/drasi-core/issues/611);
  once fixed, the flags can be plain booleans.
- **One change per advancement.** `advance_due_orders()` advances a single order by
  a single stage per call and touches each row at most once, so every CDC
  transaction carries one clean change. This keeps the continuous queries reliable
  under the self-driving load (batched multi-row updates in one transaction can be
  collapsed on the stream).
- **No seed data / start with a fresh DB.** The seeder inserts orders live so they
  animate through the full lifecycle. Rows that exist before startup arrive via
  bootstrap, which the stage queries observe, but the self-driving loop only
  advances orders whose transitions flow through the live change stream. Use
  `docker compose down -v` between runs to reset.
- The Drasi Postgres source auto-creates its replication slot, so `init.sql`
  deliberately does **not** pre-create one (doing so would replay seed rows on top
  of the bootstrap snapshot and double the data).
