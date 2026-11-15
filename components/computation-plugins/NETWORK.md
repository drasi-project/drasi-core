# Native HTTP/gRPC/SSE network components

The six [independent network component crates](README.md) provide native
ComputationGraph HTTP/gRPC transport, HTTP bootstrap and browser SSE delivery. Each library
registers its own factories and uses its package name as its plugin identity. The
HTTP sink library registers two: the ordinary sink and the opt-in
`drasi.network/http-delivery` consumer.
Native ABI is **4.0.0**, independently of the workspace package version.
Native wire version is **2**, using binary computation envelopes
and bulk MessagePack buffers. Rebuild libraries built against an older native ABI.
Each implementation has version `"1"` and configuration version `1`. The legacy
Source/Reaction/Bootstrap ABI **0.19** is independently versioned; earlier
ordinary layouts are rejected.

| Factory | Native role | Port/schema |
|---|---|---|
| `drasi.network/http-source` | `EnvelopeSource` | `out`, `GraphChangeCodec::schema()` |
| `drasi.network/grpc-source` | `EnvelopeSource` | `out`, `GraphChangeCodec::schema()` |
| `drasi.network/http-sink` | `EnvelopeSink` | `in`, `GraphChangeCodec::schema()` |
| `drasi.network/http-delivery` | consumer (`External`) | `in`, `GraphChangeCodec::schema()` |
| `drasi.network/grpc-sink` | `EnvelopeSink` | `in`, `GraphChangeCodec::schema()` |
| `drasi.network/sse-sink` | `EnvelopeSink` (`Accepted`) | `in`, `GraphChangeCodec::schema()` |
| `drasi.network/http-bootstrap` | `NativeBootstrapProvider` | query-scoped snapshot, `GraphChangeCodec::schema()` |

HTTP webhook/auth/mapping, REST bootstrap, HTTP/gRPC rendering and protocol
helpers are extracted from the existing implementations into shared crates.
Native execution constructs no legacy
`Source`, `Reaction`, `SourceBase`, `ReactionBase`, `SourceEventWrapper`,
adapter or subscription queue. The HTTP, gRPC and SSE sinks accept the standard graph-change
schema from **any** producer — a source, a transformer or a query — through one
decoder (see [Native sink input](SINKS.md)). They project typed changes to the
truthful change JSON at the actual network boundary (`operation`, canonical
`identity`, only the images the record carries), using the same core JSON value
projection as the existing output codecs. No query annotation is required, no
query identity is invented for a source and no evaluator row signature exists. The host remains the graph
scheduler, fanout owner and continuous-query evaluator.

## Native browser SSE

[`drasi-computation-sink-sse`](sinks/sse) supplies `drasi.network/sse-sink`,
consuming standard graph-change envelopes from any producer directly from graph edges:

```json
{
  "outputs": {"example-query": {"component": "bc769198-8a46-426c-acb5-435b084520d0", "port": "out"}},
  "host": "127.0.0.1",
  "port": 8080,
  "ssePath": "/events",
  "heartbeatIntervalMs": 30000,
  "broadcastCapacity": 1024
}
```

Use the producer's assigned UUID in `component`; `example-query` is only an
output alias. No listener setting is required; the default host is `127.0.0.1`, with the other
defaults shown above. A non-loopback host requires `authToken`, or an explicit
`allowUnauthenticated: true` behind an authenticating proxy. Each input's stream selects the declared incoming output
(a real component and output port of a source, transformer or query); a stream
that is not a declared edge fails. Connect every output to the sink's `in` port. The untemplated data frame is
`{"output":{"alias","component","port"},"sequence","publishedAt","changes":[…]}`
where each change states its real `operation` (`ADD`, `REPLACE`, `PATCH`,
`DELETE`), canonical `identity` and only the images the record carries (see
[Native sink input](SINKS.md)); `publishedAt` is the sink's own wall clock in
milliseconds, not an event time. Heartbeat frames are `{"type":"heartbeat","ts":…}`.
Patches and identity-only deletes are transmitted as such: the sink keeps no row
cache, and the `@drasi/react` client owns applying them to its identity-keyed
rows. The optional `output_snapshots` dependency exposes `/snapshot/:alias` for any
bound output (source, transformer or query) as a JSON array of canonical `ADD`
changes, read through the graph's output catalogue; an output without a snapshot
provider, or a sink without the capability, answers `501`. It adds no result
cache or query executor. The Server query-results and snapshot-first
subscription APIs remain available separately.

