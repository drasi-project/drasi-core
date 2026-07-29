# Drasi State Machine Source

A Drasi **source** that maps live continuous-query results to per-entity **state
transitions** and exposes the realtime state of every entity as a Drasi source
that downstream queries can subscribe to.

Unlike an ordinary source that ingests data from an external system, the state
machine source is driven by the results of *other continuous queries*. It uses
the source query-subscription API — the same mechanism reactions use to consume
query results.

## How it works

```text
   input queries                 state machine source
 ┌───────────────┐      ┌──────────────────────────────────┐
 │ draft-orders  │─────▶│  subscribes to queries             │
 │ paid-orders   │─────▶│  computes transitions              │──▶ downstream queries
 │ shipped-orders│─────▶│  persists + dispatches node state  │
 └───────────────┘      └──────────────────────────────────┘
                                    │
                                    └── state store (durable)
```

* The source **subscribes** to the input queries referenced by its states. Each
  *enter condition* declares:
  * `query` — the input query whose results drive the transition,
  * `ops` — the result operations that trigger it (`added` / `updated` / `deleted`),
  * `previous` — the allowed prior states (`[]` = initial entry, `["*"]` = any),
  * `key` — a Handlebars template (e.g. `{{orderId}}`) that extracts the entity key.

  When a result matches and the entity is in an allowed prior state, the entity
  transitions. The new state is persisted to the state store and dispatched as a
  graph node change to the source's own subscribers.

* Each emitted node carries:
  * `element_id` = the entity key,
  * label = `entityLabel` (default `EntityState`),
  * properties = the key field, `state`, `previousState`, `enteredAt`, plus the
    pass-through fields from the triggering query result row.

  Late-joining and downstream subscribers are bootstrapped from the durably
  persisted state, so the source survives restarts.

## IDs

The source's own `id` is the id downstream queries subscribe to. There is **no**
separate companion component — the single source both consumes upstream query
results and produces the entity-state graph.

## Usage (static / embedded)

```rust
use drasi_source_state_machine::{StateMachineSourceBuilder, config::{StateDef, EnterCondition, Op}};

let source = StateMachineSourceBuilder::new("order-state-source")
    .with_entity_label("OrderState")
    .with_key_field("orderId")
    .with_state(StateDef {
        id: "NEW".to_string(),
        enter: vec![EnterCondition {
            query: "draft-orders".to_string(),
            previous: vec![],
            key: "{{orderId}}".to_string(),
            ops: vec![Op::Added],
        }],
    })
    // ... more states ...
    .build()?;

// drasi.add_source(source).await?;
# Ok::<(), anyhow::Error>(())
```

A **durable** `StateStoreProvider` (e.g. `drasi-state-store-redb`) should be
configured on the `DrasiLib` instance so entity state survives restarts.

## Configuration (declarative / YAML)

The crate exports a `state-machine` **source** descriptor (behind the
`dynamic-plugin` feature). Declare the source and let downstream queries read
from its id:

```yaml
sources:
  - kind: state-machine
    id: order-state-source
    properties:
      entityLabel: OrderState
      keyField: orderId
      states:
        - id: NEW
          enter:
            - query: draft-orders
              previous: []
              key: "{{orderId}}"
              ops: [added]
        - id: CONFIRMED
          enter:
            - query: confirmed-orders
              previous: [NEW]
              key: "{{orderId}}"
              ops: [added]
        # ... PAID, PICKED, SHIPPED, DELIVERED ...
```

## Example

See [`examples/lib/orders-state-machine`](../../../examples/lib/orders-state-machine)
for a complete, self-driving order-lifecycle demo using a Postgres source,
a stored-procedure reaction to advance orders, and the dashboard reaction to
visualize the live state.
