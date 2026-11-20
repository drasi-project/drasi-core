# Native HTTP sinks: design

`drasi-computation-sink-http` provides two native ComputationGraph factories.
This document describes how they work. [CONFIGURATION.md](CONFIGURATION.md)
lists every setting and how to operate them.

| Factory | Role | Completion profile |
|---|---|---|
| `drasi.network/http-sink` | `Component::Sink` (plain `NativeSink`) | `Handled` |
| `drasi.network/http-delivery` | `Component::Consumer` (`ConsumerMode::External`) | `Consumer/External` through the host `DeliveryRunner` |

Both have implementation version `1`, configuration version `1` and native
ABI 0.1, and both consume the standard `GraphChangeCodec` schema on their `in`
port from any producer: a source, a transformer or a query
([sink input contract](../../SINKS.md)). The HTTP sink is the plugin's first
factory.

## Purpose and scope

- **HTTP sink**: deliver graph changes to an ordinary webhook. Each change
  becomes one request, or, with adaptive batching, each run of changes from
  one output becomes one `{"batch":[...]}` request. Requests may be shaped by
  Handlebars templates (body, URL, method and headers per output and
  operation). A delivery completes when the destination answers a configured
  success status.
- **HTTP delivery consumer**: deliver whole input batches to an endpoint that
  speaks Drasi's confirmation protocol (for example the reference
  `HttpDeliveryEndpoint`), so that operation completion is tracked by the host
  `DeliveryRunner` and only an exact confirmation advances progress.

Neither component owns external state, runs a worker or opens a listener.

## Architecture

```
                 ┌──────────────────── HttpSink ─────────────────────┐
 host handle ──► │ wire::notifications (alias, sequence, ordinal)    │
 handle_batch    │   │                                               │
                 │   ├─ per change: Renderer ─► Request ─┐           │
                 │   └─ batching:  Renderer ─► items ─► split by     │
                 │                  output / maxRequestBytes / size ─┤
                 │                                                   ▼
                 │   transport::deliver ─► reqwest (rustls, no redirects)
                 │      retry: Retry (jitter, Retry-After, budget)    │
                 │   FailurePolicy (strict: fail input / skip: log)   │
                 └────────────────────────────────────────────────────┘

                 ┌──────────────── HttpDeliveryConsumer ─────────────┐
 DeliveryRunner ►│ HttpDeliveryHandler: one POST per batch,          │
 (per operation) │ checks 200 + DeliveryBatchIdentity, caches it so  │
                 │ later operations of the batch complete locally    │
                 └────────────────────────────────────────────────────┘
```

| Piece | Location | Responsibility |
|---|---|---|
| `HttpSink` | `src/component.rs` | Lifecycle hooks, management (assess/apply), rendering, batching, failure policy |
| `HttpSinkConfig` | `src/config.rs` | Fields, published defaults, validation (URLs, headers, secrets, TLS, templates) |
| `Renderer` | `src/render.rs` | Compiles every template once at construction and checks helper names |
| Transport | `src/transport.rs` | Client construction (TLS, pool, timeouts), retry loop, classification, capped body reads |
| `HttpDeliveryConsumer`, `HttpDeliveryConfig` | `src/consumer.rs` | The consumer factory's component, configuration and management |
| `HttpDeliveryHandler` | `src/delivery.rs` | Confirmation protocol client |
| `HttpDeliveryEndpoint` | `src/delivery/receiver.rs` | Reference receiver (feature `delivery-receiver`, depends on `drasi-lib`) |
| Retry, TLS, delivery identity, sanitizing | `common/network` | Shared with the gRPC sink |

## Input decoding and change projection

`wire::notifications` validates the input port and declared incoming stream,
decodes the standard graph-change batch and yields one notification per change
with its output alias, producer sequence and change ordinal. The notification
JSON is both the default request body and the template context:
`operation` (`ADD`, `REPLACE`, `PATCH`, `DELETE`), `output` (`alias`,
`component`, `port`), `sequence`, `timestamp` (only when the envelope carries
one), `identity`, and only the `element`, `before`, `after`,
`removedProperties` and `effectiveFrom` the record carries. A scheduling control
or recovery frame fails the input; an empty change set sends nothing.

## Delivery profile (HTTP sink): `Handled`

`handle` returns `Ok` only when every change of the input was delivered (a
configured success status was received). Under `onPermanentFailure: skip`, an
input with a permanently rejected change still delivers its other changes and
then fails permanently, so the graph counts it as skipped. There is no local
queue: a returned `Ok` is never "accepted for later".

