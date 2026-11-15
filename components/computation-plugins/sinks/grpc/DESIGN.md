# Native gRPC sink: design

`drasi.network/grpc-sink` sends graph changes to a gRPC receiver. This
document describes how it works. [CONFIGURATION.md](CONFIGURATION.md) lists
every setting and how to operate the sink.

## Purpose and scope

The sink is a client of one unary RPC from `drasi-grpc-common`'s reaction
protocol:

```
service ChangeService {
  rpc ProcessChanges(ProcessChangesRequest) returns (ProcessResultsResponse);
}
ProcessChangesRequest  { ChangeBatch changes; map<string,string> metadata; }
ChangeBatch            { OutputIdentity output; repeated Change changes; Timestamp timestamp; }
ProcessResultsResponse { bool success; string message; string error; uint32 items_processed; }
```

Each `Change` states its `operation` (`ADD`, `REPLACE`, `PATCH`, `DELETE`),
canonical `identity`, optional `element` shape, optional typed `before` and
`after` images (a patch's `after` holds only the properties it sets), a
patch's `removed_properties`, the producer `sequence`, the envelope
`timestamp` when there is one, and a `payload` (the canonical change JSON, a
rendered template, or nothing; see `outputFormat`). The projection is the
shared truthful change projection of [SINKS.md](../../SINKS.md); no query
annotation is read, so source, transformer and query outputs are sent the same
way.

The legacy `drasi.v1.ReactionService` is not used.

## Architecture

```
 host ─► handle / handle_batch (one at a time, &mut self)
           │
           ▼
   decode every input (common/network::wire)            ── malformed input: error
           │
           ▼
   project every change: templates (render.rs) ─► protobuf Change ─► request metadata
           │                                      per-change failure ─► failurePolicy
           ▼
   group: same output and metadata, ≤ item limit, ≤ maxRequestBytes (incl. delivery id)
           │
           ▼
   per request: x-drasi-delivery-id ─► ProcessChanges ─► classify (transport.rs)
           │                         retry / reconnect within retryBudgetMs
           ▼
   exact acknowledgement ─► next request ─► Ok (Handled)
```

| Piece | Location | Responsibility |
|---|---|---|
| `GrpcSinkFactory` | `src/lib.rs` | Metadata, capabilities, creation, factory assessment |
| `GrpcSinkConfig` | `src/config.rs` | Fields, defaults, validation, published definition |
| `GrpcSink` | `src/component.rs` | Lifecycle, management, projection, grouping, delivery loop |
| `Renderer` | `src/render.rs` | Templates compiled at construction; routing; strict rendering |
| Transport | `src/transport.rs` | tonic endpoint (TLS, keepalive, timeouts), client, status classification |
| Retry policy | `common/network/src/retry.rs` | Jittered exponential backoff, requested delay, time budget |

There are no workers: all work happens inside the host's awaited `handle`.

## Acknowledgement rule

A request completes only when the receiver returns `success=true` **and**
`items_processed` equals the number of changes in the request. Anything else
is a failure:

| Response | Handling |
|---|---|
| `success=true`, exact count | Complete; the next request is sent |
| `success=true`, other count (zero, partial or excessive) | Incomplete. Not retried locally (a resend would repeat the processed part); governed by `failurePolicy` |
| `success=false` | Permanent rejection. Not retried locally; governed by `failurePolicy`. The receiver's `error` is logged sanitized |
| gRPC status | Classified below |

`handle` returns `Ok` only after every request of the input has completed, or
been deliberately skipped under `failurePolicy: skip`. The receiver must not
acknowledge before its effect is durable if the deployment relies on
`Handled`.

## Batching

Batching is host-owned (`NativeSink::batching`): the sink never queues an
input privately or acknowledges it early.

- Fixed mode (`batchSize` > 1, `batchFlushTimeoutMs`) and adaptive mode
  (`batching.mode: adaptive`, throughput-driven between the minimum and
  maximum) publish a `SinkBatching` policy. The host re-reads it before each
  batch, so an in-place update takes effect at the next batch. The host
  contract caps one batch at 1024 inputs; `batchSize` above 1024 still caps the
  changes per request at `batchSize`.
- `batchSize` 1 (the default) publishes no policy: each input is handled
  alone, with no batching state.
- Every input of a host batch is decoded and projected first. Changes are then
  grouped into requests: one output (alias, component, port) and one set of
  request metadata per request, at most the item limit, and at most
  `maxRequestBytes` encoded including the delivery-id entry. A change of output
  or rendered metadata starts a new request; order is never changed.
- A single change larger than `maxRequestBytes` is a per-change failure under
  `failurePolicy`.
- `handle_batch` accepts any nonempty batch, including one collected under a
  previous policy, because requests are split by the current limits.

## Delivery profile: `Handled`

The sink declares `SinkCompletion::Handled`: the receiver confirmed the
effect before `handle` returned. It is at-least-once, not exactly-once.

**Duplicate windows** (the same delivery id is sent again):

1. A local retry after a timeout, `CANCELLED`, `UNAVAILABLE`, `UNKNOWN`,
   `INTERNAL`, `ABORTED` or `RESOURCE_EXHAUSTED`, when the receiver had in
   fact processed the request (for example the reply was lost).
2. A host retry of an input that failed under `strict`: requests of that input
   that had already completed are sent again.
3. A crash, Abort or host cancellation after the receiver processed a request
   but before the host recorded the input as complete.
4. The host replaying retained input after a restart.

**Loss windows:** none under `strict`. Under `skip`, an input whose delivery
failed (exhausted retries, permanent status, rejection, incomplete count,
template failure, oversized change) is completed without being delivered and
logged at warning level. That is deliberate, not accidental, loss.

A multi-request input is not atomic: a failure part way through leaves the
earlier requests delivered.

## Idempotency key

Every request carries `x-drasi-delivery-id`, both as gRPC metadata (a header)
and in the request's `metadata` map:

- `{component}/{port}/{sequence}.{ordinal}` for a single change;
- `{component}/{port}/{s1}.{o1}-{s2}.{o2}` for a contiguous run.

`component`/`port` name the real producing output, `sequence` is the
producer's logical sequence and `ordinal` the change's position within it.
The identity is built by `common/network::config::delivery_id`; it contains no
transport, retry or configuration identity, so local retries, host retries
and replays of the same changes carry the same value. A receiver deduplicates
by remembering processed ids (or the highest `(sequence, ordinal)` per
`component/port`). If the batching settings change between an attempt and its
replay, the same changes can be grouped differently and carry different range
ids; a receiver that needs exact deduplication across such changes should
track per-change `(component, port, sequence, ordinal)` from the items
themselves (`Change.sequence` plus position).

The receiver protocol has no acknowledgement field carrying the identity
back, so a host `DeliveryRunner` consumer mode (`DeliveryBatchIdentity`) is not
offered; see Known limitations.

## Retry and classification

Failures are classified by `tonic::Code`, never by message text
(`transport::classify`):

| Code | Retried | New channel | Requested delay honoured |
|---|---|---|---|
| `UNAVAILABLE` (includes connection failures) | yes | yes | yes |
| `UNKNOWN`, `INTERNAL` (transport and HTTP/2 errors map here) | yes | yes | no |
| `DEADLINE_EXCEEDED`, `CANCELLED` (tonic reports the expiry of the call's own deadline as `CANCELLED`) | yes | yes | no |
| `RESOURCE_EXHAUSTED` | yes | no | yes |
| `ABORTED` | yes | no | no |
| `INVALID_ARGUMENT`, `UNAUTHENTICATED`, `PERMISSION_DENIED`, `UNIMPLEMENTED`, `FAILED_PRECONDITION`, `NOT_FOUND`, `ALREADY_EXISTS`, `OUT_OF_RANGE`, `DATA_LOSS` | no | – | – |

- `UNKNOWN` and `INTERNAL` are retried because tonic reports broken pipes,
  resets and protocol errors with them; a receiver bug that always returns
  `INTERNAL` costs at most `maxRetries` extra calls per request.
- A "new channel" is a fresh lazily connected tonic channel, which replaces a
  connection that may be half-closed or draining after `GOAWAY`.
- The requested delay comes from `grpc-retry-pushback-ms` (milliseconds) or
  `retry-after` (seconds or an HTTP date) response metadata. gRPC `RetryInfo`
  status details are not decoded.
- Backoff is the shared policy: full jitter under a ceiling of
  100 ms × 2ⁿ, capped at 5 s. Retrying stops after `maxRetries` retries, or
  when the next delay would pass `retryBudgetMs` from the request's first
  attempt. A requested delay that exceeds the remaining budget ends retrying.
- Each request has its own retry budget, so one input of `n` requests can take
  up to `n × (retryBudgetMs + one attempt)`.
- Timeouts: each call has one deadline, `timeoutMs`, sent to the receiver as
  `grpc-timeout` and enforced by tonic. Connection setup is bounded separately
  by `connectTimeoutMs`. One attempt therefore lasts at most
  `connectTimeoutMs + timeoutMs`.
- The retry loop keeps no state outside the `handle` future. The host cancels
  an in-flight delivery by dropping that future (Stop with `Abort`, or a
  deadline), which also ends any backoff sleep or connection attempt at once.

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Nothing to contact. Templates, routes, metadata and TLS material are compiled and validated at construction, so an invalid configuration fails creation. |
| `start` | A pre-cancelled Start returns a `Cancelled` lifecycle error and leaves the sink startable. A second Start while running is refused. By default (`requireConnectedAtStart: false`) Start builds a lazy channel and returns without contacting the receiver: a receiver that is down is a delivery failure, not a Start failure. With `requireConnectedAtStart: true`, Start connects, retrying within `maxRetries`/`retryBudgetMs`, and returns `Cancelled` when its cancellation is requested or a retryable error when its deadline passes or retries are exhausted; the sink then stays stopped and startable. |
| `pause` | Succeeds (host-driven, D-6). The sink owns no worker; the host calls no `handle` while paused and settles the in-flight one first. That in-flight delivery's retries are bounded by `retryBudgetMs`. |
| `resume` | Clears the pause flag. |
| `stop` | Closes the channel. `Drain` and `Abort` both complete at once, because no delivery runs concurrently with Stop: the host awaits (Drain) or drops (Abort) the in-flight `handle` first. Stop before Start succeeds. A later `handle` is refused until Start. |
| `dispose` | The sink owns no destination-side state. `Preserve` and `Erase` are idempotent no-ops; `erases_owned_state` is declared (D-1). |
| `handle` / `handle_batch` | As above. A failure leaves the sink able to retry the same input without Stop/Start: nothing is latched, and a broken channel is rebuilt by the next attempt. |

Readiness is not reported through a status channel: the SDK offers sinks no
status API besides upstream control notifications, which are not used. A
receiver that is down is visible as failing deliveries and warning logs.

## Management

`component::plan` classifies every field by the weakest safe boundary:

- **Pause (in place):** `metadata`, `secretMetadata`, `timeoutMs`,
  `maxRetries`, `retryBudgetMs`, `failurePolicy`, batching (`batchSize`,
  `batchFlushTimeoutMs`, `batching`, `maxBatchBytes`), `outputTemplates`,
  `outputFormat`, and `requireConnectedAtStart` (read only by the next Start).
  These are read per delivery and are swapped as one set.
- **Stop (restart):** `endpoint`, TLS fields, `connectTimeoutMs`,
  `keepaliveIntervalMs`, `keepaliveTimeoutMs`, `compression`,
  `maxRequestBytes`. These are bound into the channel and client at Start.
- **Replace:** `outputs` aliases, and any change to the bound inputs
  (`update::inputs_changed`; live rebinding is a host decision, CW-94).

The factory and the live object use the same pure assessment. `apply`
re-parses both configurations, refuses if the live configuration is not the
assessed current one, re-assesses and refuses with `now_requires` if the plan
changed, requires a completed Pause (or a stopped sink) for in-place updates
and a stopped sink for restart updates, and builds the complete new delivery
settings before changing anything, so a refused apply leaves the sink
unchanged.

## Security model

- **Transport:** `grpc://`/`http://` is plaintext. `grpcs://`/`https://` uses
  TLS through tonic with the ring provider. Server certificates are verified
  against the bundled Mozilla root set (`webpki-roots`, not the operating
  system store) plus `tlsCaPem`. `tlsDomainName` overrides the name verified
  (for connecting by IP address). A client certificate and key enable mutual
  TLS. TLS settings on a plaintext endpoint are rejected rather than ignored.
- **Credentials:** `secretMetadata` (for example `authorization`) and
  `tlsClientPrivateKeyPem` are secret fields: hosts accept only secret
  references for them, `Debug` redacts them, and secret metadata is sent only
  as request headers, never in the request's `metadata` map and never logged.
  `configuration()` returns them because it is the persistence hook.
- **Reserved metadata:** `x-drasi-component`, `x-drasi-port` and
  `x-drasi-delivery-id` are set by the sink; configuration and templates
  cannot set them. Template metadata names are checked at construction;
  rendered values are checked per change.
- **Logs:** receiver error text, status messages and output names are
  sanitized (control characters escaped, 512-byte cap). Bodies and secret
  values are never logged.
- **Resource bounds:** `maxRequestBytes` bounds each encoded request and the
  decoded response; `maxBatchBytes` bounds host collection; HTTP/2 keepalive
  pings (default every 30 s while a call is open, 20 s timeout) detect dead
  connections. Keepalive is not sent on idle connections, which avoids
  `too_many_pings` `GOAWAY` from receivers with default ping policies.

## Observability

Logging uses the `log` facade through `common/network`'s `diag!` when the host
installed a logger; a dynamically loaded library without one writes warnings
and errors to standard error. Retries log at warning level with the attempt
number and the sanitized status; skips under `failurePolicy: skip` log at
warning level with the output, sequence and sanitized error. There are no
metrics yet (X-11).

## Known limitations

- **No consumer mode.** The `ChangeService` acknowledgement does not echo a
  delivery identity, so the host `DeliveryRunner` profile with
  `DeliveryBatchIdentity` (D-4 (b)) is not offered. It needs a protocol change
  in `drasi-grpc-common` (an identity field in `ProcessResultsResponse`).
  Until then the profile is `Handled` with receiver-side deduplication by
  `x-drasi-delivery-id`.
- In-flight retries are not interrupted by Pause; Pause waits for them, bounded
  by `retryBudgetMs`. Stop with `Abort` interrupts them immediately.
- gRPC `RetryInfo` status details are not decoded; only the metadata hints
  above are honoured.
- Compression is gzip only, and the receiver must accept gzip when it is
  enabled (otherwise every call fails with `UNIMPLEMENTED`, a permanent error).
- No readiness status or metrics are published.
- The adaptive batching throughput window restarts at an in-place update.

## Differences from the legacy gRPC reaction

- `ChangeService.ProcessChanges` with typed changes from any producer, instead
  of `ReactionService` query-result items (D-9).
- Lazy connection by default, like the legacy reaction, but bounded and
  cancellable when `requireConnectedAtStart` is set.
- TLS and mutual TLS.
- Status classification by code; permanent codes are no longer retried; the
  fixed 5 s `RESOURCE_EXHAUSTED` delay is replaced by the backoff policy with
  receiver hints; `GOAWAY` and broken connections are handled by rebuilding the
  channel.
- Exact acknowledgement and an idempotency key on every request.
- Template render failures follow `failurePolicy` instead of falling back to
  the canonical payload (D-5).
- Different defaults; see the migration section of CONFIGURATION.md.
