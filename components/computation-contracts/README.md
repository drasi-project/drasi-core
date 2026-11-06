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
unsupported values are absent rather than zero. `TelemetryLifetime` supplies
shared revocation, and `LocalMetricBinding` supplies generation-local counter
accounting for static emitters and native Host SDK callbacks. Cloned/re-registered
handles share the same local total; absolute updates become host increments.
Retirement resets registered custom gauges and rejects stale updates.

Bounded span/log/metric records are transport data, not Rust ABI layouts.
`PluginTelemetry` is a local integration interface; the native SDK translates it
to the separately negotiated telemetry ABI. Native span collection and metric
updates do not require the optional OTel adapter. With `otel-context` and
`metrics-facade`, `OpenTelemetrySpanProcessor` lets native completed records and
ordinary host spans share one host processor without replacing IDs/timestamps.

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

The carrier also supports an optional transient `HandledBranchContext`: either
an owned timing lease or an explicit unavailable reason. Trace replacement keeps
this ownership intact; batch trace-parent selection does not select a timing
origin. The frozen V1 `encode()` rejects timing-bearing contexts rather than
silently discarding them. `without_handled_branch()` is only for an explicitly
diagnosed unsupported boundary; it is not permission to inherit a new origin
on the receiving side.

`HandledBranchTracker` is a backend-neutral implementation of host-owned timing
state, created once per graph only when explicitly enabled. It supports bounded origin/lease/branch
records, idempotent required-row completion, once-only branch success, and cleanup
after the last live lease without age-based expiry. Native ownership APIs return
opaque owner/lease identities; clocks and Rust objects never become transport
payloads. Acquisition and release must be balanced separately from the native
callback context's own retain/release lifetime.

The default bounds are 16,384 accounting records and 8 MiB of record/key/row data.
Map and allocator overhead is additionally record-count-bounded, not included in
that byte counter or claimed as process RSS. On capacity loss, further new branch
admission for that origin is sealed while already admitted branches can finish.
Typed errors and `HandledBranchDiagnostics` expose rejected work; there is no
overflow queue, eviction of a live branch, or unbounded completed-history cache.

Enable the runtime measurement with
`TelemetrySettings::with_handled_branch_latency(HandledBranchLimits::default())`.
It remains off in every profile unless opted in, and metric disablement/exclusions
still take precedence. The histogram is in seconds, per fully handled
query-output envelope and sink, with `query_id` and `phase=steady`. Queue
acceptance, administrative forwarding, progress confirmation and acknowledgement
are not completion. The host starts at framework source publication/admission,
not a persisted event timestamp or a source's polling wait.

Native timing uses separately negotiated telemetry V3 frames and host callbacks.
V1/V2/base tables and event/persisted codecs stay unchanged. Imported carriers
acquire host leases; reply buffers retain them until the receiving host has
imported them or released the buffer. Never substitute native clocks. Capability
gaps and missing/ambiguous origins are exposed in diagnostic snapshots, not
fabricated histogram observations. Reconstructed output, bootstrap and
scheduled-only work without an original live timing owner remain untimed.

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
