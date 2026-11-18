# Native SSE sink: design

Implementation `drasi.network/sse-sink`, version `1`, configuration version
`1`. For every field, default and update boundary, see
[CONFIGURATION.md](CONFIGURATION.md).

## Purpose and scope

The sink serves graph changes to browsers over
[Server-Sent Events](https://html.spec.whatwg.org/multipage/server-sent-events.html).
It accepts the standard graph-change schema on its `in` port from any output:
a source, a transformer or a query. It turns each input into one or more SSE
data frames on one or more HTTP paths. It can also serve a point-in-time
snapshot of a bound output (`GET /snapshot/:alias`), so a client can load the
current state and then apply live frames.

It is a **volatile notification sink**. It keeps no history, no replay buffer
and no per-client cursor. A subscriber receives only the frames broadcast
while it is connected.

## Architecture

```
          handle(input)                          one listener task (owned worker)
host ───────────────────► render ─► Paths ──┐    ┌────────────────────────────────┐
                                            │    │ shared accept loop (server::serve)
                                            ▼    │  TLS, connection cap,          │
                      per-path broadcast::Sender │  header-read deadline          │
                                            │    │   │                            │
                                            └───►│   ├─ GET <path>  → stream      │
                                                 │   ├─ GET /snapshot/:alias      │
                                                 │   └─ OPTIONS     → preflight   │
                                                 └────────────────────────────────┘
```

- **Construction** parses and validates the whole configuration: templates,
  paths, tokens, TLS material, CORS origins and the exposure rule. It binds
  nothing. `initialize` has no further work.
- **Start** binds the listener and spawns one owned worker that runs the
  shared `drasi_computation_network_common::server::serve` accept loop. That
  loop owns every connection task, so cancelling the activation drains them
  and aborting the worker drops them.
- **Activation state** (`Activation`) is created by each Start and shared by
  the routes: the path registry, the pause flag, the snapshot-read gate, the
  subscriber count and a cancellation token. A new activation starts with
  empty dynamic paths and event counters at 1.
- **Live settings** (`Settings`) hold everything that is read per request or
  per frame: tokens, CORS, rate limiter, subscriber cap, heartbeat and
  keep-alive intervals, and event-id mode. They are swapped atomically at a
  Pause boundary (see [Management](#management)). A request keeps the
  settings snapshot it loaded.

## Broadcast model

Each event path has one `tokio::sync::broadcast` channel with
`broadcastCapacity` slots. `handle` renders the input and sends each frame to
its path's channel. Every connected subscriber has its own receiver.

- **Static paths** are `ssePath` plus every route path without `{{`. They
  exist for the whole activation.
- **Dynamic paths** come from templated route paths. A dynamic path is
  created by its first frame. Until then, a subscription to it gets `404`.
- **The path bound.** `maxPaths` bounds static plus dynamic paths. When an
  input needs new paths and the bound would be exceeded, the sink first
  prunes dynamic paths that have no subscribers (never a static path, and
  never a path the same input targets). If the bound is still exceeded, the
  whole input fails before any frame is sent, so a client never sees part of
  an input. A pruned path returns `404` again until its next frame.
- **Lag.** A subscriber that falls more than `broadcastCapacity` frames behind
  is disconnected rather than silently missing frames. It must reconnect and
  reload the snapshot.
- **No subscribers** is not an error: the frame is dropped.

### Frames

A frame is one SSE event. Its `data:` lines carry the rendered payload (see
CONFIGURATION.md for the default JSON shape and templates). Carriage returns
cannot be sent in an SSE field, so `\r\n` and `\r` are normalised to `\n`; each
line becomes its own `data:` field, which the browser joins with `\n`.

Each stream also carries:

- a JSON heartbeat data frame, `{"type":"heartbeat","ts":<ms>}`, every
  `heartbeatIntervalMs` (the first is sent on connect). Existing clients
  depend on it, so it is kept;
- an SSE comment, `: keep-alive`, every `keepAliveIntervalMs`. Browsers ignore
  comments, but they keep proxies and load balancers from closing an idle
  stream, including while the sink is paused;
- optionally (`eventIds: true`) an `id:` field with a per-path counter that
  starts at 1 in each activation. The counter is **volatile**: it restarts on
  every Start and is not related to producer sequences. The sink ignores
  `Last-Event-ID` and never replays; a client that reconnects must reload the
  snapshot. A client can still use the id to notice a gap while connected.

## Delivery profile

Completion is **`SinkCompletion::Accepted`**. `handle` returns `Ok` once every
frame of the input is handed to the broadcast channels. That is not browser
acknowledgement and not delivery to anyone.

| Window | Effect |
|---|---|
| No subscriber connected | Frame lost. Expected; a client loads the snapshot first. |
| Subscriber disconnects, or is disconnected for lag | Frames sent before it reconnects are lost. |
| Stop or restart | Dynamic paths and event counters reset; connected streams end. |
| Host replay after a failed `handle` | Frames may be sent twice to subscribers that were connected both times. |
| `handle` fails (path bound, invalid rendered path, decode error) | No frame of that input is sent; the host may retry. |

Templates are compiled once (construction or an in-place update) by the shared
native template engine (`drasi-computation-template-common`) and checked
statically in every branch. A template that fails to render (strict: a field
the change does not carry) is a permanent failure of the input, so
`onPermanentFailure` applies; no frame of that input is sent and nothing falls
back to the default frame or to the unrendered path (D-5). A rendered path that is invalid (relative after resolution, contains `?`,
`#` or control characters, or is under `/snapshot`) fails the input.

## Snapshot handover

`GET /snapshot/:alias` reads the bound output through the graph's output
catalogue (the `output_snapshots` resource). The body is a JSON array of
canonical `ADD` changes with the same change shape as live frames, and the
response carries `x-drasi-output-epoch` and `x-drasi-output-position`.

The read is a separate request, not an atomic snapshot-plus-subscription. A
client that needs a gap-free view should:

1. open the event stream first and buffer frames;
2. read the snapshot;
3. apply the snapshot, then the buffered frames whose `sequence` is after
   `x-drasi-output-position` for the same output, and continue live.

Every change carries a canonical identity, so applying a frame that the
snapshot already reflects is harmless for row-keyed state. If the epoch
changes (the producer was reconstructed), or the stream disconnects, start
again.

Outcomes are explicit: no snapshot provider `501`, unknown alias or an output
outside the capability `404`, producer not ready `503`, reader exhaustion
`429`, and no snapshot service bound `501`. An error is never an empty array.

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Nothing to do: everything was validated at construction. |
| `start` | Refuses a second start, and a start while a previous listener still needs cleanup. Returns `Cancelled` without binding if the context is already cancelled; observes cancellation and the deadline while binding. A bind failure leaves nothing running and the sink startable. |
| `pause` | See below. Honours cancellation and the deadline; an incomplete pause reopens the sink and returns `Cancelled` or a retryable error. |
| `resume` | Reopens new subscriptions, snapshot reads and heartbeats. |
| `stop` | See below. Stop before start, or after a completed stop, succeeds. |
| `dispose` | Requires a stopped sink. The sink owns no persistent or external state, so `Erase` removes nothing and is idempotent; `erases_owned_state` is declared so removal with Erase succeeds. |
| `handle` | Refused before start, after stop, after the listener failed, and while paused. |

### Pause

The host does not call `handle` while the sink is paused; if it does, the call
is refused. Pause additionally gates the listener, because it is independent
work:

- new event-stream subscriptions and snapshot reads get `503` with
  `Retry-After: 1`;
- in-flight snapshot reads are ended (the body ends with an error, so the
  client sees a truncated response, never a falsely complete one), and Pause
  returns only once they have finished;
- heartbeat frames stop; keep-alive comments continue;
- **connected subscribers stay connected** and receive nothing but keep-alive
  comments until Resume.

Keeping subscribers connected was chosen over closing them: a pause is
usually a short in-place update, and closing every browser stream would cause
a reconnect storm and a full snapshot reload by every client for no benefit,
because no frames are produced while paused anyway. CORS preflights are not
gated.

### Stop

Stop cancels the activation first. That stops accepting connections, ends
every event stream gracefully (the response completes), and ends in-flight
snapshot reads.

- **`Drain`** joins the listener within the context deadline (30 s when the
  host gives none). The listener finishes when every connection has flushed
  and closed. If the deadline passes first, for example because a client
  stopped reading and its socket buffer is full, Stop returns a **retryable**
  error and **retains** the listener task (`join_owned_worker_gracefully`). A
  later Stop continues joining it; Start is refused until then.
- **`Abort`** also aborts the listener task. That drops every connection task,
  so connections close at once, and the join completes promptly.

A listener that failed earlier is reported in the log, not as a stop failure.
`Running` has a `Drop` that cancels and aborts as a last resort, but no
lifecycle path relies on it: Stop always joins.

## Management

The factory plans updates (`Factory::update_planner`, `UpdatePlanner::assess`) and
the instance exports `NativeManagement`. Capabilities declare `management`
and update modes `InPlace`, `Restart` and `Recreate`.

| Boundary | Fields |
|---|---|
| Pause (InPlace) | `routes`, `defaultTemplate`, `heartbeatIntervalMs`, `keepAliveIntervalMs`, `eventIds`, `authToken`, `authTokenSecondary`, `allowQueryToken`, `allowUnauthenticated`, `corsAllowedOrigins`, `rateLimitPerSecond`, `rateLimitBurst`, `maxSubscribers` |
| Stop (Restart) | `host`, `port`, `tlsCertificatePem`, `tlsPrivateKeyPem`, `tlsClientCaPem`, `ssePath`, `broadcastCapacity`, `maxPaths`, `maxConnections`, `headerReadTimeoutMs` |
| Replace (Recreate) | `outputs`, or any change to the incoming edges |

`apply` re-parses the request, refuses if the live configuration is no longer
the assessed `current`, re-assesses and refuses a plan that no longer matches.
A Pause apply requires a completed pause and a Stop apply a stopped sink.
`Refused` leaves the object unchanged.

In-place effects:

- tokens, CORS, rate limit and `maxSubscribers` apply to the next request;
  lowering `maxSubscribers` does not disconnect anyone;
- templates and routes apply to the next input; new static paths are created
  at once, and paths that are no longer static become prunable dynamic paths;
- `eventIds` applies to the next frame of every stream;
- heartbeat and keep-alive intervals apply to new connections. Existing
  streams keep their intervals until they reconnect.

## Security model

The legacy reaction served every result to anyone who could reach its port,
on all interfaces, to any web origin. The native sink is closed by default.

| Threat | Mitigation |
|---|---|
| Accidental exposure on all interfaces | Default host `127.0.0.1`. A non-loopback host requires `authToken`, or an explicit `allowUnauthenticated: true` (for an authenticating proxy). Refused at construction and at assessment. |
| Unauthorised reads | Optional bearer token (`authToken`, plus `authTokenSecondary` for rotation), compared in constant time, on the event-stream and snapshot routes. |
| Browser clients cannot set headers | `EventSource` cannot send `Authorization`, so the token is also accepted as `?access_token=` (disable with `allowQueryToken: false`). Query strings can appear in proxy and server access logs, browser history and `Referer` headers: use TLS, short-lived or rotated tokens, and keep access logs private. Prefer a cookie-authenticating reverse proxy for public deployments. |
| Cross-site reads | No CORS headers unless `corsAllowedOrigins` lists the origin; `["*"]` must be explicit. Listed origins are echoed with `Vary: Origin`. Preflights are unauthenticated (browsers send no credentials on them) but rate limited. |
| Eavesdropping | Optional TLS (`tlsCertificatePem`, `tlsPrivateKeyPem`), and mutual TLS with `tlsClientCaPem`. ALPN offers HTTP/2 and HTTP/1.1. |
| Connection floods and slow clients | `maxConnections` closes excess connections at accept; `headerReadTimeoutMs` bounds header reads and TLS handshakes; `maxSubscribers` caps streams (503); optional per-client-address rate limit on new subscriptions, snapshot reads and preflights (429). Failed authentication is charged to the rate limit. |
| Unbounded memory | `broadcastCapacity` bounds each channel; lagging subscribers are disconnected; `maxPaths` bounds paths, with idle-path pruning. |
| Secrets in logs | Secret fields are redacted by `Debug`. `configuration()` returns them because it is the persistence hook. |

The rate limiter keys on the TCP peer address. Behind a proxy every client
shares the proxy's address, so set the limit accordingly or enforce it at the
proxy.

## Observability

Logs use the shared `diag!` facade with target `drasi.network`: listener start
(Info), unexpected listener exit (Error), snapshot failures
and lagging subscribers (Warn), pruning (Debug). In a dynamically loaded
library without a host logger, warnings and errors go to standard error. There
are no metrics yet.

## Known limitations

- No replay, no `Last-Event-ID` support, no durable delivery.
- Snapshot and subscription are separate requests (see
  [Snapshot handover](#snapshot-handover)).
- In-place heartbeat and keep-alive interval changes reach only new
  connections.
- The rate limit sees the proxy address behind a proxy.
- Incoming recovery snapshot/progress control envelopes are unsupported.

## Differences from the legacy SSE reaction

| Legacy | Native |
|---|---|
| Default host `0.0.0.0` | `127.0.0.1`; non-loopback needs a token or `allowUnauthenticated` |
| CORS `Any` on every route | No CORS unless `corsAllowedOrigins` is set |
| No authentication, TLS or limits | Bearer token (header or query), TLS/mTLS, connection, subscriber and rate limits |
| Keep-alive comment every 30 s, fixed | `keepAliveIntervalMs` (default 15 s) |
| Heartbeat frame | Kept, suspended while paused |
| `/snapshot/:query_id` via a query snapshot fetcher | `/snapshot/:alias` via output snapshots of any output |
| Template context with `query_id`, `query_name`, `data` | Truthful change notification context (`output`, `sequence`, `operation`, `identity`, images) |
| Render failure falls back to the default payload or the configured path; templates trial-rendered against `{}`; HTML escaping | Render failure is permanent under `onPermanentFailure`; templates compiled once and checked statically by the shared native template engine; strict, no escaping |
| Pause not applicable | Real pause gate |
