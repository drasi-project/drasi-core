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
  "outputs": {"example-query": {"component": "example-query", "port": "out"}},
  "host": "127.0.0.1",
  "port": 8080,
  "ssePath": "/events",
  "heartbeatIntervalMs": 30000,
  "broadcastCapacity": 1024,
  "maxPaths": 1024
}
```

Nothing is required: the graph supplies each incoming output, and optional
`outputs` maps an alias to `{component, port?}`. An unaliased output is named
after its producing component. The alias names routes and snapshot URLs; the
real component and port travel with the data. The default host is `0.0.0.0`; the other defaults
are shown above. Host must be an IP address, the path must be absolute without a
query/fragment, heartbeat must be positive and representable, and capacity must
be in `1..=1048576`. `maxPaths` bounds the total static and dynamically rendered
paths in `1..=65536`. `/snapshot` and its descendants are reserved for snapshots.
Unknown fields reject.

Connect each output — of a source, a transformer or a query — to the
sink's `in` port, using the standard graph-change schema. The input's stream must
be bound by `outputs`; the binding names the data and no query annotation is
read ([sink input contract](../../SINKS.md)). Construction binds no listener or
worker; normal host lifecycle starts and stops the endpoint.

Clients open an `EventSource` at the configured path. A data frame is
`{"output":{"alias","component","port"},"sequence","publishedAt","changes":[…]}`.
Each change states its real `operation` (`ADD`, `REPLACE`, `PATCH`, `DELETE`), its
canonical `identity` (`namespace`, `id`) and only the `element`, `before`,
`after` (a patch's `after` holds only the properties it sets),
`removedProperties` and `effectiveFrom` that the record carries; an unknown
before image is absent, never `null`. `publishedAt` is the sink's own wall clock in
milliseconds, not an event time, and heartbeats are `{"type":"heartbeat","ts"}`.
There is no `queryId` and no row signature. The sink keeps no row cache: patches
and identity-only deletes are transmitted truthfully and the client that owns
row state (`@drasi/react`, see `nativeGraphResultAdapter`) applies them, using
the canonical identity as the row key. An envelope with no changes publishes
nothing. A scheduling control or recovery frame fails.

## Templates and paths

`routes` and `defaultTemplate` reuse the legacy SSE reaction's renderer and
template types from `drasi-reaction-common`, not a separate native implementation:

```json
{
  "outputs": {"alerts": {"component": "warehouse.alerts", "port": "out"}},
  "routes": {
    "alerts": {
      "added": {"template": "{{json after}}", "path": "zones/{{after.zone}}"},
      "updated": {"template": "{{json after}}", "path": "zones/{{after.zone}}"},
      "deleted": {"template": "{{json before}}", "path": "/removed"}
    }
  }
}
```

Full output aliases take precedence over their last dotted segment and the default
template. A route's `updated` entry covers `REPLACE` and `PATCH`. A selected route
with no template for an operation filters that operation, as the legacy SSE
reaction does. With no selected route, one event contains the whole `changes`
array. An explicitly empty body template uses the default per-change frame
`{output, sequence, publishedAt, change}`. The template context is the truthful
change notification: `operation`, `output`, `sequence`, `timestamp` (only when
the envelope has one), `identity` and the `element`, `before`, `after`,
`removedProperties` and `effectiveFrom` the record carries, with the same `json`
helper. Body-render failures report an error and use the
documented per-result fallback; path-render failures report the configured-path
fallback. Invalid rendered paths fail rather than silently route elsewhere.

Relative paths are appended to `ssePath`; absolute paths are used directly.
Static paths are available immediately. A dynamic path becomes available after
its first actual event, so an earlier connection receives 404. Exceeding
`maxPaths` fails before publishing any part of that input. Stop/restart clears
dynamic paths only after the old listener and connections finish cleanup.

## Output snapshots

`GET /snapshot/:alias` streams the current state of the aliased output as a JSON
array of canonical `ADD` changes: each has its real `identity`, `element` shape
and complete `after` image, exactly the change shape a live frame carries, so a
client applies both with one identity-keyed state. It works for a source, a
transformer or a query output port alike, because it reads the output's own
registered `SnapshotProvider` through the graph's output catalogue. The request
names the real `component` and `port` of the alias's binding; the alias is only
the route.

Bind the factory's optional `output_snapshots` dependency to a
`NativeOutputSnapshotsResource`: an **immutable allow-list and limits policy**
(`outputs`: component and optional port, `timeoutMs`, `maxReaders`). It holds no
catalogue. The native factory combines it with the constructing graph's actual
output catalogue, so a snapshot is read from the same graph that built the sink,
and no result cache, processing worker or second graph is created. A selector
without a port allows the component's primary output through the graph's shared
port resolution. A sink output outside the allow-list is rejected.

Server/Host resource recipes use:

```json
{
  "kind": "nativeOutputSnapshots",
  "outputs": [{"component": "warehouse.alerts", "port": "out"}],
  "timeoutMs": 300000,
  "maxReaders": 16
}
```

Declare it with role `OutputCatalogue` (no dependencies) and bind it to the SSE
component's `output_snapshots` slot. The Move-a-wall Server configuration is a
complete example of this wiring.

Responses carry `x-drasi-output-epoch` (the producer's epoch UUID, which changes
when the producer is reconstructed) and `x-drasi-output-position` (its logical
sequence at capture). Outcomes are explicit: an output that registered no
snapshot provider returns `501`, an unknown alias or an output outside the
capability `404`, a producer that is not constructed or ready `503`, a retired
or restarted provider `503`, reader exhaustion `429`; a sink with no snapshot
service bound returns `501`. Errors never become a successful empty result.

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
