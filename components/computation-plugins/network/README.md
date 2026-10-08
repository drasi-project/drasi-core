# Native HTTP/gRPC/SSE network components

`drasi-computation-network` provides five native ComputationGraph factories for
HTTP/gRPC transport and browser SSE delivery. Plugin identity is
`drasi-computation-network`; native ABI is **1.0.0**, independently of its workspace
package version. Native wire version is **2**, using binary computation envelopes
and bulk MessagePack buffers. Rebuild prototype wire-version-1 native libraries.
Each implementation has version `"1"` and configuration version `1`. The legacy
Source/Reaction/Bootstrap ABI **0.17.0** (with 0.16 fast-mode compatibility)
is independently versioned.

| Factory | Native role | Port/schema |
|---|---|---|
| `drasi.network/http-source` | `EnvelopeSource` | `out`, `GraphChangeCodec::schema()` |
| `drasi.network/grpc-source` | `EnvelopeSource` | `out`, `GraphChangeCodec::schema()` |
| `drasi.network/http-sink` | `EnvelopeSink` | `in`, `QueryChangeCodec::schema()` |
| `drasi.network/grpc-sink` | `EnvelopeSink` | `in`, `QueryChangeCodec::schema()` |
| `drasi.network/sse-sink` | `EnvelopeSink` (`Accepted`) | `in`, `QueryChangeCodec::schema()` |

Only existing HTTP DTO/conversion helpers and generated gRPC service/protobuf
types are reused from the transport crates. Native execution constructs no legacy
`Source`, `Reaction`, `SourceBase`, `ReactionBase`, `SourceEventWrapper`,
adapter or subscription queue. HTTP/gRPC sinks project typed query rows at
the actual network boundary, using the same core JSON value projection as the
existing output codecs. SSE reuses the established query-result DTO projection
only to serialize the unchanged browser protocol; it has no legacy execution
adapter or result-processing worker. The host remains the graph scheduler,
fanout owner and continuous-query evaluator.

## Native browser SSE

`drasi.network/sse-sink` consumes query envelopes directly from graph edges:

```json
{
  "queryStreams": {"example-query": "example-query/out"},
  "host": "127.0.0.1",
  "port": 8080,
  "ssePath": "/events",
  "heartbeatIntervalMs": 30000,
  "broadcastCapacity": 1024
}
```

Only `queryStreams` is required; the default host is `0.0.0.0`, with the other
defaults shown above. Each query ID must match its envelope metadata and the
configured stream. Connect every query's output to the sink's `in` port.
The untemplated `queryId`/`results`/`timestamp` data frames, row signatures,
before/after images and heartbeat data frames remain compatible with the
existing SSE browser adapter. Read snapshots through the Server query-results
API; this transport does not own a second result cache or snapshot endpoint.

Delivery is explicitly **volatile, Accepted**, not handled by a browser.
No connected listener is required; disconnected clients receive no history.
Each listener uses the bounded broadcast channel; lag closes that stream rather
than silently delivering a truncated tail. Clients must reconnect and fetch a
fresh snapshot. Stop cancels active streams and joins the HTTP server, retaining
ownership after cancelled or timed-out cleanup. Restart requires that cleanup.
Construction binds no socket or worker.

This focused native factory supports the untemplated protocol used by the
examples, not legacy templates/dynamic paths or recovery snapshot/progress
control envelopes; unsupported configuration rejects. GET/OPTIONS permit
cross-origin reads, as the existing SSE transport does. Configure an
authenticated reverse proxy before exposing private results.

## Opt-in HTTP completion service

The separate Rust `delivery` module provides `HttpDeliveryHandler` and
`HttpDeliveryEndpoint`. None of the factories constructs them automatically;
ordinary configurations, legacy protocols and ABI 1.0 fast-mode binaries are
unchanged. These services are not yet exposed through native service negotiation.

