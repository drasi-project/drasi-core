# Shared HTTP source protocol

`drasi-http-common` owns `HttpSourceChange`, `HttpElement` and the conversion to
Drasi graph changes used by both ordinary and native HTTP sources. It contains
no HTTP server, component registration or plugin SDK.

The JSON field names, nanosecond wire timestamps, millisecond graph timestamps,
nested values and relation direction retain their existing meaning. The ordinary
`drasi-source-http` package re-exports the same public types and converter, so
existing Rust imports remain valid.

Run `cargo test -p drasi-http-common` from the workspace root.
