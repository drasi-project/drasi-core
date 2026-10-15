# Native network shared helpers

This crate supplies configuration validation, HTTP/gRPC source admission and
listener ownership, and query-output wire projection. It contains no component
implementation, plugin export or registered factory.

The default feature set has no HTTP/gRPC component or protocol dependencies.
`http-source`, `grpc-source`, `http-sink` and `grpc-sink` select the relevant
transport helpers. Each component selects its own requirements; an SSE-only
build does not enable any of these features.

Applications should depend on the [individual component crate](../../README.md),
not assemble a component from shared modules. See the
[network protocol guide](../../NETWORK.md) for admission and delivery contracts.