Use `HttpDeliveryHandler` with the shared
[`DeliveryRunner`](../../../lib/docs/computation-graph-qos.md#opt-in-operation-completion)
on the sending side. Supply a reqwest client, the complete endpoint URL, an
`EnvelopeCodec` with the allowed schemas, a message limit and a timeout. Configure
redirects/authentication/TLS on that client explicitly; disable redirects when
delivery must stay at that exact destination.

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
[`PostgresDeliveryHandler`](../../reactions/storedproc-postgres/README.md#opt-in-computationgraph-transactional-delivery)
as that handler. Ordinary nontransactional effects may repeat after failure:
stable keys and an HTTP endpoint do not make them atomic. Retain both progress
stores and the destination's deduplication state across replacement. The upstream
lossless pipe must retain input until sender completion.

Admission allows one active request per endpoint and rejects excess requests
before parsing their bodies. Message limits are explicit and at most **16 MiB**;
confirmation/error replies are limited to **4,096 bytes**. Sender timeouts must be
positive and at most 60 seconds; the endpoint also bounds active requests to
60 seconds. Connection/time-out errors and explicit HTTP 429/503 responses are
retryable only through the runner's configured bounded policy. Permanent handler
failures return 422; conflicting identity/history returns 409. Nothing silently
skips a failed operation.

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

Both sources require `stream`, matching the graph's output stream binding:

```json
{
  "stream": "facilities-db",
  "sourceId": "facilities-db",
  "host": "0.0.0.0",
  "port": 9000,
  "timeoutMs": 60000,
  "ingressCapacity": 1024,
  "maxMessageBytes": 2097152,
  "maxBatchEvents": 1024,
  "adaptiveEnabled": false
}
```

All keys except `stream` have defaults shown above. For gRPC, `port` defaults to
`50051`, `maxMessageBytes` to `4194304`, and `maxBatchEvents` is not accepted.
`host` must be a literal IP address. Port zero permits an ephemeral listener;
the bound address is logged, while the configuration getter preserves the
configured value (getters are not live state registries).

`ingressCapacity` bounds queued events and concurrent admitted HTTP requests/gRPC
streams or unary calls. Excess concurrent submissions reject rather than create
an unbounded waiting queue. A full event queue backpressures admitted requests
until `timeoutMs`; health traffic does not acquire a data admission permit.
Request/message sizes and HTTP batch counts have separate explicit limits.

HTTP exposes `POST /sources/{sourceId}/events`, `POST
/sources/{sourceId}/events/batch` with `{"events":[...]}`, and `GET /health`.
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

gRPC uses the unchanged `drasi.v1.SourceService` definition from
`drasi-source-grpc`. It implements `SubmitEvent`, bidirectional `StreamEvents`
and `HealthCheck`; `RequestBootstrap` explicitly returns `Unimplemented`.
Each accepted streamed input yields `success=true, events_processed=1`:
counts are **deltas**, matching both test-run-host dispatchers' summation.
There is no cumulative final count to accidentally double-count events.
Invalid streamed input yields a failed response and terminates the stream;
admission/transport failures are gRPC errors, not successful EOF.

The gRPC mapping preserves nanosecond-to-millisecond effective timestamps, scalar
integer/float distinctions and cross-source relation endpoints. Nested protobuf
list/object properties retain the legacy gRPC source's JSON-**string** mapping,
which is deliberately different from HTTP. Event and element source IDs must
match `sourceId`; relation endpoint source IDs may differ. Missing/invalid IDs,
unsupported change types and nonfinite protobuf numeric properties reject.

### Optional durable admission

Fast mode remains the default. To enable durable acceptance, bind the source
specification's `admission` dependency to an actual `QosChannel` with producer
admission enabled, and use that same channel for every outgoing edge. Configure
its storage, failure scope, capacity and receipt limits using the
[QoS admission contract](../../../lib/docs/computation-graph-qos.md).
The source's `stream` must match the channel. Missing service support, incompatible
storage, foreign scope or a memory/mixed output path rejects rather than silently
downgrading. Construction still performs no network I/O.

This uses the optional native service-v1 interface. Existing ABI 1.0 binaries
remain usable in fast mode but must be rebuilt for durable admission. No volatile
event queue, local producer sequence or second journal is allocated in durable
mode. HTTP and gRPC acknowledge only after host graph validation and the outgoing
QoS commit. That is not completion or exactly-once execution of downstream effects.

HTTP durable routes are all `POST` under
`/sources/{sourceId}/admission/v1`:

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
[`proto/admission.proto`](proto/admission.proto), reusing the existing event DTO.
The imported `common.proto` declaration is packaged locally for standalone crate
builds; workspace builds reject any difference from the shared source protocol.
It exposes registration, status, retirement, receipt lookup, unary `Admit`, and
bidirectional `StreamEvents`. Each streamed request carries its session and
sequence; each response is a receipt or typed error, never a cumulative count.
A failed event terminates the stream after its error response; preceding receipts
remain valid. Transport failures still require resolving the same input identity.

On a durable source, the old HTTP event routes return 409 and old gRPC submissions
return `FailedPrecondition`; they cannot accidentally accept without the session
protocol. Health remains available and identifies the active mode. Admission
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
(`facilities-db`, without `/out`). The host query's `runtime_compatibility=true`
configuration preserves outward `source_id`, `processed_by` and `result_count`
metadata while still using native `ContinuousQueryFactory` execution. Native
sinks forward that `QueryChangeCodec` metadata without reconstructing legacy
source wrappers. The loopback graph test covers this exact configuration.
Receivers must be listening before starting gRPC sinks. Single-query timing is
the closest scheduling comparison; multi-query runs also include differences
between the native graph controller and ordinary API query-task/queue scheduling.

Construction performs no network I/O and starts no workers. Start binds before
returning, reporting bind errors directly. Listener work has one explicit owner;
gRPC streaming response work is polled by the transport, not detached tasks.
Stop cancels admission/streams, closes the receiver and awaits the listener.
A cancelled stop retains the join handle for subsequent cleanup. Buffered events
discarded during stop are counted and logged. Dropping a `next()` future does not
consume a later event or manufacture successful exhaustion.

## HTTP sink configuration

```json
{
  "queryId": "building-comfort",
  "stream": "building-comfort/out",
  "url": "http://127.0.0.1:9001/reaction",
  "headers": {
    "Content-Type": "application/json",
    "X-Query-Sequence": "building-comfort"
  },
  "timeoutMs": 60000,
  "maxRetries": 3,
  "failurePolicy": "strict"
}
```

`queryId` and the complete `url` are required. `stream` defaults to
`{queryId}/out`. Headers default to `application/json` and
`x-query-sequence=queryId`; explicit conflicting values reject. Invalid/duplicate
case-insensitive header names/values reject. Redirects are disabled.

Each result change produces the existing default HTTP notification, not a bare
query row: `operation` (`ADD`/`UPDATE`/`DELETE`), `queryId`, `sequenceId`,
RFC3339 `timestamp`, optional `before`/`after`, and optional query `metadata`.
Aggregation output maps to `UPDATE`, including absent `before` on its first
emission. Noop/empty changes produce no request. No template rendering or adaptive
batching is claimed; `outputTemplates`, batching and other unknown keys reject.

## gRPC sink configuration

```json
{
  "queryId": "building-comfort",
  "stream": "building-comfort/out",
  "endpoint": "grpc://localhost:50052",
  "timeoutMs": 60000,
  "batchSize": 1,
  "batchFlushTimeoutMs": 100,
  "maxRetries": 5,
  "connectionRetryAttempts": 10,
  "initialConnectionTimeoutMs": 15000,
  "metadata": {"x-query-sequence": "building-comfort"},
  "outputFormat": "canonicalJson",
  "failurePolicy": "strict"
}
```

Only `queryId` is required; other defaults are shown, with `stream` derived from
`queryId` and default metadata `x-query-sequence=queryId`. Endpoint schemes
`grpc://` and plaintext `http://` are supported. TLS is not advertised.

One unchanged `drasi.v1.ReactionService/ProcessResults` request is sent per result
change. `batchSize=1` is the only supported value. `batchFlushTimeoutMs` is inert
for immediate single-item delivery, not a hidden batching timer. Fixed multi-item
and adaptive batching configurations reject explicitly.

Items preserve operation, row signature, before/after, original query sequence,
timestamp and query metadata. The outer query ID/timestamp match that item.
Configured metadata is sent in both the request body and actual gRPC headers.
`canonicalJson` includes the existing canonical `payload` with `rowSignature`;
`proto` omits it. Noop results produce no RPC. No output template support is claimed.

Startup actually attempts the connection, with up to `connectionRetryAttempts`
total attempts and `initialConnectionTimeoutMs` per attempt; errors are not
disguised as successful lazy activation.

### Sink completion, retry, skip and cancellation

Both sinks validate the input port, exact schema, stream, query metadata and row
identity. They reject snapshot/progress-only recovery control messages because
they do not implement snapshot/recovery replacement.

`Handled` means network delivery completed successfully, or a failed delivery was
deliberately skipped under the explicitly configured policy. There is no early
`Accepted` queue. `maxRetries` counts additional attempts after the first; backoff
starts at 100 ms and caps at 5 s. Retries send the same sequence and payload in
order. One multi-change envelope can already have partially delivered before a
later change fails. Retries/cancellation can duplicate external effects.

`failurePolicy="strict"` propagates failure (default). `"skip"` reports every
exhausted delivery to stderr with component/query/sequence/error and deliberately
continues. This is the observer-endpoint-closure behavior required for the
performance comparison; it is not an implementation of the full legacy
`auto_skip_gap` recovery policy. Healthy paired runs should not measure failed
request/retry timings.

Client requests live in host-polled operation futures, not background sink
processing queues. Cancellation drops pending calls/backoff; stop drops owned
client/channel handles after the host drains/cancels operations. The plugin's I/O
runtime and library remain process-pinned according to ABI 1.0. Cancellation is
not rollback or exactly-once delivery.

## Validation and separate-library tests

From `drasi-core`:

`make test-native-network` builds the separate library and runs the package tests.
The workspace-test, coverage and cross-platform FFI workflows also build this
fixture explicitly; a clean checkout must not depend on a stale local dylib.

```sh
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3
cargo build --offline -p drasi-computation-network --features dynamic-plugin
cargo test --offline -p drasi-computation-network
cargo clippy --offline -p drasi-computation-network --all-targets -- -D warnings
cargo build --release --offline -p drasi-computation-network --features dynamic-plugin
```

Do not enable `dynamic-plugin` when compiling the test host. Loopback tests use
public `computation::load` to load the separately built native library. A missing
binary fails explicitly. `DRASI_NATIVE_NETWORK_PLUGIN` can select an alternate
artifact; by default tests use the platform library in `target/debug`.
To exercise the optimized artifact with the same independently compiled test
host on macOS, run:

```sh
DRASI_NATIVE_NETWORK_PLUGIN="$PWD/target/release/libdrasi_computation_network.dylib" \
  cargo test --offline -p drasi-computation-network --test network_loopback
```

Use `.so` on Linux or `drasi_computation_network.dll` on Windows. These loopback
tests validate correctness; the parent paired runner owns 100k-event performance
measurement and its scheduling/queue-path interpretation.

Tests cover legacy DTO/protobuf mapping, numeric/temporal/query metadata,
aggregation and Noop semantics, actual loopback HTTP/gRPC, streamed delta counts,
individual batch evaluation, backpressure, errors, retries/skips, independent
health, cancelled operations, awaited listener stop/rebind, and the unchanged
host continuous-query evaluator between native endpoints.

The durable cases also cover receipt conflicts/expiry, partial-batch prefixes,
retirement, stopped consumers, abrupt process exit without a saved client receipt,
and before/after-commit client timeout or source stop for both transports.

Query-role hosting, arbitrary native resource injection, adaptive transports,
snapshot/live handover, in-place reconfiguration and hot unloading remain unsupported.
