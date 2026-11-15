# Native gRPC source: design

`drasi.network/grpc-source` turns gRPC calls into graph changes. This document
describes how it works. [CONFIGURATION.md](CONFIGURATION.md) lists every
setting and how to operate the source.

## Purpose and scope

The source serves two gRPC services on one listener:

- **`drasi.v1.SourceService`**, the published source protocol from
  `drasi-grpc-common`:
  - `SubmitEvent` (unary);
  - `StreamEvents` (bidirectional, with cumulative accepted counts);
  - `HealthCheck`;
  - `RequestBootstrap`, which returns `UNIMPLEMENTED`. Configure a bootstrap
    provider on the query.
- **`drasi.admission.v1.AdmissionService`**
  ([`proto/admission.proto`](proto/admission.proto)), the durable
  producer-session protocol. It covers registration, status, retirement,
  receipt lookup, unary `Admit` and streaming `StreamEvents`, and reuses the
  shared source event message.

Each change is emitted on the `out` port as a separate `GraphChangeCodec`
envelope.

## Architecture

```
 client ─► TCP accept ─► connection cap (incoming.rs) ─► [TLS] ─► tonic HTTP/2
              │
              ▼
        Access interceptor: per-client rate limit ─► bearer token   (every RPC)
              │
              ├─ SourceService ─────┐
              └─ AdmissionService ──┤  unary: request slot (ingressCapacity)
                                    │  stream: stream slot (maxConcurrentStreams),
                                    │          idle timeout, rate limit per message
                                    ▼
                    Ingress (common/network): pause gate ─► admission
                        ├─ volatile: bounded queue ─► next() ─► graph
                        └─ durable: host QoS admission service ─► graph
```

| Piece | Location | Responsibility |
|---|---|---|
| `GrpcSource` | `src/component.rs` | Lifecycle, management, `SourceService`, interceptor, server assembly |
| Admission service | `src/durable_grpc.rs` | `AdmissionService` handlers and protobuf mapping |
| Incoming | `src/incoming.rs` | Connection-limited accept stream; accept-error backoff |
| `SourceRuntime` / `Ingress` | `common/network/src/ingress.rs` | Listener ownership, pause gating, volatile queue, Stop modes, live settings |
| Durable admission | `common/network/src/durable.rs` | Session protocol and server-sequenced producer |
| Security | `common/network/src/security.rs` | Bearer tokens, rate limiting, TLS material validation |

## Admission modes

The admission modes match the HTTP source.

**Volatile** (no admission binding). Accepted events go into a bounded queue
(`ingressCapacity`) and reach the graph through `next()`. A success response
means *accepted into the queue*.

**Durable** (admission bound to a persistent QoS channel).

- `SourceService` calls use the **server-sequenced producer**:
  - the producer is named with the source's own component UUID;
  - writers are serialized, so sequences stay consecutive;
  - a call is acknowledged only after commit;
  - after any uncertain outcome the source re-reads the committed head before
    admitting again;
  - delivery is at-least-once.
- `AdmissionService` provides **idempotent client sequencing**. Each streamed
  request carries its session and sequence, and each response is a receipt or a
  typed error. A failed event ends the stream after its error response.

## Streaming

- `SourceService.StreamEvents` replies to every accepted event with
  `success=true` and the **cumulative** `events_processed` total. Clients keep
  the latest total; they do not sum the counts.
  - Invalid input gets one failed response with the accepted prefix count, and
    the stream closes.
  - Admission failures end the stream with a gRPC error.
- Each stream holds one of `maxConcurrentStreams` slots for its lifetime.
  Streams therefore never consume the unary request slots, and open streams
  cannot starve unary calls.
- A stream that sends nothing for `streamIdleTimeoutMs` is closed with
  `DEADLINE_EXCEEDED`.
