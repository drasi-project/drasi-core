# Native network shared helpers

This crate holds the code shared by the native HTTP, gRPC and OpenTelemetry components. It
contains no component implementation, plugin export or registered factory.

| Module | Feature | Contents |
|---|---|---|
| `config` | always (items gated by feature) | `SecretString` (always); listener security settings (`SecuritySettings`, `IngressSecurity`, `TlsMaterial`, `RateLimit`) and secure defaults (`listener`); `IngressConfig` and `ingress_options` (`ingress`); sink helpers: reserved headers, `delivery_id`, `ClientTls`, `sanitize` (sink features) |
| `security` | `listener` | Constant-time bearer-token `Authenticator`, per-client `RateLimiter`, TLS server configuration (rustls, ring) |
| `server` | `listener` | The shared HTTP listener: TLS, connection cap, header-read timeout, graceful drain; used by the HTTP source and the SSE sink |
| `grpc_incoming` | `grpc-listener` | The connection-limited, TLS-capable incoming stream for tonic servers (HTTP/2 preface deadline, concurrent handshakes); used by the gRPC and OTel sources |
| `ingress` | `ingress` | `SourceRuntime` (listener ownership, Start with cancellation and deadline, one or several listeners per activation (`start_listeners`), Pause gating of data admission, Drain/Abort Stop, live settings) and `Ingress` (access control, request slots, volatile admission, all-or-nothing volatile group admission (`admit_all`), typed `IngressError`). Drain stops accepting but lets in-flight admissions finish and answer; Abort interrupts them as uncertain |
| `durable` | `ingress` | Durable admission with back-pressure (`admit_wait`): the session protocol, and the server-sequenced producer used by standard routes. Batches are not atomic; the committed prefix is always reported |
| `update` | always | `BoundaryPlanner` for classifying configuration changes, `ensure_same_component`, `inputs_changed` |
| `retry` | `sink` | Sink delivery retries: jittered exponential backoff, `Retry-After`, a total time budget; `InputOutcome` applies a sink's `onPermanentFailure` to the changes of one input |
| `wire` | protocol features | HTTP and gRPC change conversion, and sink output projection |
| `batching` | sink and `ingress` features | The one adaptive batching configuration (`AdaptiveBatchSettings`, the published `adaptive` object), its validator and the sink host-batching policy |
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
