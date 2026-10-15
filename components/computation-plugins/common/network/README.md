# Native network shared helpers

This crate supplies shared validation, HTTP/gRPC source admission and
listener ownership, and query-output wire projection. It contains no component
implementation, plugin export or registered factory.

Component settings, defaults and protocol-specific validation live in each
component's own `config` module. `IngressConfig` contains only the connection,
message-size and admission values shared by both source runtimes; it does not
select HTTP or gRPC. HTTP batching and gRPC output-format settings are not part
of this shared contract.

The default feature set has no HTTP/gRPC component or protocol dependencies.
`http-source`, `grpc-source`, `http-sink` and `grpc-sink` select the relevant
transport helpers. Each component selects its own requirements; an SSE-only
build does not enable any of these features.

HTTP message types and conversion come from `drasi-http-common`; gRPC protocol
types and value conversion come from `drasi-grpc-common`. None of these helpers
depends on an ordinary source or reaction implementation.

Applications should depend on the [individual component crate](../../README.md),
not assemble a component from shared modules. See the
[network protocol guide](../../NETWORK.md) for admission and delivery contracts.

Code that previously imported component settings from this helper crate should
import `HttpSourceConfig`, `GrpcSourceConfig`, `HttpSinkConfig` or
`GrpcSinkConfig` from the corresponding component crate. The source crates
retain the `SourceConfig` name and deprecated protocol-flag `parse` entry point;
new code uses the explicitly named type's `from_value` method.