- Every streamed message is charged against the client's rate limit.
- Pause ends open admission streams at their next message, with `UNAVAILABLE`.
- Stop ends them promptly.

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | No external prerequisites; returns immediately. |
| `start` | Binds the listener, observing the hook's cancellation and deadline, then serves both services. A second Start while running is refused. |
| `pause` | Sets the pause gate. New unary admissions and new streams get `UNAVAILABLE`, and open streams end at their next message. Pause returns once in-flight admissions settle. If cancelled or past its deadline, it reopens admission. `HealthCheck` reports `DEGRADED`. |
| `resume` | Reopens admission. |
| `stop` | `Drain`: stops accepting connections, ends streams, and lets in-flight unary calls finish (tonic graceful shutdown). `Abort`: also aborts the server task and its connections. The listener is joined within the hook's deadline (or `timeoutMs + 5s`). On expiry the error is retryable and ownership is kept. |
| `dispose` | Owns no persistent state. `Preserve` and `Erase` are idempotent no-ops. Refused while running. |

## Management

Updates are planned by `component::plan`:

- **Pause:** tokens, rate limits, `allowUnauthenticated`,
  `streamIdleTimeoutMs`. These are read per call or per stream.
- **Stop:** everything bound into the tonic server at start: address, port,
  `timeoutMs`, `maxMessageBytes`, capacities and TLS.
- **Replace:** `stream`.

Apply re-checks the current configuration, the plan and the exposure rule
before changing anything.

## Security model

- **Exposure:** the listener binds `127.0.0.1` by default. A non-loopback
  listener requires `authToken` unless `allowUnauthenticated` is set. Every RPC
  admits data or producer metadata, so the token protects all of them,
  including `HealthCheck`.
- **Interceptor:** each RPC is charged against the per-client-IP rate limit,
  then checked for `authorization: Bearer <token>` metadata in constant time.
  The rate limit applies first, so floods of bad credentials are limited too.
- **TLS:** tonic with the ring provider. An optional client CA requires client
  certificates (mutual TLS).
- **Resource bounds:**
  - open connections (`maxConnections`); excess connections close at once, and
    accept errors back off for 50 ms;
  - unary admissions in flight (`ingressCapacity`);
  - open streams (`maxConcurrentStreams`);
  - decoded message size (`maxMessageBytes`);
  - the stream idle timeout.
  - HTTP/2 keepalive pings every 30 s detect dead peers.
- **Deadline ordering:** the tonic transport deadline is set above the admission
  deadline (`timeoutMs + max(1 s, timeoutMs / 4)`). An admission timeout is
  therefore always reported with its typed uncertainty, never as a bare
  transport `DEADLINE_EXCEEDED`.

## Status mapping

| Ingress outcome | gRPC status |
|---|---|
| Stopped, paused, saturated, durable outcome unknown | `UNAVAILABLE` |
| Admission deadline | `DEADLINE_EXCEEDED` |
| Rate limited, stream slots exhausted | `RESOURCE_EXHAUSTED` |
| Missing or invalid token | `UNAUTHENTICATED` |
| Invalid request | `INVALID_ARGUMENT` |
| Other failure | `INTERNAL` |

`SubmitEvent` with an invalid event returns `success=false`, matching the
published protocol, not an error status.

## Data mapping

The protobuf mapping comes from `common/network::wire::grpc_change`:

- nanosecond timestamps become millisecond `effective_from`;
- integer and float values stay distinct;
- relation endpoints may reference other sources;
- nested `Struct` and `List` properties keep the legacy JSON-string mapping;
- non-finite numbers and missing IDs are rejected;
- event source IDs must equal the source's component UUID.

## Observability

Logging matches the HTTP source: it uses the `log` facade when the host
installed a logger, and otherwise writes warnings and errors to standard error.
Per-request rejections are logged at debug level.

## Known limitations

- There is no standard `grpc.health.v1` service; `SourceService.HealthCheck` is
  the health endpoint, and it requires the token when one is configured.
- Server reflection is not provided.
- There are no metrics beyond logs.
- `StreamEvents` reports progress for every event, not every 100 events as the
  legacy source did, and sends no extra final summary message.

## Differences from the legacy gRPC source

- Binds `127.0.0.1` by default, with authentication, TLS and mutual TLS
  available.
- Durability uses the host QoS admission service. `SubmitEvent` and
  `StreamEvents` remain transparently durable through the server-sequenced
  producer.
- `RequestBootstrap` returns `UNIMPLEMENTED` instead of an empty stream.
- Invalid streamed input ends the stream instead of being skipped.
- Streams have their own concurrency limit and an idle timeout.
- The unused `adaptiveEnabled` option has been removed.
