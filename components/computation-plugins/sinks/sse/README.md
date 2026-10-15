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

## Configure and connect

```json
{
  "queryStreams": {"example-query": "example-query/out"},
  "host": "127.0.0.1",
  "port": 8080,
  "ssePath": "/events",
  "heartbeatIntervalMs": 30000,
  "broadcastCapacity": 1024
}
```

Only `queryStreams` is required. The default host is `0.0.0.0`; the other defaults
are shown above. Host must be an IP address, the path must be absolute without a
query/fragment, heartbeat must be positive and representable, and capacity must
be in `1..=1048576`. Unknown fields reject.

Connect each query's output to the sink's `in` port, using `QueryChangeCodec`.
The envelope's query ID and stream must match `queryStreams`. Construction binds
no listener or worker; normal host lifecycle starts and stops the endpoint.

Clients open an `EventSource` at the configured path. Data frames preserve
`queryId`, `results`, `timestamp`, row signatures and before/after images.
Heartbeat data frames use the existing browser protocol. Query snapshots come
from the Server query-results API; this component has no second result cache or
snapshot endpoint. Templates, dynamic paths and recovery snapshot/progress
control envelopes are unsupported and reject explicitly.

## Delivery and lifecycle

Completion is **volatile `Accepted`**, never browser-acknowledged handling.
No listener is required and disconnected clients receive no history. A lagging
listener is disconnected rather than silently receiving a truncated tail;
reconnect and fetch a fresh snapshot.

Stop cancels active streams and joins the owned HTTP server. Cancelled or
timed-out cleanup retains ownership and must finish before restart. GET/OPTIONS
allow cross-origin reads; use an authenticated reverse proxy for private results.
This sink does not add persistence, replay or exactly-once delivery.
