# Native HTTP source: design

`drasi.network/http-source` turns HTTP requests into graph changes. This
document describes how it works. [CONFIGURATION.md](CONFIGURATION.md) lists
every setting and how to operate the source.

## Purpose and scope

The source accepts graph changes from HTTP clients in two ways:

- **Standard routes** accept Drasi's `HttpSourceChange` JSON: one event per
  request, or a batch.
- **Webhook routes** accept arbitrary JSON, XML, YAML or text payloads and map
  them to graph changes through configured templates, for example GitHub or
  IoT webhooks.

It emits each change on its `out` port as a separate `GraphChangeCodec`
envelope. It has no bootstrap capability of its own. Use the native HTTP
bootstrap provider, or another provider, on the query.

## Architecture

```
            ┌──────────────────────── HttpSource (component) ───────────────────────┐
 client ──► │ listener (server.rs)                                                  │
            │   TCP accept ─► connection cap ─► [TLS handshake] ─► hyper HTTP/1+2   │
            │        │                                                              │
            │        ▼                                                              │
            │   request_scope: rate limit ─► bearer token ─► request slot ─► body   │
            │        │                                                              │
            │        ▼                                                              │
            │   routes: /health · standard events · webhooks · /admission/v1        │
            │        │                                                              │
            │        ▼                                                              │
            │   Ingress (common/network): pause gate ─► admission                   │
            │        ├─ volatile: bounded queue ──────────────► next() ─► graph     │
            │        └─ durable: host QoS admission service ──► graph               │
            └───────────────────────────────────────────────────────────────────────┘
```

Responsibilities are split as follows:

| Piece | Location | Responsibility |
|---|---|---|
| `HttpSource` | `src/component.rs` | Lifecycle hooks, management (assess/apply), router, access control, routes |
| Listener | `src/server.rs` | Accept loop, connection limit, TLS, HTTP/1.1 and HTTP/2, header-read timeout, graceful drain |
| Webhooks | `src/webhook.rs` | Route matching, webhook authentication, parsing, template mapping |
| Session protocol | `src/durable_http.rs` | `/sources/{id}/admission/v1` producer routes |
| Schema | `src/schema.rs` | Source-data schema derived from webhook mappings |
| `SourceRuntime` / `Ingress` | `common/network/src/ingress.rs` | Listener ownership, pause gating, volatile queue, Stop modes, live settings |
| Durable admission | `common/network/src/durable.rs` | Session protocol and the server-sequenced producer |
| Security | `common/network/src/security.rs` | Bearer tokens, per-client rate limiting, TLS configuration |

## Routes

| Route | Mode | Authentication |
|---|---|---|
| `GET /health` | always | none |
| `POST /sources/{id}/events` | standard mode | bearer token, if configured |
| `POST /sources/{id}/events/batch` | standard mode | bearer token, if configured |
| `POST /sources/{id}/admission/v1/...` | always (durable binding required to succeed) | bearer token, if configured |
| Any configured webhook path | webhook mode | the route's own HMAC or bearer check |

`{id}` is the source's component UUID. A request addressed to another UUID is
rejected. Configuring `webhooks` replaces the standard event routes with the
webhook routes. A webhook path cannot shadow `/health` or `/sources/...`.

## Admission modes

The admission mode is fixed for the component's lifetime. It depends on whether
the graph binds the source's `admission` dependency to a QoS channel.

**Volatile** (no admission binding). An accepted event goes into a bounded
in-memory queue (`ingressCapacity`), and the graph pulls it through `next()`.
Each event gets a consecutive graph sequence and a nonpersistent producer
identity. A success response means *accepted into the queue*. It does not mean
stored or processed. Events still queued when the source stops are counted and
logged as discarded.

**Durable** (admission bound to a persistent QoS channel). Nothing is queued
locally. There are two producer models:

- **Server-sequenced** (standard and webhook routes). The source registers a
  producer whose name is its own component UUID. Registration is idempotent,
  so a restart reuses the same session. The source reads the producer's
  committed head and assigns consecutive sequences itself. A request succeeds
  only after every event in it is committed.
  - Writers are serialized so that sequences stay consecutive.
  - After any uncertain result (deadline, Stop, lost acknowledgement) or
    out-of-sequence result, the source discards its cached position and
    re-reads the head from the host before admitting more. It therefore never
    skips or reuses a sequence.
  - Delivery is **at-least-once**: if a client retries after a lost response,
    the retry is a new event.
- **Session protocol** (`/admission/v1`). The client registers its own producer
  and supplies consecutive sequences. Retrying the same session, sequence and
  event is idempotent: it returns the original receipt. A changed event for an
  existing sequence conflicts. Events need explicit timestamps, so that retries
  produce identical changes. The source's own UUID cannot be registered as a
  client producer.

A durable webhook request that carries `x-drasi-producer-session` and
`x-drasi-first-sequence` uses the session protocol. Every mapping must then
define `effectiveFrom`, so a retried request maps to identical events. Without
those headers the request uses the server-sequenced producer.

In both models a batch is admitted event by event. On failure the response
reports the committed prefix. The durable path never acknowledges a subset of a
webhook's mapped events under a skip policy.

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Checks that every environment variable named by webhook authentication is set. Binds nothing. |
| `start` | Binds the listener, observing the hook's cancellation and deadline, then starts serving. Nothing is left running if binding fails or is cancelled. A second Start while running is refused. |
| `pause` | Sets the pause gate. New admissions are refused with 503 and `Retry-After`. A volatile batch blocked on queue capacity is interrupted; its accepted prefix stays accepted. A durable batch stops at the next event boundary. Pause returns once in-flight admissions settle. If cancelled or past its deadline, it reopens admission and reports the failure. `/health` reports `paused`. Session metadata routes (register, status, receipt, retire) stay available. |
| `resume` | Reopens admission. |
| `stop` | `Drain`: stops accepting connections and lets in-flight requests finish. The header-read timeout bounds clients that never send a request. `Abort`: also aborts the listener and every connection task. In both cases the listener is joined within the hook's deadline (or `timeoutMs + 5s`). On expiry the error is retryable, ownership is kept, and a later Stop or Abort completes the cleanup. |
| `dispose` | The source owns no persistent state: volatile events live only in the activation, and durable journals belong to the host's QoS channel. Both `Preserve` and `Erase` are idempotent no-ops. Dispose is refused while the source is running. |

