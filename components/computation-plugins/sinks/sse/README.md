# Native browser SSE sink

`drasi-computation-sink-sse` contains the configuration, factory, implementation
and plugin export for **one** component: `drasi.network/sse-sink`
(implementation `"1"`, configuration `1`, computation ABI 0.1).

It serves graph changes from any output (source, transformer or query) to
browsers over Server-Sent Events, and can serve a point-in-time snapshot of a
bound output at `GET /snapshot/:alias`. Delivery is volatile (`Accepted`):
subscribers receive only what is broadcast while they are connected.

- [DESIGN.md](DESIGN.md): broadcast model, delivery profile, snapshot
  handover, pause and stop semantics, management, security model.
- [CONFIGURATION.md](CONFIGURATION.md): every field with default, range,
  secrecy and update boundary; templates; exposure guidance; legacy mapping.

## Build and load

```sh
cargo build -p drasi-computation-sink-sse --features dynamic-plugin
```

Load `libdrasi_computation_sink_sse.dylib` (macOS), `.so` (Linux) or
`drasi_computation_sink_sse.dll` (Windows) from `target/debug`. No other
native library, gRPC or `protoc` is required. For static hosting, pass
`plugin()?` to `drasi_host_sdk::computation::NativePlugin::from_definition`.
The crate also exposes `SseFactory`, `SseSinkConfig` and `sse_payload`.

## Quick start

```json
{"outputs": {"alerts": {"component": "warehouse.alerts", "port": "out"}}}
```

This listens on `127.0.0.1:8080` with no authentication and no CORS headers;
open `new EventSource("http://127.0.0.1:8080/events")`. Exposing the sink on
another address requires `authToken` (or `allowUnauthenticated: true` behind
an authenticating proxy); see [CONFIGURATION.md](CONFIGURATION.md#exposure).
