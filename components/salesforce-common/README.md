# drasi-salesforce-common

Shared Salesforce primitives used by the Salesforce source and bootstrap crates.

## What this crate provides

- OAuth/token auth helpers (`auth`)
- Salesforce REST query/describe client (`rest`)
- Salesforce Pub/Sub gRPC client (`pubsub`)
- Native CDC payload decoding + canonical mapping (`mapping`, `projection`)
- Metadata-driven catalog + graph projection (`catalog`)
- Pure record-delta graph changes (`graph_changes`)
- Durable replay position encoding (`position`)
- Snapshot v2 ticket/manifest + framed `GraphRecord` codec (`snapshot`)

## Metadata-driven graph contract

Public API:

- `Catalog::from_describes(Vec<Describe>) -> Result<Catalog>`
- `Catalog::record(&self, object: &str, record: Record) -> Result<GraphRecord>`
- `graph_changes(source_id, org_id, previous, next, deleted_at_ms) -> Result<Vec<SourceChange>>`

`Catalog` carries:

- `pub plans: Vec<ObjectPlan>`
- `pub schema: DiscoveredSchema`

`DiscoveredSchema` exposes deterministic `fingerprint` and object-level schema details,
including explicit lookup and omission reporting.

`GraphRecord` carries node properties plus owned outgoing lookup relationships.
`graph_changes` enforces:

- relation deletions first (including reparent/null)
- node insert/update next (removed properties emitted as explicit `Null`)
- retained relation updates when owner `observed_at_ms` advances (owner-observation-time semantics)
- relation inserts last
- owner deletion removes relations before node delete
- records unchanged in properties, relationships, and owner observation time emit no changes

## Transport contract

`SalesforcePubSubClient::get_topic` returns typed `TopicUnavailable` when
Salesforce reports `can_subscribe = false`. gRPC status errors are preserved for
all other failures.

## Snapshot v2 contract

- Context property key: `salesforce.bootstrap_ticket.v2`
- Ticket/manifest version: `2`
- Ticket and manifest object scope: `objects: Vec<String>`
- Rows file format: framed `GraphRecord` stream (`u32` BE length + msgpack payload)
- Hard file-size cap: `MAX_SNAPSHOT_FILE_BYTES = 512 MiB`

Shared codec helpers:

- `write_snapshot_record(&mut W, &GraphRecord, max_record_bytes)`
- `read_snapshot_record(&mut R, max_record_bytes)`

`read_snapshot_record` only returns clean EOF at frame boundaries; partial header/body is an error.

## Protocol and behavior tests

The crate includes:

- mapping/property contract tests
- local gRPC Pub/Sub protocol harness test
- local HTTP REST protocol harness test
- typed `TopicUnavailable` transport regression
- metadata/catalog + graph delta regressions
- continuous-query executable behavior gate for relation-before-target,
  target delete/reinsert, reparent/null, owner deletion, and sparse property removal

Run:

```bash
cargo test -p drasi-salesforce-common
```
