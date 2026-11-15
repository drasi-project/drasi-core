# Native network shared helpers

This crate holds the code shared by the native HTTP and gRPC components. It
contains no component implementation, plugin export or registered factory.

| Module | Feature | Contents |
|---|---|---|
| `config` | always (ingress items need `ingress`) | `IngressConfig`, shared security settings (`SecuritySettings`, `IngressSecurity`, `SecretString`, `TlsMaterial`, `RateLimit`), secure defaults, `ingress_options` field descriptions, sink helpers |
| `security` | `ingress` | Constant-time bearer-token `Authenticator`, per-client `RateLimiter`, TLS server configuration (rustls, ring) |
| `ingress` | `ingress` | `SourceRuntime` (listener ownership, Start with cancellation and deadline, Pause gating, Drain/Abort Stop, live settings) and `Ingress` (access control, request slots, volatile admission, typed `IngressError`) |
| `durable` | `ingress` | Durable admission: the session protocol, and the server-sequenced producer used by standard routes |
| `update` | `ingress` | `BoundaryPlanner` for classifying configuration changes |
| `wire` | protocol features | HTTP and gRPC change conversion, and sink output projection |
| `batching` | sink features | Adaptive sink batching |
| `diag!` | always | Logging through `log` when the host installed a logger, otherwise warnings and errors to standard error |

Each component owns its configuration type, defaults and validation, and builds
an `IngressConfig` from it. `http-source`, `grpc-source`, `http-sink` and
`grpc-sink` select the transport helpers. The default feature set has no
HTTP or gRPC dependencies.

HTTP message types and conversion come from `drasi-http-common`; gRPC protocol
types come from `drasi-grpc-common`. None of these helpers depend on an
ordinary source or reaction implementation.

Applications should depend on the [individual component crates](../../README.md).
For the shared admission and delivery contracts, see the
[network protocol guide](../../NETWORK.md); for what a production native source
must implement, see the [native source standard](../../sources/NATIVE-SOURCE-GUIDE.md).
