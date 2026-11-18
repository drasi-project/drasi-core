# Native OpenTelemetry source: design

`drasi.network/otel-source` turns OTLP exports into graph changes. This
document describes how it works. [CONFIGURATION.md](CONFIGURATION.md) lists
every setting and how to operate the source.

## Purpose and scope

The source answers the questions a continuous query needs about running
services: which services exist, what their key metrics currently read, which
services call which, and which recently logged errors. It projects an
allowlisted subset of OTLP data into a bounded, current-state graph. It does
not store telemetry history, aggregate histograms, or serve queries over raw
spans and logs.

Supported input:

- **Metrics:** gauge and sum data points (integer or double). Histograms,
  exponential histograms and summaries are rejected; export a pre-aggregated
  gauge (for example a p99) instead.
- **Traces:** spans of a configured kind (default `CLIENT`) that name a
  destination service through a configured attribute.
- **Logs:** records at or above a minimum severity, optionally limited by an
  event-name allowlist, plus heartbeat events.

Transports: OTLP/gRPC (`opentelemetry.proto.collector.{metrics,trace,logs}.v1`
`Export`, gzip accepted) and OTLP/HTTP with binary protobuf bodies
(`POST /v1/metrics`, `/v1/traces`, `/v1/logs`, gzip accepted). OTLP/JSON and
profiles are not supported.

## Architecture

```
 OTLP/gRPC client ─► grpc_incoming (connection cap, TLS, preface deadline) ─► tonic
                         Access interceptor: rate limit ─► bearer token
 OTLP/HTTP client ─► shared HTTP server (connection cap, TLS, header deadline) ─► axum
                         rate limit ─► bearer token ─► media type ─► bounded body ─► gunzip
                                         │
                         request slot (ingressCapacity)
                                         ▼
                 Engine::ingest ── pure projection (projection.rs)
                                         │  groups of elements
                                         ▼
                 ProjectionState (state.rs): insert or update, caps, TTL schedule
                                         │  one group at a time
                                         ▼
                 Ingress::admit_all ── bounded volatile queue ─► NativeSource::next ─► graph
                                         ▲
                 TTL sweeper (1 s) ── Engine::sweep ── deletes for expired elements
```

| Module | Responsibility |
|---|---|
| `lib.rs` | Factory, metadata, description, plugin export |
| `config.rs` | Configuration, validation, published description, projection settings |
| `component.rs` | Lifecycle, update planning and application, the serving future |
| `grpc.rs` | OTLP/gRPC export services and status mapping |
| `http.rs` | OTLP/HTTP routes, body handling and status mapping |
| `engine.rs` | Serialized projection, group admission with undo, TTL sweep, counters |
| `projection.rs` | Pure OTLP-to-element projection |
| `state.rs` | Live IDs, category counts, TTL schedule, insert-or-update decisions |
| `otlp.rs` | Generated OTLP types and `google.rpc.Status` |

Listener ownership, Pause gating, Drain/Abort Stop and the volatile queue come
from `drasi-computation-network-common` (`SourceRuntime`, `Ingress`), shared
with the native HTTP and gRPC sources. Both protocols run in one activation
(`SourceRuntime::start_listeners`) and share one ingress and one projection
state.

## Graph schema

Every element's namespace is the source component's UUID. Resource attributes
are flattened onto the `Service` node; no Resource node exists.

| Label | Kind | Identity | Properties |
|---|---|---|---|
| `Service` | node | `svc:{namespace}/{name}` or `svc:{name}` | `name`, `namespace?` (`service.namespace`), `environment?` (`deployment.environment.name`, else `deployment.environment`), `instanceId?` (`service.instance.id`), `version?` (`service.version`), `lastSeen`, `registeredAt` (insert only) |
| `Metric` | node | `metric:{serviceId}:{name}[:{k=v,...}]` | `name`, `unit`, `value` (float), `observedAt`, `receivedAt` |
| `Heartbeat` | node | `hb:{serviceId}` | `lastSeen` |
| `LogEvent` | node | `log:{serviceId}:{eventName or body hash}:{timeUnixNano}` | `service`, `severity` (severity text), `body`, `eventName?`, `observedAt` |
| `REPORTS` | Service → Metric | `reports:{metricId}` | — |
| `HEARTBEAT` | Service → Heartbeat | `rel-hb:{serviceId}` | — |
| `DEPENDS_ON` | Service → Service | `dep:{callerId}:{calleeId}` | `lastSeen` |
| `EMITS` | Service → LogEvent | `emits:{logId}` | — |

Timestamps in properties are RFC 3339 strings. Element `effective_from` is the
record's OTLP time converted from nanoseconds to milliseconds, or the receipt
time when the record has no timestamp. A timestamp beyond the graph's
millisecond range rejects that record.

## Projection rules

- **Service.** Every record needs a nonempty resource `service.name`; a
  resource without one rejects all its records. `service.namespace` prefixes
  the identity.