Exact duplicate windows (the destination may see the same change again; every
repeat carries the same `x-drasi-delivery-id`):

1. **Retry after an unknown outcome.** A timeout, a connection failure after
   the request was written, or a retryable status (for example a 503 returned
   after the destination applied the effect) is retried with the identical
   request.
2. **Replay of a failed input.** A transient failure fails the whole input,
   and the graph retries it in place with backoff (see the
   [delivery-failure model](../NATIVE-SINK-GUIDE.md#delivery-failures)).
   Changes of that input that were already delivered (earlier single requests,
   or earlier batch chunks) are sent again. Under `skip`, a permanently failed
   batch is redelivered one input at a time to isolate the rejected input.
3. **Crash or cancellation before acknowledgement.** If the process stops, or
   the host drops the in-flight `handle` (Stop with `Abort`, a deadline), after
   the destination completed a request but before the host acknowledged the
   input, the input is replayed in full.

Exact loss windows:

- **None under `strict`.** Every undelivered change fails its input.
- **Under `skip`**, a change (or a batch chunk) whose delivery permanently
  failed, exhausted its retries, failed to render, or exceeded
  `maxRequestBytes` is logged (sanitized: component, output, sequence and
  error, never the body) and dropped.
- A configured success status is trusted. Adding `202` to
  `successStatusCodes` accepts the destination's own queue as completion; a
  loss inside that queue is invisible to Drasi.

Receivers that must not apply a change twice deduplicate on
`x-drasi-delivery-id`.

## Idempotency key

Every request carries `x-drasi-delivery-id`, built by the shared
`delivery_id(first, last)` from the changes the request carries:

- one change: `{component}/{port}/{sequence}.{ordinal}`;
- a batch request: `{component}/{port}/{first sequence}.{ordinal}-{last sequence}.{ordinal}`
  for the run it carries (a run of one change uses the single form).

The producer's logical sequence and the change ordinal are replayed unchanged,
so retries and replays carry the same identity. It contains no transport,
retry or configuration identity. Batch boundaries depend on host collection,
`adaptive` settings and `maxRequestBytes`, so after a replay a batch request
may cover a different run than before; deduplicate per change using the
`sequence` (and position) in each item when batching.

## Batching

With `adaptive` and `batchEndpoint`, the factory advertises native host
batching (`HostServices::sink_batching`). The host collects inputs and timers within
`SinkBatching` (`maxBatchBytes`, at most `min(adaptive.maxBatchSize, 1024)`
inputs, the adaptive operation count and delay) and calls `handle_batch`. The
policy is the shared `AdaptivePolicy::from_settings` in `common/network`, so the
same `adaptive` object batches identically in the HTTP and gRPC sinks. A batch
is accepted whenever it is nonempty and within the host's 1024-input bound,
even if it was collected under an earlier tier or before a Pause update; the
sink splits it at the current operation target. The
sink renders each change to one item (the route's body template, which must
render JSON, or the default notification) and starts a new request when:

- the output changes, so `x-drasi-component`/`x-drasi-port` stay truthful;
- the adaptive operation count is reached;
- the next item would exceed `maxRequestBytes` (the envelope overhead
  `{"batch":[]}`, 12 bytes, plus each item and one separator).

An item larger than `maxRequestBytes` on its own is a per-change failure
governed by the failure policy. Only body templates apply to batch items; the
batch request is always `POST {baseUrl}{batchEndpoint}`.

## Retry and classification

Each request is retried by the shared `Retry` policy:

- exponential backoff with full jitter (100 ms doubling to 5 s);
- `Retry-After` (seconds or HTTP date) replaces the backoff on 429 and 503;
- at most `maxRetries` additional attempts and never past `retryBudgetMs` from
  the first attempt; a requested delay beyond the remaining budget ends
  retrying immediately.

| Outcome | Class |
|---|---|
| Status in `successStatusCodes` | success |
| 5xx, 408, 409, 425, 429 | retryable |
| Other 4xx, 1xx/3xx (redirects are never followed), 2xx not in `successStatusCodes` | permanent |
| `reqwest` connect, timeout or in-flight request error | retryable |
| Other client errors (builder, body) | permanent |
| Template, URL, header or size failure | permanent, per change |

Classification uses only the status and the `reqwest` error kind, never error
text. Up to 4 KiB of every response body is read, both for connection reuse and
for diagnostics; error bodies are logged sanitized (control characters escaped,
at most 512 bytes). A permanent failure, or exhausted retries, is returned to
the failure policy.

The loop holds no state outside the `handle` future. The host cancels an
in-flight `handle` by dropping that future, which also cancels a backoff sleep;
this is how in-flight retries observe Stop with `Abort` and lifecycle
deadlines. A Drain stop waits for the in-flight delivery, which is bounded by
`retryBudgetMs` plus one attempt's `timeoutMs`.

## Templates

`outputTemplates.routes` (keyed by output alias, or its last dotted segment)
and `outputTemplates.defaultTemplate` select, per operation (`added`,
`updated`, `deleted`), a body `template`, `url`, `method` and `headers`. Every
template string is compiled once at construction by the shared native template
engine (`drasi-computation-template-common`); a helper call must name one
of `if`, `unless`, `each`, `with`, `lookup`, `raw`, `log`, `eq`, `ne`, `gt`,
`gte`, `lt`, `lte`, `and`, `or`, `not`, `len` or `json`; partials and decorators
are rejected. Template headers may not name the reserved headers (HS-07).

Rendering a change never falls back (D-5): a failing body, URL or header
template, a rendered URL off the base origin, an invalid rendered header value,
a batch item that does not render JSON, or an oversized body is a per-change
error for the failure policy. Rendering is strict, as in every native sink: a
missing field is a render failure, not empty text as in the legacy reaction.
Values are not HTML-escaped (the legacy reaction escaped `{{x}}`); use
`{{json x}}` for a JSON value.

## Delivery consumer (`drasi.network/http-delivery`)

The host `DeliveryRunner` records each input's identity before any effect and
calls the consumer once per operation. On the first operation of a batch the
handler sends one `POST` with `{"version":1,"port":"in","identity":…,"envelope":…}`
(the complete `EnvelopeCodec` JSON envelope), `Authorization` when a token is
configured, and `x-drasi-delivery-id: {identity stream}/{sequence}`. Only HTTP
**200** whose body is exactly the batch's `DeliveryBatchIdentity` confirms the
batch; later operations of the same batch then complete without another
request. Anything else (202, another identity, a malformed or oversized reply)
is a failure.

`retryable` tells the runner which failures to retry under its own bounded
policy: transport failures (connect, timeout, in flight) and 408, 425, 429 and
5xx rejections. A confirmation mismatch and other rejections (400, 409, 422, …)
are permanent. The consumer runs no retry loop of its own.

Profile: only the operation in flight can repeat (a lost confirmation resends
the batch with the same identity, which a confirming receiver recognizes). The
receiver contract, including how `HttpDeliveryEndpoint` derives the identity
again from its configured consumer scope, is described in
[NETWORK.md](../../NETWORK.md#opt-in-http-completion-service).

## Owned and borrowed state

Neither component owns destination state. Delivery progress for the consumer
belongs to the host runner. Both declare `erases_owned_state` (D-1): erasing
disposal has nothing to remove and is idempotent.

## Lifecycle

| Hook | HTTP sink | Delivery consumer |
|---|---|---|
| `initialize` | Nothing to bind: templates, headers, secrets and TLS material were validated at construction. | Same. |
| `start` | Builds the client (rustls, private CA, client identity, connect timeout, pool, no redirects). A pre-cancelled Start returns `Cancelled`, builds nothing and leaves the sink startable. A second Start is refused. | Builds the client and handler; same cancellation rule. |
| `pause` | Succeeds (D-6): the sink has no worker, the host calls no `handle` while paused and waits for the in-flight one. Marks the in-place update boundary. | Same; the runner calls no handler while paused. |
| `resume` | Clears the pause mark. | Same. |
| `stop` | Drops the client and its pooled connections. The in-flight delivery belongs to the host's `handle` future: Drain lets it finish (bounded by the retry budget), Abort drops it. `handle` fails while stopped. | Drops the handler and its cached confirmation. |
| `dispose` | Drops any client; `Preserve` and `Erase` are idempotent no-ops. | Same. |
| `handle` failure | Leaves the sink ready: the same input can be retried without Stop/Start. | Same. |

## Management

Both factories plan updates (`Factory::update_planner`, `UpdatePlanner::assess`) and
both live objects export `NativeManagement` with the same pure classification.
The strongest boundary among the changed fields wins.

HTTP sink:

- **Pause (InPlace):** `outputTemplates`, `headers`, `secretHeaders`, `token`,
  `timeoutMs`, `maxRequestBytes`, `maxRetries`, `retryBudgetMs`,
  `successStatusCodes` — everything read per delivery — and the `adaptive`
  bounds and `maxBatchBytes`, because the host reads `batching()` before every
  collection. `onPermanentFailure` is a Replace: it is part of the description
  the graph captured.
- **Stop (Restart):** `url`, `baseUrl`, `batchEndpoint` (so enabling or
  disabling `adaptive` restarts), TLS fields, `connectTimeoutMs`,
  `poolIdleTimeoutMs`, `poolMaxIdlePerHost`.
- **Replace (Recreate):** `outputs`, or changed input bindings.

Delivery consumer: Pause for `token`, `timeoutMs`, `maxMessageBytes`; Stop for
`url`, TLS fields and `connectTimeoutMs`.

Assessment parses and validates the proposed configuration (for the sink,
against the proposed incoming edges) and errs when it is invalid. `apply`
re-parses both configurations, refuses when the current one no longer matches
the object, re-assesses and refuses a plan that no longer matches (with the
boundary now required), refuses a weaker boundary than required, and requires
a completed Pause (in place) or a stopped object (restart). A refused apply
changes nothing. An in-place apply swaps the rendering state and headers in one
assignment; a restart apply also rebuilds the batching policy, and the next
Start builds the new client.

## Security model

- **Secrets:** `token`, `secretHeaders` and `tlsClientPrivateKeyPem` are secret
  configuration fields (hosts accept only secret references for them).
  `configuration()` returns resolved values because it is the persistence
  hook; `Debug` output redacts them; header values built from them are marked
  sensitive.
- **Reserved headers:** `x-drasi-component`, `x-drasi-port` and
  `x-drasi-delivery-id` are set by the sink and rejected in `headers`,
  `secretHeaders` and template headers. `authorization` cannot be configured
  together with `token`.
- **Destinations:** rendered URLs must stay on the base origin; redirects are
  never followed; URLs with credentials or fragments are rejected.
- **TLS:** rustls; `tlsCaPem` adds trusted roots to the built-in roots;
  `tlsClientCertificatePem`/`tlsClientPrivateKeyPem` enable mutual TLS. PEM
  documents are parsed and paired at construction.
- **Logs:** the sink never logs bodies or secrets. Logged fields (outputs,
  delivery IDs, errors, response bodies) are sanitized and capped. The
  reference receiver logs internal causes and returns only a generic message
  per status.

## Observability

Logs go through the `log` facade (target `drasi.network`) when the host
installed a logger for the library. A dynamically loaded library has no logger,
so warnings and errors go to standard error and lower levels are dropped.
Retries and skips are logged at warning level with component, output, delivery
ID or sequence and the sanitized error.

## Known limitations

- No metrics yet (deliveries, retries, skips, latency); only logs.
- Pause relies on the host not calling `handle` while paused (D-6); the sink
  does not refuse a call made during pause.
- Batch delivery IDs name runs whose boundaries can differ after a replay;
  per-item deduplication needs the item's `sequence` and position.
- `secretHeaders` is one secret object field; a host resolves it as a whole.
- Response bodies are read only up to 4 KiB; a larger success body is
  discarded with its connection.
- The consumer has no headers option and uses the default pool settings.

## Differences from the legacy HTTP reaction

Deliberate changes (D-9, breaking; no migration of existing deployments):

- **Defaults:** `timeoutMs` 5000 → 60000; no default base URL
  (`http://localhost` before), `url` or `baseUrl` is required; redirects are no
  longer followed.
- **Completion:** 202 is not success unless configured; any status outside
  `successStatusCodes` fails.
- **Failure handling:** legacy dropped permanent "poison" events and advanced
  its checkpoint, and fell back to the default body when a template failed.
  Native `strict` fails the input for every undelivered change; only `skip`
  drops, and it logs each drop.
- **Retries:** jittered, budgeted and `Retry-After`-aware, instead of three
  fixed attempts.
- **Templates:** routes are keyed by output alias, not query ID; the context
  is the truthful change notification (no `query_id`, `query_name`, `data` or
  `metadata`); configured static headers are no longer rendered as templates;
  an empty template `url` uses the default destination.
- **Idempotency:** every request carries `x-drasi-delivery-id`.
- **Secrets and TLS:** secret fields, `secretHeaders`, private CAs and mutual
  TLS are new.
- **Recovery:** the legacy checkpoint/recovery policy is replaced by host
  progress; durable delivery is available through `drasi.network/http-delivery`.
