# Shared HTTP protocols

`drasi-http-common` owns `HttpSourceChange`, `HttpElement` and the conversion to
Drasi graph changes used by both ordinary and native HTTP sources. Optional
modules contain the extracted webhook, REST-bootstrap and reaction logic,
without a component factory, graph executor or legacy plugin SDK.

The JSON field names, nanosecond wire timestamps, millisecond graph timestamps,
nested values and relation direction retain their existing meaning. The ordinary
`drasi-source-http` package re-exports the same public types and converter, so
existing Rust imports remain valid.

| Feature | Shared logic |
|---|---|
| `webhook` | Authentication, JSON/XML/YAML/text parsing, route/condition matching, mapping, CORS and the published HTTP configuration DTO boundary |
| `adaptive` | Original ingress throughput tiers and bounded telemetry |
| `bootstrap` | Endpoint configuration, auth/OAuth caching, pagination, response mapping and streamed REST snapshot reading |
| `reaction` | HTTP output configuration, wire DTOs, template routing/rendering and retry classification |

The legacy crates re-export their original Rust modules and keep their lifecycle
bindings. Native libraries consume the shared protocol logic directly. The
original helper regression suites move with their implementation.

Run `cargo test -p drasi-http-common --all-features` from the workspace root.
