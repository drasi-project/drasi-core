# Native network shared helpers

This crate holds the code shared by the native HTTP and gRPC components. It
contains no component implementation, plugin export or registered factory.

| Module | Feature | Contents |
|---|---|---|
| `config` | always (items gated by feature) | `SecretString` (always); listener security settings (`SecuritySettings`, `IngressSecurity`, `TlsMaterial`, `RateLimit`) and secure defaults (`listener`); `IngressConfig` and `ingress_options` (`ingress`); sink helpers: `FailurePolicy`, reserved headers, `delivery_id`, `ClientTls`, `sanitize` (sink features) |
| `security` | `listener` | Constant-time bearer-token `Authenticator`, per-client `RateLimiter`, TLS server configuration (rustls, ring) |
| `server` | `listener` | The shared HTTP listener: TLS, connection cap, header-read timeout, graceful drain; used by the HTTP source and the SSE sink |
| `ingress` | `ingress` | `SourceRuntime` (listener ownership, Start with cancellation and deadline, Pause gating, Drain/Abort Stop, live settings) and `Ingress` (access control, request slots, volatile admission, typed `IngressError`) |
| `durable` | `ingress` | Durable admission: the session protocol, and the server-sequenced producer used by standard routes |
| `update` | always | `BoundaryPlanner` for classifying configuration changes, `ensure_same_component`, `inputs_changed` |
| `retry` | `sink` | Sink delivery retries: jittered exponential backoff, `Retry-After`, a total time budget |
| `wire` | protocol features | HTTP and gRPC change conversion, and sink output projection |
| `batching` | sink features | Adaptive sink batching |
| `diag!` | always | Logging through `log` when the host installed a logger, otherwise warnings and errors to standard error |

Each component owns its configuration type, defaults and validation, and builds
an `IngressConfig` from it. `http-source`, `grpc-source`, `http-sink` and
`grpc-sink` select the transport helpers; `listener` provides the shared
server for components that serve clients. The default feature set has no
HTTP or gRPC dependencies.

HTTP message types and conversion come from `drasi-http-common`; gRPC protocol
types come from `drasi-grpc-common`. None of these helpers depend on an
ordinary source or reaction implementation.

Applications should depend on the [individual component crates](../../README.md).
For the shared admission and delivery contracts, see the
[network protocol guide](../../NETWORK.md); for what production native components
must implement, see the [native source standard](../../sources/NATIVE-SOURCE-GUIDE.md)
and the [native sink standard](../../sinks/NATIVE-SINK-GUIDE.md).
