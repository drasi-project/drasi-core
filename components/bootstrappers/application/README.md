# Application bootstrap snapshots

`ApplicationBootstrapProvider` implements the native
`ComputationBootstrapProvider` interface. It supplies application-owned,
in-memory graph data to a query's initial snapshot.

This is a host-linked Rust provider, not a dynamically loaded plugin or a
Server configuration factory. The old count-only `BootstrapProvider` callback
and empty configuration descriptor have been removed.

## Create and attach a provider

```rust,ignore
use std::sync::Arc;
use tokio::sync::RwLock;
use drasi_core::models::SourceChange;
use drasi_bootstrap_application::ApplicationBootstrapProvider;
use drasi_lib::computation::v1::{ContinuousQueryTransformer, StreamId};

let stored = Arc::new(RwLock::new(Vec::<SourceChange>::new()));
// Populate `stored` with the application's authoritative initial graph data.
let bootstrap = Arc::new(
    ApplicationBootstrapProvider::with_shared_data(stored.clone())
        .with_stream(StreamId::try_new("people/bootstrap")?)
        .with_node_labels(vec!["Person".into()])
        .with_relation_labels(vec!["KNOWS".into()]),
);
let query = ContinuousQueryTransformer::new(definition, indexes)
    .await?
    .with_bootstrap(bootstrap);
```

Attach the provider to the query, not to an old Source-plugin bootstrap setter.
Live application-source input does **not** automatically populate or maintain
this data. The application must update or rebuild its snapshot data deliberately.

## Data ownership and configuration

| API | Behavior |
|---|---|
| `new()` / `default()` | Create an empty, isolated data vector |
| `with_shared_data(data)` | Use the application's `Arc<RwLock<Vec<SourceChange>>>` |
| `builder().with_shared_data(data).build()` | Build the same shared-data provider |
| `with_stream(stream)` | Set the snapshot stream; the default is `application/bootstrap` |
| `with_node_labels(labels)` | Include nodes matching any listed label; empty includes all nodes |
| `with_relation_labels(labels)` | Include relations matching any listed label; empty includes all relations |
| `store_insert_event(change)` | Append an insert; other change variants are not stored by this helper |
| `get_stored_events()` | Copy the currently stored changes |
| `clear_stored_events()` | Clear the application's provider data for later snapshots |

The shared vector can contain insert, update and delete changes. Inserts and
updates use their element's node/relation filter; deletes match either selected
label set, or all labels when both filters are empty. Future notifications are
not bootstrap data and are excluded.

There is no automatic deduplication, pruning or capacity limit. Applications own
those policies and must not treat an append-only list of historical inserts as
an automatically maintained current-state snapshot.

## Snapshot and lifecycle contract

Each `snapshot()` captures a stable copy of the current vector. Changes made
afterward affect subsequent snapshots, not the returned one. Encoding is lazy,
one record at a time as the query polls the stream; no writer lock is held during
streaming and no worker or forwarding task is spawned.

Native envelopes have consecutive snapshot-local sequence numbers. Invalid
timestamps or encoding failures remain errors in the stream. Empty data yields
an empty snapshot, not invented records.

The provider does not own live-source progress and returns no fabricated
watermark or source checkpoint. The application must coordinate its snapshot
with the live input when that boundary matters. Data is memory-only and does
not survive process restart unless the application reconstructs it.

Bootstrap initializes the query's result snapshot. It does not emit those rows
as live reaction events. To read an initial result set followed by live changes,
use `DrasiLib::subscribe_query_results`; native application reactions consume
actual subsequent graph output.

## Coverage and examples

The package tests exercise construction, isolated/shared data, filtering,
stable snapshots, error propagation and actual native query initialization.
The integration tests exercise the complete native application pipeline both
standalone and inside DrasiLib, including late query creation with populated
application bootstrap data, on both Tokio runtime flavors.

```bash
cargo test -p drasi-bootstrap-application
cargo test -p lib-integration-tests --test native_application --test native_application_host
```

See [the application source](../../sources/application/README.md),
[the application reaction](../../reactions/application/README.md), and
[ComputationGraph usage](../../../lib/docs/computation-graph-usage.md).

Licensed under the Apache License, Version 2.0.
