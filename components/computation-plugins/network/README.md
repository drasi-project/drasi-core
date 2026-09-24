# Native HTTP/gRPC network components

`drasi-computation-network` provides four native ComputationGraph factories for
standard HTTP/gRPC performance comparisons. Plugin identity is
`drasi-computation-network`; native ABI is **1.0.0**, independently of its workspace
package version. Native wire version is **2**, using binary computation envelopes
and bulk MessagePack buffers. Rebuild prototype wire-version-1 native libraries.
Each implementation has version `"1"` and configuration version `1`. The legacy
Source/Reaction/Bootstrap ABI **0.15.0** is unchanged.

| Factory | Native role | Port/schema |
|---|---|---|
| `drasi.network/http-source` | `EnvelopeSource` | `out`, `GraphChangeCodec::schema()` |
| `drasi.network/grpc-source` | `EnvelopeSource` | `out`, `GraphChangeCodec::schema()` |
| `drasi.network/http-sink` | `EnvelopeSink` | `in`, `QueryChangeCodec::schema()` |
| `drasi.network/grpc-sink` | `EnvelopeSink` | `in`, `QueryChangeCodec::schema()` |

Only existing HTTP DTO/conversion helpers and generated gRPC service/protobuf
types are reused from the transport crates. Native execution constructs no legacy
`Source`, `Reaction`, `SourceBase`, `ReactionBase`, `SourceEventWrapper`,
`QueryResult`, adapter or subscription queue. Sinks project typed query rows at
the actual network boundary, using the same core JSON value projection as the
existing output codecs. The host remains the graph scheduler, fanout owner and
continuous-query evaluator.

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

### Volatility, ordering and lifetime

Ingress is **volatile**. Successful network submission means acceptance into the
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

Query-role hosting, native resource injection, durable/adaptive transports,
source recovery, in-place reconfiguration and hot unloading remain unsupported.
