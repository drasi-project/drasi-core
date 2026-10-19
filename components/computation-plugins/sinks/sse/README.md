# Native browser SSE sink

`drasi-computation-sink-sse` contains the configuration, factory, implementation,
browser serialization and plugin export for **one** component:
`drasi.network/sse-sink` (implementation `"1"`, configuration `1`).

## Build and load

From the `drasi-core` workspace:

```sh
cargo build -p drasi-computation-sink-sse --features dynamic-plugin
```

Load `target/debug/libdrasi_computation_sink_sse.dylib` on macOS,
`libdrasi_computation_sink_sse.so` on Linux, or `drasi_computation_sink_sse.dll`
on Windows. No other native component library is required. This crate does not
require gRPC, protobuf generation or `protoc`. The Rust SDK exposes `plugin()`,
`SseFactory`, `SseSinkConfig` and `sse_payload` from the same crate.
For static hosting, pass `plugin()?` to
`drasi_host_sdk::computation::NativePlugin::from_definition`; it returns the same
validated factories and optional host-service bindings without a library file.

## Configure and connect

```json
{
  "queryStreams": {"example-query": "example-query/out"},
  "host": "127.0.0.1",
  "port": 8080,
  "ssePath": "/events",
  "heartbeatIntervalMs": 30000,
  "broadcastCapacity": 1024,
  "maxPaths": 1024
}
```

Only `queryStreams` is required. The default host is `0.0.0.0`; the other defaults
are shown above. Host must be an IP address, the path must be absolute without a
query/fragment, heartbeat must be positive and representable, and capacity must
be in `1..=1048576`. `maxPaths` bounds the total static and dynamically rendered
paths in `1..=65536`. `/snapshot` and its descendants are reserved for snapshots.
Unknown fields reject.

Connect each query's output to the sink's `in` port, using `QueryChangeCodec`.
The envelope's query ID and stream must match `queryStreams`. Construction binds
no listener or worker; normal host lifecycle starts and stops the endpoint.

Clients open an `EventSource` at the configured path. Data frames preserve
`queryId`, `results`, `timestamp`, row signatures and before/after images.
Heartbeat data frames use the existing browser protocol.

## Templates and paths

`routes` and `defaultTemplate` reuse the legacy SSE reaction's renderer and
template types from `drasi-reaction-common`, not a separate native implementation:

```json
{
  "queryStreams": {"warehouse.alerts": "warehouse.alerts/out"},
  "routes": {
    "alerts": {
      "added": {"template": "{{json after}}", "path": "zones/{{after.zone}}"},
      "updated": {"template": "{{json after}}", "path": "zones/{{after.zone}}"},
      "deleted": {"template": "{{json before}}", "path": "/removed"}
    }
  }
}
```

Full query IDs take precedence over their last dotted segment and the default
template. A selected route with no template for an operation filters that
operation, as the legacy SSE reaction does. With no selected route, one event
contains the whole `results` array. An explicitly empty body template uses the
legacy per-result `result` field instead. The shared context supplies `before`,
`after`, `data`, `query_id`, `query_name`, `operation`, `timestamp` and `metadata`,
with the same `json` helper. Body-render failures report an error and use the
documented per-result fallback; path-render failures report the configured-path
fallback. Invalid rendered paths fail rather than silently route elsewhere.

Relative paths are appended to `ssePath`; absolute paths are used directly.
Static paths are available immediately. A dynamic path becomes available after
its first actual event, so an earlier connection receives 404. Exceeding
`maxPaths` fails before publishing any part of that input. Stop/restart clears
dynamic paths only after the old listener and connections finish cleanup.

## Query snapshots

`GET /snapshot/:query_id` streams the current rows as a JSON array. Bind the
factory's optional `query_snapshots` dependency to a
`NativeQuerySnapshotsResource` over the **same actual `QueryResultsCatalog`**
used by the queries. No query, result cache, processing worker or second graph
is created by this read capability.

Server/Host resource recipes use:

```json
{
  "kind": "nativeQuerySnapshots",
  "catalog": "query-catalog",
  "queries": ["warehouse.alerts"],
  "timeoutMs": 300000,
  "maxReaders": 16
}
```

Declare the resource's dependency on `query-catalog` with role `QueryCatalog`;
bind it to the SSE component's `query_snapshots` slot. The query itself binds
that original catalogue. The Move-a-wall Server configuration is a complete
example of this wiring.

The host enforces allowed query IDs, bounded reader counts and readiness
timeouts. Snapshot operations execute under the owning host runtime even when
called from a dynamically loaded library. Responses include
`x-drasi-query-sequence` and `x-drasi-query-generation`. Missing binding or
unavailable data returns 503, unknown query IDs return 404, and reader exhaustion
returns 429; errors never become a successful empty result.

Dropping a response cancels/releases its reader. Stop cancels pending captures
and streams and retains ownership until cleanup completes. This endpoint is a
point-in-time read, not an atomic snapshot/live subscription protocol. The
Server's snapshot-first subscription API remains available for that separate
contract. Incoming recovery snapshot/progress control envelopes remain
unsupported by this volatile notification sink.

## Delivery and lifecycle

Completion is **volatile `Accepted`**, never browser-acknowledged handling.
No listener is required and disconnected clients receive no history. A lagging
listener is disconnected rather than silently receiving a truncated tail;
reconnect and fetch a fresh snapshot.

Stop cancels active streams and joins the owned HTTP server. Cancelled or
timed-out cleanup retains ownership and must finish before restart. GET/OPTIONS
allow cross-origin reads; use an authenticated reverse proxy for private results.
This sink does not add persistence, replay or exactly-once delivery.
