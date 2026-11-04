# Native computation contracts

Shared identifiers, envelopes, schemas, message codecs and component interfaces
belong here rather than in the query runtime. Both the host and native plugins
use the same definitions and encoding implementations.

There is no graph executor, query evaluator, language parser or storage backend
in this package. Sources, transformers and query outputs share the graph-change
contract; query-specific metadata is optional for generic graph consumers.
Shared graph values come from `drasi-core-types`.
Owned-worker helpers use the caller's Tokio runtime and do not create an executor.
`OperationFailures` preserves all typed operation/cleanup causes without requiring
the runtime; `drasi_lib::error` re-exports that same type. `SourceProgressView`
exposes the actual provider's read-only publication and subscriptions, never local
transaction ownership or a copied progress registry.
`QueryResultSnapshot` is a snapshot of result rows, not query configuration.
The former `QuerySnapshot` alias has been removed.

The runtime remains responsible for graph membership, actual storage ownership,
query processing and recovery. Native plugins use explicit host capabilities,
not concrete runtime objects.

The `telemetry` module defines observation catalogues, stable scopes, profiles,
trace-only parent sampling and bounded registration validation. The catalogue,
policy, portable carrier and registration types are backend-neutral. The
optional `metrics-facade` feature adds the shared facade adapter used by the
library and static SDK, avoiding two implementations of recorder registration.
It installs no recorder or exporter and does not add a runtime dependency.

The catalogue distinguishes input envelopes, newly published result-row changes,
and handling attempts. Query engine time excludes result hooks and commit;
bootstrap, scheduled evaluation, and replay retain distinct count semantics.
Persistence entries describe supported framework observations, not engine
statistics. Checkpoint lag requires known, compatible query/checkpoint progress;
unsupported values are absent rather than zero. Ordinary worker implementation
and generation fencing remain host-local; these definitions do not provide
native metric transport.

The host owns one `MetricRegistrationContext` per recorder and shares its clones
across instances. Incompatible same-name definitions fail atomically before
recorder calls; identity values are not part of schema identity. Definitions and
series are bounded and retained rather than silently evicted. The default limits
and migration contract are documented in the
[library guide](../../lib/README.md#telemetry-foundations).

`ExecutionContext` carries an optional validated `TraceContextV1`: trace/span IDs,
sampling flags and bounded W3C trace state. It contains no live span, transaction
or component identity and is not serializable as event data. `InputEnvelope`,
`OutputEnvelope` and `EnvelopeMessage` carry it separately from `ChangeEnvelope`.
Never put tracing metadata in `ProcessingContext`, annotations, rows or receipts.

The `tracing-context` feature supplies `ExecutionScope` for poll-safe restoration
and explicit worker capture. A scope owns local dispatch/component bindings;
only its portable execution context belongs on a work-item queue. The optional
`otel-context` adapter integrates with a host-composed `tracing-opentelemetry`
layer using its default context activation. It installs no subscriber/exporter.
The adapter feature brings the tracing integration's OpenTelemetry dependencies;
DTO-only builds do not enable them.

With `tracing-context`, owned worker helpers distinguish component lifetimes
from child operations. `spawn_owned_component_worker` (and its blocking variant)
retains the receiving component binding with an empty default event context.
`spawn_owned_scoped_worker` / `spawn_owned_scoped_blocking_worker` preserve a
short initiating operation, such as a snapshot. All retain the existing
ownership-before-spawn and cancellation-safe joining contract. The original
`spawn_owned_worker` remains ownership-only.

`TraceContextAdapter::prepare_operation` supplies parent/root and bounded links
before the first span context extraction or entry. `BatchTraceParent` selects a
shared parent or ordered, deduplicated links without retaining an unbounded
intermediate list. The runtime validates positive `TraceLinkLimits`; hosts must
align their count/byte limits with their tracing backend. The optional
`TelemetrySampler(host_root_sampler)` supports configured new-root policies
while always inheriting valid parent decisions. This is host composition, not
an automatically installed provider or exporter.

Rust callers must migrate input/output struct literals to include
`execution_context`, or use the new context-free constructors. Forward the input
context explicitly, or use `OutputEnvelope::with_current_context()` when producing
work inside a restored scope. Raw constructors remain context-free; the host
fills missing processing-output context from the actual producing operation.
Query replay explicitly chooses a new attempt context, never a persisted or
unrelated caller's trace. See the [runtime migration guide](../../lib/README.md#portable-execution-context).

Published envelopes have a maximum ancestry depth of 1,024
(`MAX_LINEAGE_DEPTH`). JSON and binary decoding, encoding and binary size
measurement enforce the same bound. `ChangeEnvelope::try_derive` checks it before
derivation; `derive` may construct a deeper in-memory value, which publication
rejects. Lineage destruction is iterative and preserves other shared branches.