Delivery is explicitly **volatile, Accepted**, not handled by a browser.
No connected listener is required; disconnected clients receive no history.
Each listener uses the bounded broadcast channel; lag closes that stream rather
than silently delivering a truncated tail. Clients must reconnect and fetch a
fresh snapshot. Pause is a real gate: new subscriptions and snapshot reads get
`503` with `Retry-After`, heartbeats stop and connected subscribers receive only
keep-alive comments until Resume. Stop honours `StopMode` and the lifecycle
deadline: `Drain` ends streams and joins the server gracefully, `Abort` closes
connections immediately, and a timed-out Drain retains ownership so a later
Stop finishes it. Restart requires that cleanup. Construction binds no socket
or worker.

The native sink also reuses the legacy SSE renderer for per-query/default
templates and static/dynamic paths, with explicit path-count bounds.
Output snapshot readers have bounded admission, cancellation and host
runtime ownership. Incoming recovery snapshot/progress control envelopes remain
unsupported; configuration does not pretend otherwise.

The listener uses the shared network server: optional bearer authentication
(`authToken`, `authTokenSecondary`, also accepted as `?access_token=` for
browser `EventSource` clients unless `allowQueryToken` is false), optional TLS
and mutual TLS, `maxConnections`, `maxSubscribers`, `headerReadTimeoutMs` and
an optional per-client rate limit. CORS is off unless `corsAllowedOrigins` is
set. See [DESIGN.md](sinks/sse/DESIGN.md) and
[CONFIGURATION.md](sinks/sse/CONFIGURATION.md) for every field, the management
boundaries and the snapshot resource recipe.

## Opt-in HTTP completion service