- **Metrics.** A data point becomes a Metric when its name matches
  `metricAllowlist` (`*` is the only wildcard; an empty list admits none).
  Attributes listed in `metricIdentityAttributes` are appended to the identity,
  sorted, so each combination is its own node; other attributes are ignored.
  The same series later updates the same node's `value`.
- **Dependencies.** A span whose kind is in `spanKinds` and that carries the
  first nonempty attribute from `destinationAttributes` yields a `DEPENDS_ON`
  from the resource's service to the named callee. A callee value containing
  `/` is split into namespace and name as-is; a bare name inherits the
  caller's namespace. The callee is created as a placeholder Service; its own
  resource enriches it when that service exports.
- **Logs.** A record at or above `logMinSeverity` whose `event_name` matches
  `logEventNameAllowlist` (when nonempty) becomes a `LogEvent`. A record
  without an event name is identified by a stable FNV-1a hash of its body.
- **Heartbeats.** A data point named `heartbeatMetric` or a log record whose
  event name is `heartbeatEventName` refreshes the service's Heartbeat,
  independently of the metric allowlist, severity and log allowlist.
- **Feedback loops.** With `rejectDerived`, a record whose resource or own
  attributes carry `drasi.source.origin=derived` is rejected, so Drasi output
  exported back through OTLP cannot re-enter.

Every element one record produces forms a **group** (for example Service,
Metric and REPORTS). Groups are admitted or dropped whole, so the graph never
receives a relation without its endpoints.

## State, caps and expiry

`ProjectionState` records every live element ID, a live count per capped
category, and an expiry schedule:

- The first observation of an ID is an **insert**; later ones are **updates**
  that set the projected properties and remove nothing. A Service's
  `registeredAt` is stamped only on its insert, so updates leave it in place.
- **Caps.** A group containing a new Service, Metric, `DEPENDS_ON` or
  `LogEvent` that would exceed `maxServices`, `maxMetrics`,
  `maxDependencies` or `maxLogEvents` is dropped whole and counted. Existing
  elements keep updating at the cap. Lowering a cap below the current count
  admits no new elements until enough expire.
- **TTL.** `DEPENDS_ON` lives `dependencyTtlSecs` and `LogEvent`/`EMITS`
  live `logEventTtlSecs`, measured from **receipt**, never from the OTLP
  event time, so a late batch cannot expire on arrival. Each refresh moves
  the expiry. The sweeper runs every second and emits a delete, effective at
  the sweep time, for each expired element. Services, Metrics and Heartbeats
  have no TTL.

The state is **volatile and per object**: it survives Stop and Start of the
same object (expiries that fell due while stopped are swept after the next
Start) but not reconstruction, which starts empty under a new producer
identity. Changes still queued when Stop completes are discarded undelivered;
Stop then reconciles the state with them. An element whose discarded insert
never reached the graph is forgotten, so its next observation is inserted
again. An element whose discarded update or delete leaves it in the graph but
no longer in the state gets a delete scheduled for the first sweep after
Start, unless it is observed again first.

## Ordering and admission

One mutex serializes projection, admission and expiry, so the
insert-or-update decision always matches the order in which the graph
receives changes. Each change is a separate `GraphChangeCodec` envelope with a
consecutive sequence and a volatile `GraphProducerProgress` identity.

Each group is admitted with `Ingress::admit_all`, which reserves queue
capacity for every change of the group before assigning any sequence. When
admission refuses a group (Pause, Stop, a full queue at the `timeoutMs`
deadline), or the request is cancelled while waiting (an exporter deadline or
a dropped connection), the state that group recorded is undone and the request
fails with a retryable status. Groups admitted earlier in the same request stay
admitted; the exporter's retry updates them again rather than duplicating
them, because identities derive from the data. `ingressCapacity` must be at
least 8 so a group always fits.

Expiry deletes are admitted in queue-sized chunks; a refused chunk stays
scheduled for the next sweep.

**Durability.** An acknowledged export means admission into the bounded
in-memory queue, not persistence or downstream completion. The source does
not offer durable admission: its projection state cannot be persisted
together with a host journal, so journaled output would be inconsistent with
the state of a reconstructed object.

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Nothing to check: configuration was validated at construction, secrets arrive resolved. |
| `start` | Binds every configured listener in `protocols` order, observing cancellation and the deadline. A failed bind closes listeners already bound and leaves nothing running. Then serves both protocols and the sweeper as one owned worker. |
| `pause` | Refuses new exports (gRPC `UNAVAILABLE`, HTTP 503) before projecting, defers expiry, and returns once in-flight admissions settle. A cancelled or expired Pause leaves the source admitting. |
| `resume` | Reopens admission; expiries due while paused are swept at the next tick. |
| `stop` | `Drain` stops accepting and lets in-flight exports finish within the deadline; `Abort` interrupts them. On expiry the listener is retained and a later Stop completes it. Logs the admission counters. |
| `dispose` | The source owns no persistent state; erasing disposal is an idempotent no-op. Requires a stopped source. |