All listener and connection work is owned by one task, held in the runtime's
worker slot. A cancelled or timed-out Stop keeps that ownership until the task
has exited. Dropping a `next()` future never consumes an event and never fakes
end-of-stream.

## Management

The factory and the live instance plan updates with the same pure
classification in `component::plan`. Each field is classified by the weakest
boundary at which a change is safe, and the strongest boundary among the
changed fields wins.

- **Pause (in place):** settings read per request. A swap is atomic per
  request: a handler keeps the settings snapshot it loaded.
- **Stop (restart):** settings bound when the listener starts.
- **Replace:** the stream.

[CONFIGURATION.md](CONFIGURATION.md#updates) lists the boundary for every field.

Apply re-checks three things before changing anything:

- the current configuration still matches the object;
- the plan still matches;
- the proposed configuration passes validation, including the exposure rule for
  the actual admission mode.

An in-place apply requires a completed Pause, and a restart apply requires a
stopped source. A refused apply leaves the object unchanged.

## Security model

- **Exposure:** the listener binds `127.0.0.1` by default. A non-loopback
  address is refused at construction unless routes that admit data are
  authenticated or `allowUnauthenticated` is set:
  - the bearer token protects the standard routes, and the admission routes in
    durable mode;
  - in webhook mode, every route must configure route authentication.
- **Bearer tokens:** constant-time comparison against one or two configured
  tokens (two allow rotation). The tokens are secret configuration fields.
- **Webhook authentication:** HMAC-SHA1 or HMAC-SHA256 signatures, or bearer
  tokens read from environment variables. Comparison is constant-time. A
  failure always returns 401 with a generic message, whatever the route's error
  policy, so callers learn nothing about the configuration.
- **Rate limiting:** an optional token bucket per client IP address. It is
  charged before authentication, so floods of bad credentials are limited too.
  At most 65,536 client addresses are tracked; idle buckets are discarded first.
- **TLS:** rustls with the ring provider and safe default protocol versions;
  ALPN offers `h2` and `http/1.1`. An optional client CA requires client
  certificates (mutual TLS).
- **Resource bounds:**
  - open connections (`maxConnections`); excess connections are closed at once;
  - concurrent admitting requests (`ingressCapacity`);
  - body size (`maxMessageBytes`, checked against `Content-Length` first);
  - batch size (`maxBatchEvents`);
  - the header-read deadline;
  - the body and admission deadline (`timeoutMs`);
  - accept errors back off for 50 ms, so a persistent error cannot spin the loop.

  Worst-case buffered request memory is about `ingressCapacity ×
  maxMessageBytes`.

## Failure model

| Failure | Behaviour |
|---|---|
| Listener exits unexpectedly | Logged. In durable mode it is also reported to the graph. `next()` then returns an error, never end-of-stream. |
| Client disconnects mid-body | 400; nothing is admitted. |
| Queue full (volatile) | The request waits up to `timeoutMs`, then gets 503 with the accepted prefix count. |
| Durable write outcome unknown | 503 stating the event may have been committed. The source resynchronizes before the next admission. |
| Host admission busy or full | 503 (standard routes) or 429 (session protocol). Retry later. |

## Data mapping

Standard events use the shared `HttpSourceChange` conversion from
`drasi-http-common`:

- timestamps are nanoseconds and become millisecond `effective_from`;
- an event without a timestamp is stamped with the receive time;
- labels and element IDs must be non-empty and contain no control characters;
- the element namespace is the source's component UUID.

Webhook mappings use the shared `drasi-source-mapping` template engine.

## Source schema

In webhook mode, `source_schema_reader` publishes the labels and property names
the mappings can produce. It is derived from configuration without I/O and
updated by in-place updates. Standard mode accepts arbitrary changes and
publishes no schema.

## Observability

The source logs through the `log` facade when the host installed a logger for
the library, which is the case for statically linked hosts. A dynamically
loaded library has no logger, so warnings and errors go to standard error and
lower levels are dropped. Per-request rejections are logged at debug level, so
rejections cannot flood the logs.

## Known limitations

- Webhook secrets come from environment variables, not host secret references,
  because references resolve only for top-level fields.
- There are no metrics yet beyond logs and the health route.
- The listener does not report readiness through the control interface. Start
  returns once the socket is bound.
- An HTTP listener configured with port `0` exposes its bound port only in the
  startup log.

## Differences from the legacy HTTP source

These are deliberate changes; nothing working was removed:

- **Network defaults:**
  - binds `127.0.0.1` instead of `0.0.0.0`;
  - non-loopback listeners need authentication or an explicit opt-in.
- **Authentication and transport:** standard routes can require bearer tokens,
  and TLS and mutual TLS are supported.
- **Responses:**
  - webhook authentication failures are always 401 (legacy could return 200
    under `acceptAndLog`);
  - router and body errors use the JSON error shape.
- **Durability:** uses the host QoS admission service instead of a
  source-owned WAL. Standard and webhook routes remain transparently durable
  through the server-sequenced producer.
- **Adaptive prefetch:** disabled by default.
- **Pause:** gates admission instead of having no effect.