The `drasi_computation_sink_http::delivery` module provides `HttpDeliveryHandler` and
`HttpDeliveryEndpoint`. The `drasi.network/http-delivery` factory constructs
`HttpDeliveryHandler` as a `ConsumerMode::External` consumer, so the host's shared
[`DeliveryRunner`](../../lib/docs/computation-graph-qos.md#opt-in-operation-completion)
drives it; configuration (URL, secret token, timeouts, message limit, client TLS)
is listed in [the HTTP sink configuration](sinks/http/CONFIGURATION.md#http-delivery-consumer-fields).
The ordinary `drasi.network/http-sink` never uses it. `HttpDeliveryEndpoint` is a
reference receiver that an application hosts itself (feature `delivery-receiver`).

The handler can also be constructed directly: supply a reqwest client, the
complete endpoint URL, an `EnvelopeCodec` with the allowed schemas, a message limit
and a timeout, and configure redirects/TLS on that client explicitly. Each request
carries `x-drasi-delivery-id: {identity stream}/{sequence}` and, when configured,
a bearer token.

The reference endpoint exposes `POST /drasi/delivery/v1`. A JSON request contains
`version: 1`, the input `port`, expected batch `identity` and the complete
`EnvelopeCodec` JSON `envelope`. The receiver decodes/validates the actual envelope
and derives the identity again using its **configured consumer scope** before
allowing any effects. Sender and receiver must represent the same logical
consumer scope, graph and component ID. A supplied digest is never accepted as
proof of the request's content.

One request carries the entire batch, not another full-envelope encoding for
each operation. The receiver runs operations in order and retains partial
progress. Only after the entire batch completes does it return HTTP **200** with
the exact `DeliveryBatchIdentity`. The sender checks that response before
advancing its local progress; HTTP 202, a different identity, malformed replies
and missing replies are not completion. Remaining local operations reuse that
one checked confirmation. Use JSON parsers that preserve unsigned 64-bit integers.

The receiver owns a dedicated `DeliveryRunner` and an explicit `DeliveryHandler`.
For once-only PostgreSQL effects, use
[`PostgresDeliveryHandler`](../reactions/storedproc-postgres/README.md#opt-in-computationgraph-transactional-delivery)
as that handler. Ordinary nontransactional effects may repeat after failure:
stable keys and an HTTP endpoint do not make them atomic. Retain both progress
stores and the destination's deduplication state across replacement. The upstream
lossless pipe must retain input until sender completion.

Admission allows one active request per endpoint and rejects excess requests
before parsing their bodies. Message limits are explicit and at most **16 MiB**;
confirmation/error replies are limited to **4,096 bytes**. Sender timeouts must be
positive and at most 60 seconds; the endpoint also bounds active requests to
60 seconds. Transport errors (connect, timeout, in flight) and HTTP 408, 425, 429
and 5xx responses are retryable only through the runner's configured bounded
policy. Permanent handler failures return 422; conflicting identity/history
returns 409. Error replies carry only a generic message; internal causes are
logged. Nothing silently skips a failed operation.

Construction spawns no workers or listener. The application owns the HTTP server
and destination connection driver. Call endpoint `shutdown()` to reject/cancel
requests and finish progress-store cleanup; stop and join the HTTP server, then
join the destination driver before reconstruction. Configure authentication and
TLS outside this reference router before exposing it to untrusted clients.

Six real HTTP cases cover lost replies, two-sided reconstruction, partial SQL
effects, forged input/confirmations, exact limits, full-width sequences, bounded
admission and shutdown cancellation. A nontransactional fixture explicitly
demonstrates repeated effects. Run them with:

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo test -p drasi-reaction-storedproc-postgres --test delivery http:: -- --test-threads=1
```

## Source configuration

The authoritative per-source references are the HTTP source
[configuration](sources/http/CONFIGURATION.md) and [design](sources/http/DESIGN.md)
documents, and the gRPC source [configuration](sources/grpc/CONFIGURATION.md)
and [design](sources/grpc/DESIGN.md) documents. This section summarizes what
the two sources share.

Both sources require `stream`, matching the graph's output stream binding, and
share these listener settings:

- **Address:** `host` (default `127.0.0.1`, which must be a literal IP) and
  `port`.
- **Deadlines and limits:** `timeoutMs`, `ingressCapacity`, `maxMessageBytes`
  and `maxConnections`.
- **Access control:** bearer tokens (`authToken`, plus `authTokenSecondary` for
  rotation, both secret), `allowUnauthenticated`, and per-client rate limiting
  (`rateLimitPerSecond`, `rateLimitBurst`).
- **TLS:** `tlsCertificatePem`, `tlsPrivateKeyPem` (secret) and the optional
  mutual-TLS `tlsClientCaPem`.

A non-loopback listener requires authentication, or an explicit
`allowUnauthenticated: true` behind an authenticating proxy. Port zero permits
an ephemeral listener; the bound address is logged.

`ingressCapacity` bounds the volatile event queue and the number of concurrent
admitting requests. gRPC streams use a separate `maxConcurrentStreams` limit.
Excess submissions are rejected rather than queued without bound.
Request and message sizes and HTTP batch counts have their own explicit limits.

Both sources implement real Pause gating: new admissions are refused with a
retryable status until Resume. Stop honours `Drain` and `Abort`. Both support
managed updates at the Pause, Stop and Replace boundaries; the configuration
documents list the boundary for each field.

HTTP exposes `POST /sources/{componentId}/events`, `POST
/sources/{componentId}/events/batch` with `{"events":[...]}`, and `GET /health`,
where `componentId` is the source component UUID.
The existing `HttpSourceChange` JSON shape is preserved: node/relation insert,
update and delete, labels, IDs, properties and relation `from`/`to` endpoints.
HTTP timestamps are nanoseconds, converted to millisecond `effective_from`.
Integers/floats and nested lists/objects retain the existing HTTP conversion.

HTTP batches validate all events before admission. Every event remains a separate
graph envelope/evaluation; a batch never collapses evaluations. Concurrent
submissions are serialized during admission. Success responses contain
`success`, `message`, and `events_processed`; failures also contain `error`.
A timeout/stop after a partially admitted batch reports failure and its accepted
prefix count. Retrying that batch can duplicate already accepted events.

HTTP also supports the published webhook configuration: custom routes, HMAC and
bearer checks, JSON/XML/YAML/text parsing, conditions, node/relation mappings,
explicit timestamps and CORS. Its canonical camelCase DTO is converted through
the existing mapping engine, not a legacy source adapter. Webhook authentication
failures always return 401. In volatile mode, `adaptiveEnabled` enables bounded,
cancellation-safe prefetch; each prefetched event remains a separate graph
envelope. Durable admission stays host-scheduled and does not allocate an
adaptive local queue.

gRPC uses the unchanged `drasi.v1.SourceService` definition from
`drasi-source-grpc`. It implements `SubmitEvent`, bidirectional `StreamEvents`
and `HealthCheck`; `RequestBootstrap` explicitly returns `Unimplemented`.
Each accepted streamed input yields `success=true` and a **cumulative**
`events_processed` total. This preserves the legacy SourceService contract and
both test-run-host dispatchers' exact-final-total checks. Clients must not sum
these counts. Invalid streamed input yields a failed response with the accepted
prefix count and terminates the stream;
admission/transport failures are gRPC errors, not successful EOF.

The gRPC mapping preserves nanosecond-to-millisecond effective timestamps, scalar
integer/float distinctions and cross-source relation endpoints. Nested protobuf
list/object properties retain the legacy gRPC source's JSON-**string** mapping,
which is deliberately different from HTTP. Event and element source IDs must
match the source component UUID; relation endpoint source IDs may differ. Missing/invalid IDs,
unsupported change types and nonfinite protobuf numeric properties reject.

### Optional durable admission

Fast mode remains the default. To enable durable acceptance, bind the source
specification's `admission` dependency to an actual `QosChannel` with producer
admission enabled, and use that same channel for every outgoing edge. Configure
its storage, failure scope, capacity and receipt limits using the
[QoS admission contract](../../lib/docs/computation-graph-qos.md).
The source's `stream` must match the channel. Missing service support, incompatible
storage, foreign scope or a memory/mixed output path rejects rather than silently
downgrading. Construction still performs no network I/O.

This uses the optional native service-v1 interface with ABI 4.0 libraries.
Fast-mode support alone does not provide durable admission. No volatile
event queue, local producer sequence or second journal is allocated in durable
mode. HTTP and gRPC acknowledge only after host graph validation and the outgoing
QoS commit. That is not completion or exactly-once execution of downstream effects.

HTTP durable routes are all `POST` under
`/sources/{componentId}/admission/v1`, where `componentId` is the source UUID:

| Route | Request | Successful response |
|---|---|---|
| `/producers/register` | `{"producer":"client-name"}` | Session: `incarnation`, `producer`, `epoch` |
| `/producers/status` | `{"session":...}` | Session, `next_sequence`, `earliest_receipt` |
| `/producers/retire` | `{"session":...}` | `{"retired":true}` after all subscriber obligations finish |
| `/receipts` | `{"session":...,"sequence":1}` | Receipt, or `null` only for the next unaccepted sequence |
| `/events` | `{"session":...,"sequence":1,"event":...}` | `{"receipts":[...],"error":null}` |
| `/events/batch` | `{"session":...,"first_sequence":1,"events":[...]}` | The same receipt-list shape |

Pass the complete returned session unchanged. Sequences start at 1 and increase
consecutively; a batch covers consecutive numbers, not one atomic transaction.
Each receipt contains the session, client `sequence`, and shared journal
`position`. Batches validate/convert their events first, then admit individually.
Failure returns the committed prefix receipts and a typed error; an empty list
accompanies rejection before admission. Do not mistake an unknown result for
proof that its next event was rejected.

Durable HTTP events require an explicit timestamp. Both transports normalize to
their existing graph-change mapping; map ordering and receiving-side clocks do
not affect retries. Effective timestamps remain milliseconds after the existing
nanosecond conversion, and the envelope timestamp uses that client event time.
Retry the same normalized event, session and sequence. A changed retained event
conflicts; an expired receipt cannot be resubmitted as new input.

Application errors have `kind` and `message`. Busy/full is HTTP 429,
closed/unknown acceptance is 503, conflicts are 409, expired sessions/receipts
are 410, and invalid input is 400. Normal HTTP router/body/JSON errors can precede
these application responses. `AcceptanceUnknown`, `MetadataUnknown`, a timeout,
disconnect or unread response requires lookup or retry of the original identity.
Never change producer identity merely to retry an uncertain request.
Registration is idempotent while that producer name is active. Retirement
invalidates the old session and a new registration obtains a new epoch.

gRPC adds the separate `drasi.admission.v1.AdmissionService` defined in
[`admission.proto`](sources/grpc/proto/admission.proto), reusing the shared source event DTO.
The build script compiles it against the shared source messages in
`drasi-grpc-common`; there is no local copy of `common.proto`.
It exposes registration, status, retirement, receipt lookup, unary `Admit`, and
bidirectional `StreamEvents`. Each streamed request carries its session and
sequence; each response is a receipt or typed error, never a cumulative count.
A failed event terminates the stream after its error response; preceding receipts
remain valid. Transport failures still require resolving the same input identity.

On a durable source, the standard HTTP event routes, webhooks and the gRPC
`SourceService` calls commit through the source's own **server-sequenced
producer**. That producer is registered under the source's component UUID,
which clients therefore cannot register.

- A request is acknowledged only after every event in it is committed.
- Delivery is at-least-once: a client retry after a lost response is admitted
  as a new event.
- After any uncertain or out-of-sequence result, the source re-reads the
  producer's committed head from the host before admitting again. Health remains available and identifies the active mode. Admission
retains existing request/body limits and deadlines; host outstanding operations
and graph pending requests are independently bounded. Listeners report failure
separately from event publication and are joined during cleanup.

### Fast-mode volatility, ordering and lifetime

Without an admission binding, ingress is **volatile**. Successful network submission means acceptance into the
bounded component queue, not persistent storage, query completion or sink
completion. Every event carries a real nonpersistent `GraphProducerProgress`
identity; existing persistent graph processing can reject this upstream.
No WAL, resume position, source recovery handle, durable input or bootstrap claim
is made. Initial performance-test graph data is ordinary ordered insert traffic.

For the paired comparison, bind the source stream to its source ID
(`facilities-db`, without `/out`). Queries use the single
`ContinuousQueryFactory` execution path, without a compatibility switch.
Query results reach HTTP/gRPC as ordinary result nodes; the sinks state each
change's real `operation` and canonical `identity`, and no `source_id`,
`processed_by`, `result_count` or query identity is synthesized. The loopback graph
test covers this exact configuration.
Receivers must be listening before starting gRPC sinks. Single-query timing is
the closest scheduling comparison; multi-query runs also include differences
in input/output component wiring and queue policies, not a different query engine.

Construction performs no network I/O and starts no workers. Start binds before
returning, reporting bind errors directly. Listener work has one explicit owner;
gRPC streaming response work is polled by the transport, not detached tasks.
Stop cancels admission/streams, closes the receiver and awaits the listener.
A cancelled stop retains the join handle for subsequent cleanup. Buffered events
discarded during stop are counted and logged. Dropping a `next()` future does not
consume a later event or manufacture successful exhaustion.

For graph-owned durable admission, ordinary graph stop first waits for an
executing journal/producer transaction to finish before invoking plugin cleanup.
A graceful deadline leaves that operation owned; after it finishes, stop can be
retried. To deliberately interrupt the write, use explicit force cancellation
over every registered user of its actual storage domain, then clean up and
reconstruct before resolving the original session/sequence. Forced interruption
is not a successful graceful drain or proof that the write rolled back.

## HTTP sink configuration

```json
{
  "outputs": {
    "building-comfort": {
      "component": "building-comfort", "port": "out"
    }
  },
  "url": "http://127.0.0.1:9001/reaction",
  "timeoutMs": 60000,
  "maxRetries": 3,
  "failurePolicy": "strict"
}
```

Supply `url` or `baseUrl`, and optionally `outputs` (aliases for incoming
source, transformer or query outputs); `baseUrl` alone selects
`/changes/{alias}`, where an unaliased output is named after its component.
Every request carries `x-drasi-component` and `x-drasi-port` naming the real
producing output and a stable `x-drasi-delivery-id`; configuring any of them,
including in template headers, rejects at construction. `token`,
`secretHeaders` and `tlsClientPrivateKeyPem` are secret fields. Redirects are
never followed. Success means a status in `successStatusCodes`
(`[200, 201, 203, 204]` by default; 202 only when configured). Retries are
jittered, honour `Retry-After` on 429/503 and are bounded by `maxRetries` and
`retryBudgetMs`.

Each graph change produces one truthful notification: `operation` (`ADD`,
`REPLACE`, `PATCH`, `DELETE`), `output` (`alias`, `component`, `port`),
`sequence`, an RFC3339 `timestamp` only when the envelope carries one, the
canonical `identity`, and only the `element`, `before`, `after`,
`removedProperties` and `effectiveFrom` that the record carries. For a patch,
`after` holds only the properties it sets and an explicit `null` stays `null`.
Empty change sets produce no request; a scheduling control or recovery frame
fails. `outputTemplates` render body/URL/method/headers with
`added`/`updated`/`deleted` routing keyed by output alias; templates compile at
construction and a render failure is a per-change failure under
`failurePolicy`, never a fallback. `adaptive` plus `batchEndpoint` enables a
`{"batch":[...]}` protocol with host-owned collection and timers, one batch per
output. `maxBatchBytes` bounds collected inputs; `maxRequestBytes` splits
rendered requests. See [DESIGN.md](sinks/http/DESIGN.md) and
[CONFIGURATION.md](sinks/http/CONFIGURATION.md).

## gRPC sink configuration

```json
{
  "outputs": {
    "building-comfort": {
      "component": "building-comfort", "port": "out"
    }
  },
  "endpoint": "grpc://localhost:50052",
  "timeoutMs": 60000,
  "batchSize": 1,
  "batchFlushTimeoutMs": 100,
  "maxRetries": 5,
  "retryBudgetMs": 120000,
  "connectTimeoutMs": 10000,
  "outputFormat": "canonicalJson",
  "failurePolicy": "strict"
}
```

Supply `outputs`; other defaults are shown. Plaintext `grpc://` and `http://`,
and TLS `grpcs://` and `https://` endpoints are supported, with an optional
private CA, client certificate (mutual TLS) and `tlsDomainName`. Credentials go
in the secret `secretMetadata` field.

The new `drasi.v1.ChangeService/ProcessChanges` protocol carries an
`OutputIdentity` (`alias`, `component`, `port`) and ordered `Change` items; the
legacy `ReactionService/ProcessResults` query-result protocol is not used by the
native sink. It supports fixed and adaptive batches. Use the flat
`batchSize`/`batchFlushTimeoutMs` settings or the legacy `batching` object.
Count, byte and timeout limits flush real batches; output or rendered-metadata
changes split RPCs without reordering items.

Each item states its `operation` (`ADD`, `REPLACE`, `PATCH`, `DELETE`), its
canonical `identity` (`namespace`, `id`), an `element` shape only when the record
carries one, a typed `before` or `after` only when present (a patch's `after`
holds only the properties it sets, with `removed_properties` naming removals), the
producing stream's `sequence` and a `timestamp` only when the envelope has one.
There is no `row_signature`. The batch timestamp is the latest item timestamp, if any.
Configured metadata is sent in both the request body and actual gRPC headers
together with `x-drasi-component` and `x-drasi-port`.
`canonicalJson` includes the canonical JSON `payload`; `proto` omits it unless a
body template is configured. `outputTemplates` reuses the legacy renderer and
metadata templates without changing typed images. Every request also carries
`x-drasi-delivery-id`. See [DESIGN.md](sinks/grpc/DESIGN.md) and
[CONFIGURATION.md](sinks/grpc/CONFIGURATION.md).

Connection is lazy by default: Start does not contact the receiver, and the
first delivery connects within `connectTimeoutMs` and the retry budget. With
`requireConnectedAtStart: true`, Start connects with retries bounded by its own
cancellation and deadline, and fails rather than reporting a lazy success.

### Sink completion, retry, skip and cancellation

Both sinks validate the input port, exact schema and declared incoming stream.
They reject scheduling controls and snapshot/progress-only recovery control
messages because they do not implement snapshot/recovery replacement. No query
annotation is read.

`Handled` means network delivery completed successfully, or a failed delivery was
deliberately skipped under the explicitly configured policy. There is no early
`Accepted` queue. HTTP success is a status in `successStatusCodes` (default 200,
201, 203, 204; a queued `202` is not completion unless configured). A gRPC
success must also acknowledge the exact requested item count; partial or
excessive counts fail, and `success: false` is a permanent rejection.

`maxRetries` counts additional attempts after the first, and `retryBudgetMs`
bounds the total retry time of one request. Backoff is exponential with full
jitter (100 ms doubling to a 5 s ceiling) and honours `Retry-After` (HTTP) or
`grpc-retry-pushback-ms`/`retry-after` (gRPC). Failures are classified by HTTP
status, `tonic::Code` or transport error kind, never by message text; permanent
failures are not retried. Retries send the same sequence, payload and
`x-drasi-delivery-id` in order, so a receiver can deduplicate them. One
multi-change envelope can already have partially delivered before a later change
fails, and retries or cancellation can duplicate external effects.

A template that fails to render is a per-change failure under `failurePolicy`;
it never falls back to the default body. `failurePolicy="strict"` propagates
failure (default). `"skip"` logs every exhausted delivery as a sanitized warning
with component/output/sequence/error and deliberately continues. It is not an implementation of the full legacy `auto_skip_gap`
recovery policy. The native HTTP strict path does not silently drop permanent
poison outcomes as the legacy checkpoint loop did. Healthy paired runs should not measure failed
request/retry timings.

Client requests live in host-polled operation futures, not background sink
processing queues. Cancellation drops pending calls/backoff, so `Abort` ends an
in-flight delivery at once and `Drain` waits at most for the retry budget; stop drops owned
client/channel handles after the host drains/cancels operations. The plugin's I/O
runtime and library remain process-pinned according to ABI 4.0. Cancellation is
not rollback or exactly-once delivery.

The optional batching extension leaves frozen native metadata/base layouts
unchanged. The graph retains collected inputs, local work ownership and the
actual acknowledgement future across interruption. Retained/QoS receivers keep
partially collected leases on the graph-owned receiver and prohibit retirement
or another delivery while those leases remain outstanding. Successful batches
advance acknowledgements in order, never past an incomplete prefix. Byte limits
include the native MessagePack framing; a larger single input uses direct
handling instead of becoming an unsendable multi-input request.

## HTTP bootstrap

The [native HTTP bootstrap](bootstrappers/http/README.md) is a bootstrap-only
library using the shared legacy REST reader, authentication/OAuth, pagination,
parsing and response mapping. Bind a provider to a query in the hosting DrasiLib
graph, with the same source/element identity as its live source. Bounded output,
response and duration limits, owned cancellation and explicit incomplete-snapshot
failures apply to static and dynamic loading. REST bootstrap establishes no
atomic snapshot/live replay boundary and returns no invented source watermark.
No gRPC bootstrap is claimed or implemented.

## Validation and separate-library tests

From `drasi-core`:

`make test-native-network` builds the six separate libraries, runs shared/SSE
unit tests and runs the Host SDK wire/loopback tests. The workspace and
cross-platform FFI workflows build these prerequisites explicitly; a clean
checkout must not depend on stale local libraries.

```sh
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3
make build-native-network-plugins
cargo test --offline -p drasi-host-sdk --test native_network_loopback --test native_network_wire
cargo clippy --offline -p drasi-computation-network-common -p drasi-computation-sink-sse --all-targets -- -D warnings
```

Do not enable `dynamic-plugin` when compiling the test host. Loopback tests use
public `computation::load` to load the separately built native libraries. Each
must export exactly the selected component. Missing binaries fail explicitly.
By default tests use the platform libraries in `target/debug`; set
`CARGO_TARGET_DIR` to select another build directory or `DRASI_NATIVE_PLUGIN_DIR`
to select an explicit directory of separately built libraries.

Use `.dylib` on macOS, `.so` on Linux or `.dll` on Windows. These loopback
tests validate correctness; the parent paired runner owns 100k-event performance
measurement and its scheduling/queue-path interpretation.

Tests cover legacy source DTO/protobuf mapping, numeric/temporal values,
truthful add/replace/patch/delete projection, untimed identity-only deletion,
multi-port output identity and Noop semantics, actual loopback HTTP/gRPC, cumulative stream counts,
individual batch evaluation, backpressure, errors, retries/skips, independent
health, cancelled operations, awaited listener stop/rebind, and the unchanged
host continuous-query evaluator between native endpoints.
The HTTP/gRPC family modules additionally run static and dynamic DrasiLib
hosting, webhook/auth/CORS and adaptive ingress cancellation, HTTP bootstrap
followed by live updates, template and metadata routing, real multi-item
delivery, exact acknowledgements, byte splitting and snapshot cleanup.

The durable cases also cover receipt conflicts/expiry, partial-batch prefixes,
retirement, stopped consumers, abrupt process exit without a saved client receipt,
and before/after-commit client timeout and forced cancellation for both transports.
Separate cases prove graceful commit completion and deadline/retry behavior,
including retry on both runtime flavors. Every interruption case retains exact
acceptance and downstream-delivery checks after reconstruction.

Query-role plugin hosting, arbitrary native resource injection,
snapshot/live handover, in-place reconfiguration and hot unloading remain unsupported.