## Management

Updates are classified by the weakest safe boundary; the strongest changed
field wins.

| Boundary | Fields |
|---|---|
| Pause (in place) | `authToken`, `authTokenSecondary`, `allowUnauthenticated`, `rateLimitPerSecond`, `rateLimitBurst`, `metricAllowlist`, `destinationAttributes`, `spanKinds`, `heartbeatMetric`, `heartbeatEventName`, `logMinSeverity`, `logEventNameAllowlist`, `logEventTtlSecs`, `dependencyTtlSecs`, the four caps, `rejectDerived` |
| Stop (restart) | `host`, `protocols`, `grpcPort`, `httpPort`, `timeoutMs`, `ingressCapacity`, `maxMessageBytes`, `maxConnections`, `headerReadTimeoutMs`, TLS fields |
| Replace | `stream`; `metricIdentityAttributes`, which changes every Metric's identity |

Projection settings are read per request: new TTLs apply to elements
refreshed afterwards, new caps to new elements. `apply` refuses (leaving the
object unchanged) when the request's current configuration differs from the
object's, the plan changed, or the proposed configuration is invalid or
exposed.

## Security model

- Listeners bind `127.0.0.1` by default. A non-loopback host requires
  `authToken` or an explicit `allowUnauthenticated: true`; every route admits
  data, so every route is token-protected when a token is set.
- Bearer tokens are compared in constant time; two tokens allow rotation.
  Tokens and the TLS private key are secret fields, never printed by `Debug`.
- The per-client rate limit is charged before authentication, so failed
  attempts are limited too.
- TLS (rustls, ring) with optional mutual TLS serves both protocols. Handshakes
  and request headers must complete within `headerReadTimeoutMs`.
- `maxConnections` bounds connections per listener; `ingressCapacity` bounds
  concurrent exports; `maxMessageBytes` bounds request size before and after
  gzip decompression, so a compression bomb is refused rather than inflated.
- Cardinality caps bound the state an untrusted exporter can create.

## Failure model

| Condition | gRPC | HTTP |
|---|---|---|
| Missing or wrong token | `UNAUTHENTICATED` | 401 |
| Rate limited | `RESOURCE_EXHAUSTED` | 429, `Retry-After` |
| Paused, stopping, overloaded, queue timeout | `UNAVAILABLE` | 503, `Retry-After` |
| Message too large | `OUT_OF_RANGE` (tonic) | 413 |
| Wrong media type or encoding | — | 415 |
| Malformed body | `INTERNAL`/`INVALID_ARGUMENT` (tonic decode) | 400 |
| Records not projected or dropped at a cap | success with `partial_success` | 200 with `partial_success` |

A listener that fails while running ends the activation; the failure is
reported through `next()`. A sweep refused by Pause or Stop is retried at the
next tick.

## Observability

The source logs through the `log` facade (with the standard-error fallback of
a dynamically loaded library): listener addresses at Start, cap drops and
malformed bodies at debug level, and accepted/rejected/dropped/expired
counters at Stop. Per-request rejections are reported to the exporter through
`partial_success`.

## Known limitations

- Ingress is volatile; there is no durable admission, replay or bootstrap.
- Metric types other than gauge and sum are rejected.
- OTLP/JSON, profiles and other compression encodings are not supported.
- Changing `metricIdentityAttributes` requires reconstruction.

## Differences from the legacy source

The legacy `drasi-source-otel` remains for ordinary hosts. This source keeps
its projection, graph schema and identities, with these differences:

- Native lifecycle, management and conformance: real Pause gating, Drain and
  Abort Stop, pure update planning, typed refusals.
- Configuration: `host` + `protocols` + `grpcPort`/`httpPort` replace
  `grpcBind`/`httpBind`; listeners bind loopback by default instead of
  `0.0.0.0`; inline PEM (`tlsCertificatePem`, `tlsPrivateKeyPem`,
  `tlsClientCaPem`) replaces certificate paths; `maxMessageBytes` replaces
  `maxRequestBytes`; unknown fields, unknown span kinds and severities are
  refused instead of ignored.
- Authentication uses secret bearer tokens with rotation and rate limiting;
  identity-provider Basic credentials are not supported.
- `durability` (a WAL of projected changes) and persisted lifecycle state are
  removed; see *Durability* above.
- Admission is all-or-nothing per group with undo on refusal; the legacy
  source could leave state ahead of a failed dispatch.
- OTLP/HTTP now checks the media type, accepts gzip, answers protobuf
  responses with partial success and `google.rpc.Status` errors, and uses the
  OTLP retryable statuses.
- Records with impossible timestamps are rejected rather than failing the
  request.
