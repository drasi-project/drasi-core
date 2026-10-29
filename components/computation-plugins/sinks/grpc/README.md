# Native gRPC sink

`GrpcSinkConfig`, `OutputFormat` and their validation live in this crate's
`config` module. Graph changes are projected to protocol items with the shared
reaction-protocol renderer; see the [sink input contract](../../SINKS.md).

Published configuration metadata includes the actual defaults and allowed values.
`batchSize` supports 1 through 65,536 items. `batchFlushTimeoutMs` is an effective
host collection deadline for multi-item batches, bounded to 60 seconds.

`drasi-computation-sink-grpc` provides only `drasi.network/grpc-sink`
(implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-sink-grpc --features dynamic-plugin
```

Its `in` port consumes standard graph-change envelopes from any producer: a
source, a transformer or a query. The graph supplies each incoming output;
optional `outputs` (alias → `{component, port?}`) names them. Defaults include
`endpoint="grpc://localhost:50052"`, `timeoutMs=60000`, `maxRetries=5`,
`connectionRetryAttempts=10`, `initialConnectionTimeoutMs=15000`, `batchSize=1`,
`batchFlushTimeoutMs=100`, `outputFormat="canonicalJson"`,
`failurePolicy="strict"`. Every RPC carries `x-drasi-component` and `x-drasi-port` metadata naming the
real producing output; configuring either rejects. The sink calls the
`drasi.v1.ChangeService/ProcessChanges` RPC with ordered `Change` items; the legacy
query-result `ReactionService` is not used.

Start establishes the actual connection; exhausted failure is not lazy startup
success. `Handled` requires both receiver `success=true` and an exact
`items_processed` count, or an explicit `"skip"` policy that logs skipped failure,
not a transactional remote effect. See the
[configuration and completion contract](../../NETWORK.md#grpc-sink-configuration).
No gRPC source or HTTP/SSE sink component is loaded.

## Fixed and adaptive batches

Use the flat `batchSize`/`batchFlushTimeoutMs` options or the published `batching`
object, not both:

```json
{
  "outputs": {"rooms": {"component": "rooms", "port": "out"}},
  "endpoint": "grpc://localhost:50052",
  "batching": {
    "mode": "adaptive",
    "adaptiveMinBatchSize": 1,
    "adaptiveMaxBatchSize": 100,
    "adaptiveWindowSize": 10,
    "adaptiveBatchTimeoutMs": 100
  }
}
```

Fixed mode accepts `{"mode":"fixed","batchSize":100,"batchFlushTimeoutMs":100}`.
The default single-item path does not acquire batching state. Other modes use
host-owned bounded collection and timers: no input is acknowledged merely for
entering a private queue. Retained/QoS receivers keep partial collections and
their pending leases across cancellation; acknowledgements remain contiguous.

Each RPC contains one output (alias, component, port) and one set of request metadata. Output/metadata
changes split requests without reordering items. `maxBatchBytes` (16 MiB by
default) bounds host collection, and `maxRequestBytes` (4 MiB by default) splits
protobuf requests at their encoded-byte limit. An individually oversized item
fails explicitly. Every chunk must finish before its graph inputs complete.

## Shared output and recovery logic

`outputTemplates.defaultTemplate` and per-alias `routes` reuse the legacy
renderer, including full-name/last-segment/default routing, `json`, metadata
templates and logged canonical fallbacks. Templates populate `payload`; typed
before/after images, sequences and timestamps are not rewritten. There is no
`row_signature`: each item states its `operation` (`ADD`, `REPLACE`, `PATCH`,
`DELETE`), canonical `identity`, optional `element` shape, optional typed
`before`/`after` (a patch's `after` holds only the properties it sets) and a
patch's `removed_properties`. `timestamp` is set only when the envelope carries one.
`outputFormat: "proto"` omits untemplated canonical payloads.

The native path reuses the legacy connection-error classification and batch
metadata rules. Reconnection and retries stay inside the awaited operation,
with bounded attempts; they are not deferred to an untracked worker or a later
input. A partial, zero or excessive success count is an error, not completion.
Lost replies may repeat effects on replay. This protocol has no atomic remote
effect/receipt transaction and does not promise exactly-once execution.
