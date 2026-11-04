# Native ComputationGraph plugin SDK

This is a separate plugin family, not a replacement or layout change to the
Source/Reaction/Bootstrap SDK. It uses `drasi-computation-plugin-abi` **1.0.0**
with native **wire version 1**, independent of workspace package versions.
Legacy ABI **0.17.0** (also accepting 0.16 fast-mode plugins) is independently versioned. Rebuild native plugins with a matching SDK; binaries built for a different
wire version are rejected.

## Authoring

A plugin exports configuration-only factories for this SDK's `NativeSource`,
`NativeTransformer`, `NativeSink` and `NativeService` interfaces. Shared messages,
schemas and codecs come from `drasi-computation-contracts`; graph and result
values come from `drasi-core-types`. None requires the graph runtime, query
evaluator, query parsers or legacy plugin SDK.

1. Implement `NativeComponent` and the appropriate processing trait, including
   the real `NativeComponent::configuration()` persistence hook.
2. Implement this SDK's `Factory`. Its `metadata()` declares implementation and
   configuration versions, a `ConfigSchema`, fixed port/schema descriptors, sink
   completion and explicit `Capabilities`. Its synchronous `create()` receives a
   `CreateRequest` and `ControlSender`, returning a `CreatedComponent`.
3. Register factories and executable schemas in `PluginDefinition::new`.
   A sink that can replace its state from a snapshot returns a
   `SnapshotConsumer` from `NativeSink::snapshot_consumer(port)` for each such
   input (no I/O, fixed at construction) and lists exactly those ports in
   `Capabilities::snapshot_inputs`. Export rejects duplicate, unknown,
   output-direction or non-sink claims, and any construction whose handlers
   differ from the list; snapshots on an input without a handler fail.
4. Export only behind the plugin's `dynamic-plugin` feature:

```rust,ignore
use std::sync::Arc;
use drasi_computation_plugin_sdk::{PluginDefinition, export_computation_plugin};
use drasi_computation_contracts::GraphChangeCodec;

fn definition() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        "example/native",
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(MySourceFactory), Arc::new(MyTransformerFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
export_computation_plugin!(definition());
```

### Runtime-independent implementations

Replace runtime-only imports with `drasi-computation-contracts` data types and
the SDK's `NativeComponent`/processing traits. A native source receives only a
`SourceProgressView`, not the host's `QuerySourceProgress` owner. It declares
`replay_retention()` separately when needed; the SDK still verifies the exact
negotiated reader and consumer identity. Bootstrap factories return
`NativeBootstrapProvider`. Graph-local control handlers, admission owners,
query APIs and construction contexts are intentionally absent from these
plugin-side traits.

The Host SDK adapts the existing binary operations to graph-owned interfaces.
No second executor or legacy compatibility layer is introduced. Build the host
and native libraries from matching sources; the ABI layout version alone does
not establish compatibility with the current data definitions.

Use `make check-native-dependencies` to ensure an independently selected native
plugin build cannot regain an engine/runtime dependency through a helper crate.

Declare the dynamic library feature and explicit native family metadata in the
plugin manifest so `xtask` can discover it independently of legacy crate categories:

```toml
[lib]
crate-type = ["lib", "cdylib"]

[features]
dynamic-plugin = []

[package.metadata.drasi-plugin]
abi-family = "computation"
abi-version = "1.0.0"
kind = "example-native"
```

Keep `abi-version` equal to `drasi_computation_plugin_abi::ABI_VERSION`, not the
plugin's package version. Constructors and metadata discovery must not perform
I/O, start tasks, activate components or inspect mutable external state. A
factory returns one of:

```rust,ignore
Component::Source(Box::new(source)).into()
Component::Transformer(Box::new(transformer)).into()
Component::Sink(Box::new(sink)).into()
Component::Service(Box::new(service)).into()
```

`CreatedComponent` can additionally carry an `Arc<dyn NativeControlHandler>`.
Ports, role, capabilities, sink completion, configuration and transaction schemas
are checked against metadata at construction. The SDK supplies plugin provenance;
conflicting author-supplied provenance is an error. `ConfigSchema` is the graph's
typed object-field contract, not arbitrary JSON Schema: it checks required keys,
allowed additional fields and field types. The constructor validates any further
domain-specific constraints. Secret fields require host-side unresolved references.

`Capabilities::output_progress` describes whether all outputs originate their
own replay progress or relay their inputs' progress. Sources default to an origin
and transformers to a relay. This declaration is independent of semantic labels
and does not itself establish durability or validate a producer identity.

### Explicit state removal

A transformer that owns removable state can set `Capabilities::deprovision`,
return `true` from `NativeComponent::supports_deprovision()`, and implement
`NativeComponent::deprovision()`. Metadata and the constructed component must
agree. This capability currently applies only to transformers; it does not
provide a storage service or authorize deletion of borrowed or shared
host-owned resources.

Ordinary stop, replacement and shutdown preserve state. Explicit removal is
allowed only before the first start or after a successful stop and cleanup.
Once removal is accepted, the component is permanently fenced from restart and
other work, even if the request is cancelled or fails. The Host retains an
accepted operation across cancellation and retries it rather than starting a
second deletion. A failed operation can be retried on the retired instance.
The exporter waits for accepted independent control/wakeup
operations and output reads to release their work before invoking the hook.
Repeated completed removal does not invoke the hook again.

A failed or directly cancelled native operation may already have removed part
of the state. Implement the hook so it can safely retry an incomplete deletion,
and report errors rather than claiming success. Existing native components that
do not opt in retain their ordinary lifecycle and do not allocate exporter-side
removal tracking.

## Quiescent provider-driven transformer initialization

A transformer can declare `Capabilities::quiescent_bootstrap = Some(input_port)`.
Its `NativeTransformer::quiescent_bootstrap_input()` must return that same port.
The profile is explicitly non-indexed and non-transaction-participant; it does
not claim durable or coordinated bootstrap.

`bootstrap_optional` lets a quiescent transformer use `bootstrap_status()` to
declare whether its configuration requests a provider. The Host checks that
choice before activation. Without a configured provider it completes the
initialization barrier without loading records. Required-provider factories keep
their required resource dependency.

`start()` prepares private initialization state. The Host SDK then drives an
existing `ComputationBootstrapProvider` bound as `NativeBootstrapResource`:
prepare, stream snapshot envelopes, complete the snapshot and await provider
stop. Each envelope reaches `NativeTransformer::bootstrap(InputEnvelope)`.
Only after that succeeds does `bootstrap_completed(completion)` publish initialized
state. The exporter blocks ordinary data operations and output reads until
completion; initialization callbacks do not emit live result batches.

The `BOOTSTRAP_INPUT` and `BOOTSTRAP_COMPLETE` operations use the existing
component vtable and bounded envelope transport. They are admitted only for the
declared capability. Base C table layouts remain unchanged; a host that does
not recognize the capability rejects its metadata rather than ignoring it.

Provider scope must match the constructing component. This profile rejects
progress-bound providers, non-ready preparation, watermarks and durable
completion state. It supplies no `BootstrapState` persistence service and
does not invent positions from row ordinals. Writers must remain quiescent
through startup. Source-paired/coordinated providers use the separate
`durable_bootstrap` binding described below.

Provider cleanup precedes transformer cleanup on failure/stop. The Host retains
accepted provider cleanup across waiter cancellation and reports cleanup errors;
dropping an owner is not proof of completed asynchronous cleanup. The
experimental query's `drasi.experimental/provider-query` factory exercises this
contract using the unchanged native SQLite file-bootstrap library.

## Optional activation credentials

Factories may opt into `activation_credentials_v1` by implementing
`supports_activation_credentials() -> bool` and the pure
`activation_credential_context(&CreateRequest) -> Result<ActivationCredentialContext>`.
The latter returns the configured `hostname` and `port`, performs no I/O, and
receives `activation_credentials_bound=true`. It must not require literal
authentication when that marker is set. Make literal auth fields optional in
the metadata schema and enforce their conditional requirements in construction.

`CreateRequest::activation_credentials_bound` is a local, serde-skipped SDK
annotation. The separately negotiated versioned creation envelope sets it for
existing factory callbacks, including progress and coordinated-source creation.
Ordinary Rust request literals set it to `false`; the base request encoding,
base ABI 1.0.0 layouts, and wire version 1 remain unchanged.

Implement `NativeComponent::start_with_credentials(ActivationCredentials)` for
the bound path. It runs **inside the normal START operation**, with the same
ownership, busy/role gates, panic containment, progress activation, schema
generation and retained STOP cleanup as `start()`. The DTO distinguishes
`UsernamePassword` from `Token`; certificate credentials are explicitly rejected
by the Host before startup. Credential fields are bounded to 64 KiB and Debug
redacts both usernames and secrets. Bound startup failures are sanitized at
the ABI boundary.

Keep acquired credentials transient. Do not modify `configuration()`, captured
configuration, source identity or the constructor recipe, and do not fall back
to literals after a provider failure. Unbound instances still use the unchanged
literal `start()` path. Authentication is not a processing RPC. The SDK cannot
make a connector's detached startup tasks safe: connector startup must remain
cancellation-safe under the existing operation-ownership contract.

## Configuration descriptions

The optional `configuration-v1` entry point publishes field descriptions, defaults,
integer limits and allowed values without changing the frozen ABI 1.0 metadata or
wire version 1. Older hosts ignore it; newer hosts continue loading older plugins
without inventing missing descriptions. A present but malformed extension is an
error, including during metadata-only discovery.

Use one `ConfigDefinition` to supply both `metadata().configuration` through
`definition.schema()` and `Factory::configuration_details()` through
`Some(definition.details())`. Build it with `ConfigDefinition::try_new` and
`ConfigOption`; duplicate fields, incompatible limits and invalid defaults reject.
Reuse the typed configuration's default functions and limit constants instead of
maintaining another set of values. Descriptions must never publish literal secret
defaults.

The SDK and host validate advertised limits and allowed values before constructing
a component. Constructors still apply defaults and check relationships between
fields, paths and resources. The descriptions do not replace those checks or imply
that arbitrary JSON Schema validation is implemented.

`NativeFactory::configuration_schema()` produces the shared JSON Schema used by
Server's kind-schema API, version-qualified `configurationSchemas` metadata and
OpenAPI definitions. A factory without the extension retains its original
field/type schema and reports that detailed descriptions are unavailable.
The independent network components describe their actual supported choices:
gRPC source `adaptiveEnabled` allows only `false`, while gRPC sink `batchSize`
is bounded to `1..=65536`. Optional capabilities are negotiated separately;
configuration descriptions alone do not enable them.

## Read-only source data schemas

`NativeSource::source_schema_reader()` may return an independent
`SourceSchemaReader` over the source's actual catalog or bounded worker. Obtaining
the reader is configuration-only; it must not perform I/O or acquire the mutable
source processing lock. Reads must remain possible while `next()` is pending.
This is data-schema discovery, not the factory's configuration schema.

The optional `source_schema_v1` extension carries a bounded MessagePack
`Option<SourceSchema>` without changing ABI 1.0.0 or envelope wire version 1.
The SDK permits one schema read per source at a time, limits replies to 1 MiB
and reads to 30 seconds, and rejects results from stopped or replaced
activations. Cancellation releases the read slot. Actual failures remain errors;
older libraries without the extension simply have no native schema reader.
This interface exposes no SQL execution, database query or transaction methods.

## Optional per-output snapshot, replay, follow and query readers

`NativeComponent::output_readers()` returns the component's `OutputReaders`: a
`SnapshotProvider`, `ReplayProvider`, `SnapshotFollowProvider` and/or
`QueryViewProvider` for each
declared **output** port, registered independently (neither implies another, and
none implies persistence). The default registers nothing. Obtaining them is
configuration-only, performs no I/O and takes no processing lock; every provider
reads its own state, so a read is independent of a pending `next()`/`transform()`.

Capability is exactly the registration. Factory metadata lists the ports in
`Capabilities::output_snapshots`, `output_replays`, `output_follows` and
`output_queries`; at
construction the SDK rejects any mismatch with the actual registrations, an
undeclared, duplicate or non-output port, and a registered provider the metadata
does not list. Flags alone cannot create a provider or a no-op substitute. A library
needs the separately negotiated `output_reads_v2` extension for snapshots/replay
and query reads, and `output_follow_v2` for follows; the Host rejects a declaration
without it.

Query readers provide immutable `QueryDescription` metadata at construction and
coherent query snapshot/head/replay reads with their output generation. A query
view captures the actual producer cursor, canonical rows and retained history in
one observation; it is not assembled from independent snapshot and replay calls. The
description uses the shared execution settings and semantic configuration hash.
The Host can attach ordinary library query read APIs to this capability without
running an evaluator or keeping a second result projection. It is descriptive
query semantics over an output, not a Query execution role. Base C table layouts
are unchanged; old hosts reject an unknown `output_queries` declaration.
Query-read history uses logical query sequences, including its normalized
transport sequence, and retains matching query scope, configuration and reset
generation. The Host validates those fields as well as the actual producer epoch.

`query_observations` additionally negotiates metadata-only canonical row/outbox
counts for a single query output on a Transformer. A cold observation initializes
the Host's existing gauges; successful query-output replies include updated
metadata. No-op and replay processing cannot fabricate logical sequence advances.
This capability is validated against the actual query provider, not inferred from
its descriptive Query label.

`query_recovery_views` requires those observations and an actual recovery-view
implementation. It adds recovery observation/view operations using the same
bounded query-view pages. `QueryViewProvider::recovery_reader()` supplies the
read-only `QueryRecoveryReader` contract to recovery consumers. Recovery heads
contain no rows; full views capture canonical rows and history together.

An application may separately bind a native query's read API to the existing
graph-owned result catalogue and connect a real query-results outlet. Ordinary
channel/broadcast subscriptions then use that existing delivery path, not native
follow leases. This adds neither an evaluator nor mutable result state to the Host.

Records and envelopes use the existing codecs: snapshot records keep their
schema-encoded payload, replayed envelopes are binary computation envelopes with
their logical `GraphProducerProgress` (transport sequences are separate), and a
cursor is the producer epoch plus committed logical sequence. Typed provider
results (`NotReady`, `Gap`, `EpochChanged`, `Retired`, `Absent`, `Invalid`,
`LeaseExpired`) cross as data; they are never converted to an empty snapshot or
replay. The Host opens an owned transfer, captures one provider observation, and
pulls bounded pages until the advertised record/envelope counts are complete.
Snapshot rows, replay and combined query views have no transport-wide count or
byte cap. Pages target 1 MiB, contain at most 1,024 items and must fit 16 MiB;
an individually oversized record/envelope is an explicit `OutputReadLimit`,
never truncation. Single-message requests remain bounded to 16,384 snapshot
records or 1,024 replay envelopes, but ordinary Host reads use transfers.
At most 16 in-flight reads, 16 transfers and 16 follow leases are active per
component; excess requests report BUSY/`NotReady`.

The plugin holds the shared immutable observation, not a fresh snapshot for each
page. The Host validates metadata, schema, cursor, page order, duplicate record
identities and final counts while assembling the caller's response. Query views
must retain one common snapshot/history head. The follow extension pages its
initial snapshot before admitting live reads, using the same captured
snapshot/tail cut and page implementation.

Ordinary reads are served only while the component is started (`NotReady` otherwise).
Negotiated recovery reads may also use positively retained query state after
plugin STOP and Host cleanup both succeed. A failed publication, incomplete
initialization, cancelled cleanup or retired owner does not become an empty
successful view. Activation/deprovisioning fences that retained access again.
Stop, restart, replacement and release fence every read and **close every follow
lease synchronously**, and release all captured transfer state, even if the Host still holds
the handle; an in-flight read or lease never serves a later activation or a successor.
Dropping a read future releases its transfer and cancels provider work, including
an unpolled opening operation. A follow `next()` is a separate
cancellable operation per call that carries the last item the holder observed: the
producer retains one delivered-but-unacknowledged item per lease and resends it,
so cancellation racing completion cannot lose an item. Lag (`Gap`) and expiry
(`LeaseExpired`) are distinct and sticky.

Streaming a captured observation does not implement a generic live-follow
coordinator or crash-durable handover; snapshot plus follow
detects epoch change, lag and expiry within the lease's bounds only.

## Read-only output snapshots

A sink or transformer factory may opt in to the `output-snapshots-v1` extension
through `supports_output_snapshots()` and `create_with_output_snapshots()`. The host
supplies an optional `NativeOutputSnapshots` capability limited to an explicit
allow-list of output endpoints (component and port) of sources, transformers or
queries. The plugin cannot register outputs, obtain graph ownership or mutate
state through this capability.

`snapshot(&OutputEndpoint, &Schema)` returns a `NativeOutputSnapshot`: the
output's `OutputCursor` (producer epoch and logical sequence), its dataset and a
bounded-operation stream of validated full-image `Record`s in the caller's
schema. The host captures the snapshot through the graph's output catalogue; each
subsequent read returns one record, so the transport's per-call byte bound
applies and no whole-table buffer crosses the ABI. A typed `OutputReadError`
(`Absent` capability, `UnknownOutput`, `NotReady`, `Retired`) is returned as the
error rather than an empty snapshot. Admission limits and readiness timeouts are
host-owned. Dropping the stream releases its reader, and component stop revokes
pending reads. Errors remain errors, including after streaming has begun.

The native SSE sink and experimental volatile query transformer use this
interface. The Host resource is a
`NativeOutputSnapshotsResource`, an immutable allow-list and limits policy bound
as the factory's `output_snapshots` dependency. The native factory combines it
with the graph's actual output catalogue when it creates the binding. Snapshot
callbacks enter the owning host runtime while being polled; no Tokio handle,
catalogue or graph control crosses the ABI. This read service is distinct from
incoming recovery snapshot envelopes and does not provide an atomic
snapshot/live subscription.

The experimental query initializes its configured inputs from captured snapshots,
then checks each input's live producer progress against its own cursor. Its volatile factory also
exports completed result snapshots, allowing another transformer to initialize
from its output. This is not the full bootstrap-consumer protocol, and the read
service does not itself retain the live suffix or order component activation.

## Declared input bindings

A sink or transformer that needs to know where its data comes from sets
`Capabilities::binds_inputs`. The host then fills `CreateRequest::inputs` with
one `InputBinding { input_port, output: OutputEndpoint { component, port },
stream }` per validated, declared incoming edge, ordered by input port and
producer. The host holds creation until every incoming output has a declared
stream, and reconstructs the component with new bindings when an incoming edge
or stream changes, so a component never serves stale ones. A component that
does not opt in receives no inputs, and supplying inputs to one is an error. A
component must never infer its producer from payloads or stream spelling; a
sink needing several outputs per producer names them by alias (see the
`SinkOutputs::from_inputs` helper in `drasi-computation-contracts`). Programmatic
construction outside a graph passes the bindings to
`NativeFactory::create_component_with_inputs`.

Alternatively, a sink or transformer can set
`Capabilities::input_binding_updates` and implement
`NativeComponent::supports_input_binding_updates` and `bind_inputs`. The graph
calls the hook before start and before processing with the current validated
bindings, including after a live rebind. It can repeat an unchanged set; the
implementation must tolerate repetition and cancellation. The hook runs
serially with the component's mutable operations, and a failure prevents that
operation from proceeding. This capability does not itself request bindings
during factory construction or force reconstruction. A factory that opts into
both capabilities still receives construction-time bindings and is
reconstructed when those bindings change. Transaction-sequence steps do not
have graph input bindings and cannot use this update capability.

## Read-only consumer progress

Source and transformer factories may opt into the independent
`consumer_progress_v1` extension with `supports_consumer_progress()` and
`create_with_consumer_progress(request, control, NativeConsumerProgress)`.
Opting in makes the capability required: ordinary unbound creation fails, even
when an older host does not discover the extension. Existing single-owner
factories keep their recovery-v1 behavior; advertising both contracts is invalid.

Keep the supplied `NativeConsumerProgress::reader()` and return that exact view
from `NativeComponent::consumer_progress_reader()`. A newly wrapped view with
matching graph/producer names is not the same binding. Constructors retain the
capability without reading mutable progress; snapshot/subscription operations
become available during `start`. `NativeConsumerProgress::consumer_ids()` exposes
the validated immutable membership for constructor checks without a progress read.
Each snapshot includes every fixed consumer's
independent `SourceProgressSnapshot`, including consumers without checkpoints or
with failures. The SDK validates membership and retains the existing opaque
checkpoint encoding. `wait_ready()` / `wait_recovered()` require all members and
surface failures instead of skipping the slowest or failing consumer.

`NativeComponent::consumer_replay_retention()` optionally declares the actual
upstream retention. It does not infer durability from the consumer set, promise
external effects, or replace the Host's actual owner-resource proof. Both the
reader and retention declaration must remain unchanged during activation.

When replayed events require source-defined ordering, implement
`NativeComponent::input_position_policy() -> Option<Arc<InputPositionPolicy>>`.
Construct one immutable policy and return clones of that same Arc. The SDK
captures it separately from mutable processing state and exports an opaque,
producer-retained comparator. The declared `SourceProgressKey` identifies the
consumer checkpoint to compare; `is_after(event, handled)` interprets opaque
positions, including partitioned positions, without I/O. Each position is bounded
to `MAX_SOURCE_POSITION_BYTES` (65,536 bytes). Errors remain errors, and comparator
panics quarantine the native instance and wake pending operations. A policy
without consumer progress is rejected; absence is valid when logical replay
identity is already stable. The Host graph owns policy installation/revocation.

The complete snapshot is bounded to 256 members, 4,096 checkpoint/transport entries
and 1 MiB, with 16 retained subscriptions per binding and one wait per subscription.
Limits produce errors, never partial snapshots. Cancelling a wait loses no update;
only a successful snapshot marks its exact observations seen. Stop/drop fences
pending reads and waits. After successful stop/start, acquire new subscriptions:
old subscriptions remain closed. Failed/cancelled start or stop cannot be followed
by a successful restart without completing stop. The transport polls bounded
watch operations directly, without a forwarding worker or second graph.

## Scheduling and ownership

Bootstrap preparation may return `RefreshVolatile` or `ResetRequired`. Until a
snapshot has started, the query may reset its state and call `prepare` again on
that same provider. A `Ready` result seals preparation until successful stop;
snapshot startup remains single-use. Failed or cancelled preparation, snapshot
work or cleanup still requires successful stop before reuse.

The host owns start, data processing, stop and cancellation. Each operation is a
producer-owned opaque handle with cooperative, nonblocking poll, retained wake
callbacks, cancellation and release. Mutable data/lifecycle calls cannot overlap.
Cancellation drops the future immediately; it does **not** promise rollback of
already performed state changes or external effects. Failed, panicked, cancelled
and invalid operations return structured failures, not empty successful output.

The Host retains the original native `Failure` as the cause of a classified
`ComponentOperationError`. Only `FAILED` with its retryable flag is retryable;
`PANICKED`, `PROTOCOL`, `CANCELLED`, `CLOSED` and unknown status codes remain
terminal even if that flag is set. The consumer extension has its own explicit
retry status and bounded `DeliveryRunner` attempt/delay policy; exhausting it
does not authorize an implicit graph restart.

The SDK retains a per-instance panic quarantine across data, control and
lifecycle operations. A detected panic during polling, operation destruction or
configuration capture wakes pending operations and prevents further normal work.
Stop remains available for cleanup, but does not make the damaged instance
restartable; reconstruction creates a fresh instance. The Host preserves
`PANICKED` as a library-owned `ComponentPanic` cause so graph supervision can
distinguish damage from an ordinary terminal error. Task cancellation alone is
not classified as a panic.

Returned terminal errors retain the graph node and its diagnostics. Unrelated
branches and host management continue. Rejected custom control messages do not
close the inbox. Graceful graph stop waits for an admitted data operation,
continuations and delivery confirmation before stop; a boundary timeout reports
failure without abandoning that work. Explicit graph cancellation remains force
cancellation with the rollback limitations above. These policies do not add a
process-isolation or comprehensive host panic-containment guarantee.

The SDK has one explicitly owned, process-lifetime Tokio I/O runtime per binary.
It enters that runtime while the host polls plugin futures so timer/socket drivers
work without borrowing the host's Tokio internals. It never spawns graph execution
or processing futures. If a component starts auxiliary I/O workers, it must own
their cancellation/join lifetime and implement stop/quiesce accordingly. Transaction
participants must not start workers. Host and plugin task-locals never cross the ABI.

All buffer/handle frees run in their producer. Rust `Future`, trait-object, `Arc`,
`Bytes`, allocator and runtime representations never cross the C boundary.
`OperationFuture` returns a local `ReceivedBytes` owner: it borrows the producer's
immutable allocation for decoding and calls its release function exactly once on
drop, including decode failure and unwinding. The allocation remains valid even
after the completed operation is released and can be released on another thread.
Decoded envelopes own their data; borrowed wire views never escape that owner.
Input admitted through `begin` is still copied because its one-call
`BorrowedBytes` lifetime cannot cover deferred asynchronous processing.
Wake/control/state callback contexts use producer-side retain/release functions.
Retained callbacks remain valid after operation cancellation or component release,
and revoked capabilities reject further calls. Exported functions and future polls
contain unwinding panics; as with all native code, `panic=abort` terminates the process.

Libraries and their I/O runtime are **pinned for process lifetime**. There is no
hot-unload promise. Native plugins are trusted in-process code, not a sandbox.

## Envelopes and schemas

Metadata and configuration are UTF-8 JSON. Native wire version 1 uses named
MessagePack records and bulk MessagePack binary buffers, not JSON envelopes or
per-byte integer arrays. Its binary envelope format is also 1.
`BinaryEnvelopeCodec` encodes the full schema descriptor, typed images, sequence,
stream, source position, optional event-local control, context identity,
annotations and lineage. It is separate from the unchanged persisted JSON `EnvelopeCodec`;
there is no storage migration. Transaction-transform inputs/outputs are directly
encoded binary envelopes. Transaction derivation and record-validation RPCs also
use bulk binary buffers.

Encoding borrows immutable record/context data instead of cloning payloads into
temporary storage frames. Decoding borrows strings and opaque byte fields from the
received frame, then constructs validated, fully owned computation objects.
Exact descriptor equality, not fingerprints alone, selects a schema. Host-side
decoding still calls the producer's executable record validator through the C
table; no permissive opaque-record validator or local replacement is substituted.

Every port's schema must be supplied to `PluginDefinition::new`, including standard
graph-change/query-row schemas when used. No data is delivered to undeclared ports
or decoded under mismatched schemas. Metadata is limited to 1 MiB and operation
messages/envelopes to 64 MiB; exceeding limits is an error, including during
encoding rather than after allocating an oversized message. Framing rejects
truncation, trailing values, impossible declared lengths and excessive nesting
before deserialization can reserve collections from untrusted lengths.

The binary envelope is a named map with `format: 1`, `id`, `change_set`, `schema`,
`operations`, `system`, `lineage`, `context_identity` and `annotations`.
Identities contain `namespace` and binary `value`; schemas contain `id`, `version`,
`encoding` and binary `definition`. Operations retain the `Add`, `Update` and
`Delete` variants and their ordinals. Record image kinds are `0` full, `1` patch,
and `2` partial. System and lineage metadata contain `stream`, `sequence`,
`timestamp`, `source_position` and optional event-local `control`, whose fields are
`kind` and binary `payload`. Timestamps retain their RFC3339 representation and full precision.
Lineage and annotations are transmitted newest first; decoding restores their
original semantics, including duplicate annotation keys. Unknown fields, tags,
versions, schemas and invalid record/change-set contracts fail explicitly.

## Optional per-port format selection

`FactoryMetadata::ports` are a factory's default descriptors. A factory that
genuinely implements more schemas on a port lists them in
`Capabilities::port_formats` (`PortFormatAlternative { port, schema }`); every
alternative must also be in `PluginDefinition::new`'s schemas with its executable
validator, and the factory must declare the optional object field
`ConfigSchema::with_port_formats()` (`ports`). Empty lists are omitted from the
manifest. Configuration then selects at most one format per existing port:

```json
{ "ports": { "out": { "format": "vendor.columnar-batch-v1" } } }
```

Names are `<schema id>-v<version>`. The default format may be named explicitly.
Selection is pure and uses no I/O: unknown ports or formats and malformed `ports`
values are errors, never defaults, and no codec or converter is created. Other
ports keep their descriptors. Use `FactoryMetadata::for_configuration` (or
`descriptor_for`) in `create` so the component's descriptor matches the
selection; the SDK rejects a component whose descriptor differs from the
resolved one. A factory opts in by declaring alternatives or by declaring `ports`
as an optional object field (`ConfigSchema::with_port_formats()`); once opted in,
any invalid selection is an error, and with no alternatives only each port's own
format is accepted. A factory that does neither keeps `ports` as an ordinary
plugin field. Changing a persisted port's format is a stopped reconfiguration;
journals are not reinterpreted.

`Capabilities::primary_ports` (`PortPrimaries`) optionally designates one primary
input and one primary output by port id for shorthand endpoints.
`FactoryMetadata::validate` requires each to name an existing port of that
direction. `FactoryMetadata::resolve_endpoint_port(direction, explicit)` resolves
an explicit port, else the primary, else a direction's only port; several ports
without a primary are an error, never a guess. Format selection does not change
port ids, so a primary is preserved.

## Control

Use the supplied `ControlSender` for ready/not-ready and neighbor notifications.
For inbound messages, opt into `capabilities.control` and supply a
`NativeControlHandler`. Its calls are polled independently of mutable data calls;
control never waits for a data pipe's capacity. Host attachment/generation and
neighbor checks remain authoritative. Queue-full, stale and closed sends fail.

An in-process graph `ControlHandler` or `ComponentControl` is not transportable:
the SDK explicitly rejects those handlers instead of sending their Rust objects
across FFI. Asynchronous readiness requires both `capabilities.readiness` and the
native control interface. The component must report the same readiness requirement.

## Optional transaction participants

`TransactionalComponent: Transformer` is explicit opt-in, not a capability flag
that upgrades an arbitrary transformer. Return `Component::Transactional`, declare
exactly one input/output, and set `capabilities.transactional`. Implement:

```rust,ignore
async fn transform_in_transaction(
    &self,
    input: ChangeEnvelope,
    context: &NativeTransactionContext<'_>,
) -> anyhow::Result<ChangeEnvelope>;
```

The non-cloneable context exposes `step_id()` and asynchronous `get`, `put`,
`remove`, `get_element`, `put_element`, `remove_element` and `derive`. It contains
no commit method. All mutable business state belongs in this context, not on
`self`; external effects, workers, independent control, wakeups and continuations
are forbidden. The existing host `TransactionTransformer` owns the transaction,
isolation, recovery, outbox and commit.

The host keeps only a revocable RPC mailbox in the retained C context, never an
erased or `'static`-extended Rust `TransactionContext`. It polls storage requests
inside the original step borrow. Cancellation/completion closes the mailbox, and
late requests fail without accessing transaction storage. Ignored failed/cancelled
or outstanding requests prevent a participant from reporting successful completion.

The same participant also works when the host container uses `SharedStorageGroup`
and shared QoS outputs. Its state, the container's input progress/retained output
and the selected journal appends then commit together. The host reserves pipe
capacity before invoking the participant. No new ABI interface or storage handle
crosses the boundary: existing ABI 1.0 transaction participants use their existing
revocable mailbox. This does not give standalone native transformers output
tracking, make separate stores atomic, or deduplicate external effects.

## Optional host-owned indexed processing

An ordinary transformer may declare `capabilities.indexed_processing` and
implement `Factory::create_with_processing` to receive `NativeProcessing`. The
graph must bind a host `NativeProcessingResource::new(provider)` as its
`processing` dependency.
For committed-progress components, the factory prepares the actual provider
before the graph captures its recovery contract, without activating native
processing. Direct constructors may call `prepare_processing()` explicitly.
Other indexed components open their provider at start. The
plugin calls `NativeProcessing::run` with async work. Its
`NativeProcessingContext` currently supports element get/set/delete and slot
lookup, join configuration, archive point reads, element/archive clearing, and
future-queue operations. Each context is revoked at the end of that operation.
Join configuration is not transactional: a failed operation that attempted it
fences the owner and requires reconstruction.

The context also exposes atomic state information, committed checkpoint and
result-sequence reads, staged checkpoint/result-sequence writes, bounded outbox
reads/appends, and live-result page reads/mutations. These operations reject a
bundle whose checkpoint, outbox, live-results writer and indexes do not
participate in the owner's actual atomic
result domain. Reported checkpoint persistence and storage durability come from
that bundle; a backend name is not evidence. Outbox, live-result and
result-sequence methods are bound to the host component, not a plugin-supplied
query ID. Page requests
are limited to 1,024 records and the native frame byte budget.

Result-sequence and live-row page reads observe committed state. Do not infer
general read-your-writes behavior from staged metadata; the typed progress
services maintain their own operation-local drafts. Live-row pages use an optional exclusive
signature cursor, so the first page includes signature zero; signatures must
increase strictly. Unsupported backend paging fails explicitly. Hold the
existing processing owner across a multi-page read to exclude writers.

Staged metadata, outbox and live-row changes commit or roll back with index
mutations. The plugin cannot commit independently. The owner's bounded
`BootstrapState` service also persists provider intent.

An indexed transformer may declare `committed_input_progress` with one declared
input port. It then uses `read_input_checkpoint`, `stage_input_checkpoint` and
`recover_input_progress` in its existing processing operations. The Host owns
the reserved checkpoint encoding and derives recovery from the actual committed
store. Generic checkpoint writes cannot bypass these typed progress operations.
The host serializes commit/publication for this opt-in path and publishes no
staged cursor. The existing progress limits apply: 1 MiB, 4,096 combined
checkpoint/transport entries, and the source-position byte bound.

`NativeComponentProxy::committed_input_progress()` exposes the standard
`SourceProgressView` without I/O or a forwarding task. Recovery is visible with
`recovered: true` and `ready: false`; successful component startup separately
enables readiness/admission. Failed or cancelled work fences the view without
advancing its last committed checkpoints. Stop revokes old views and subscriptions;
fresh readers for a later activation cannot be substituted behind old handles.
This profile publishes stream or source progress, including real sequence zero.
`stage_progress_checkpoint` preserves logical positions independently of
`stage_input_transport`; `stage_bootstrap_watermark` does not fabricate a consumed
transport position.

The view is data-only. Host construction can separately obtain the processing
owner's exact `QuerySourceProgress` resource through `processing_progress_owner()`
for existing source/coordinator bindings. One canonical publication backs both
interfaces and retains no storage owner. `stage_bootstrap_complete` persists the
initialization marker and publishes it only after the actual commit.
The graph-owned volatile `input_progress` binding
remains separate and is not offered by a factory declaring committed progress.
Factories without this opt-in acquire no progress publisher or extra gate.

Query processing uses `prepare_input_progress` and
`stage_prepared_input_progress` to reuse the existing graph source/producer
validation, duplicate rules, opaque-position policy and transport bookkeeping.
Successful operations must stage their prepared input. The actual graph-provided
progress owner is bound exclusively, not replaced by a same-named resource.

Factories can explicitly declare `volatile_processing` for actual memory
providers. That profile retains indexes and evaluator state across a healthy
quiescence, publishes successful input progress and provides
`reset_volatile_state` without an atomic/restart claim. Persistent atomic-state
requests remain unavailable. `state_info` reports the actual profile;
`physical_time` uses the bound query clock, and `next_emission_sequence` reserves
the actual catalogue transport counter so replacement cannot reuse a sequence.

`durable_bootstrap = Some(input_port)` requires that input's indexed processing
and committed progress. The component declares the same
`durable_bootstrap_input`, reports whether bootstrap is configured/completed,
consumes streamed initialization through `bootstrap`, and atomically stages
watermarks/completion state in `bootstrap_completed`. The Host binds an existing
`NativeBootstrapResource`, passes its actual owner's state service to the provider,
awaits cleanup, and keeps normal processing and reads behind the completion
barrier. Progress-bound providers must retain the exact local owner; external
views or matching names are not accepted as ownership.

`durable_output` uses typed `read_delivery_progress` and
`stage_delivery_progress` operations. The Host owns destination encoding,
fingerprints, actual group handles and the preflight publication used by graph
reconciliation/retirement. Generic checkpoint writes cannot replace these
records. The component implements `resume_delivery` so an unchanged binding
reschedules any pending output after quiescence.

Use `NativeProcessing::run_output` for work that may append a retained output;
ordinary `run` is for metadata, bootstrap and replay-counter operations.
The opt-in `OutputProcessingWorkV1` descriptor has its own exact header; the
base C tables remain unchanged and unsupported hosts reject the descriptor.
For shared destinations, the Host reserves capacity before beginning the actual
transaction and enlists the validated retained envelope using the existing
`TransactionGroupMutation` mechanism. Progress/binding publication is also a
group mutation and runs only after commit, before the group gate is released.
Cancellation/failure retains the owner's existing fencing/cleanup rules.

Factories that also declare `non_atomic_publication` can accept an explicitly
selected persistent non-atomic profile. Direct constructors use
`create_component_with_processing_publication`; graph resources use
`NativeProcessingResource::with_publication`. This is not an implicit fallback
when atomic qualification fails, and it cannot join a shared transaction group.

For phased publication, `run_deferred_progress` commits evaluation/input state
without exposing its progress. The next output operation must call
`resume_input_progress` and finish successfully before that metadata is published.
Only one bounded, generation-fenced metadata draft is retained; there are no
rows, indexes or transaction handles in it. Further input/work cannot bypass the
pending completion. `validate_input_progress` reuses the normal admission rules
without staging input, so invalid input can be rejected before committing a
publication marker.

Non-atomic reset uses separately committed intent, staged checkpoint clearing
and explicit completion. `begin_non_atomic_reset`, `reset_non_atomic_state` and
`finish_non_atomic_reset` preserve intent outside cleared query state. A
bootstrap reset cannot complete before its completion marker; deprovision
requires a cleared result head. The existing query plugin's protocol determines
when these operations are authorized. The services never grant atomicity to
independently committed output.

`reset_state` requires the actual atomic bundle and staged checkpoint clearing.
It preserves provider intent for bootstrap retry, not explicit deprovision.
It cannot discard pending delivery during reset or clear bound durable
destinations. `reset_generation` distinguishes a confirmed cleared incarnation
from unidentified state; no success-shaped fallback adopts a partial store.

`initialize_reset_output(sequence)` preserves a logical output baseline after an
authorized bootstrap reset. The Host grants it once, only in that reset operation,
before readiness and with no bound destinations. It stages the baseline and
delivery position together; it cannot acknowledge live output, skip pending
delivery, survive into another operation, or follow a deprovision reset. Ordinary
sequence advancement and delivery validation remain unchanged.

An ordinary transformer with `continuations` may additionally declare
`startup_continuation`. After successful startup the host schedules one
`continue_transform` call before admitting input, allowing recovered output to
be replayed without waiting for another source event. Return an empty batch with
no pending work when there is nothing to replay. Ordinary transformers that do
not opt in acquire no extra startup work.

Index failures retain their `IndexError` category across the native boundary;
provider-specific error objects are represented by their display text. A failed
or abandoned state request cannot be ignored to commit the operation. Stopping
the binding cancels and joins its accepted operations, including handles that
the plugin never polls.

The host owns serialization, commit, rollback and cleanup. An interrupted or
uncertain commit fences the owner, and non-atomic or volatile providers retain
their existing guarantees rather than gaining atomicity. A failed operation can
therefore leave partial state on a non-atomic provider. Calls are bounded, and
late requests fail explicitly. The indexed query plugin uses these services for
memory and persistent atomic operation, including bootstrap, archive reads,
input-progress validation, Core future evaluation and delivery. Its ordinary
factory adapter binds the existing query clock and catalogue transport counter.
Its explicit non-atomic profile uses the same Core evaluator and the existing
shared publication/reset protocol.

## Optional host-owned consumer completion

The independently negotiated `drasi_computation_plugin_consumer_v1` extension
leaves base ABI 1.0 and wire 1 unchanged. Declare `Factory::consumer_mode()` as
`External` or `Transactional`, with a handled, non-snapshot sink descriptor.
Return `Component::Consumer` for a `NativeComponent + DeliveryHandler`, or
`Component::TransactionalConsumer` for a `NativeTransactionalConsumer`. Ordinary
`Component::Sink` factories retain their existing behavior and acquire no ledger.
Consumer factories reject construction through the ordinary, unbound create path.

The Host owns the existing `DeliveryRunner`, its bounded progress records, retries,
transaction and cleanup. `NativeFactory::create_consumer` accepts the actual
`ComputationIndexProvider`, `DeliveryOptions` and stable graph/component scope.
Graph factory construction instead requires exactly one `consumer` dependency:
an `IndexBackend` resource holding `NativeConsumerResource { provider, options }`.
The resource is managed through the graph's existing dependency lifecycle; no
provider handle crosses the ABI. Requested failure survival is checked against
the constructed indexes, not a backend name. Server recipes are a separate layer.

Each batch transfers its complete envelope once. The SDK independently derives
and compares its content/consumer identity before any effects, then retains one
bounded batch. Subsequent calls contain only generation, operation index and
stable operation key (at most 4096 bytes). Keys exclude retry transport identity
and replacement configuration. A stale generation cannot handle or release a
replacement batch; an active call prevents batch retirement. Ending a batch is
memory cleanup, never an acknowledgement.

External handlers return success only after actual destination completion.
Their `DeliveryItem::id` supports destination-side idempotence; a successful
external effect can repeat after a crash before local progress commits. Progress
alone is not external exactly-once delivery. Implement `retryable` explicitly for
known transient errors. Queued/accepted responses are not handled completion.

Transactional handlers receive the non-cloneable `NativeTransactionContext`.
State and each operation's completion commit in the same actual host transaction.
All commit-sensitive state must live in that context: no independent writers,
external effects, background workers, nested transactions, or commit-sensitive
fields on the handler. Independent control/readiness interfaces are rejected.
There is no commit capability. Ignored failed, cancelled
or outstanding state requests reject completion. This is per-operation atomicity,
not whole-batch or whole-source-transaction visibility. Persisted handling mode
cannot be changed while reusing the same progress.

The Host retains the upstream envelope until all operations complete. Partial
completion resumes at the first unfinished operation after cleanup/reconstruction.
Cancelled/failed delivery requires stop before reuse; cancelled stop retains the
storage owner for a subsequent stop. A panicked consumer remains damaged after
cleanup and must be reconstructed. Activation and restart recheck negotiated
mode and actual storage guarantees. None of these services adds a graph executor.

## Optional graph-owned source admission

The export macro also supplies the separately versioned service-v1 symbol.
Base ABI 1.0 metadata and tables are unchanged; old binaries without the symbol
remain loadable for fast mode. Declare `Factory::supports_source_admission()`
only for a single-output source, and implement `create_with_admission`.
Without a bound service, retain the existing `create` behavior.

`NativeAdmission` is a retained C capability, not a Rust graph handle. Its
immutable `identity()` allows configuration-only validation of the bound scope,
component and output stream. Its async methods register/retire producers, read
producer status/receipts and admit input. A producer session plus consecutive
sequence identifies a retry. Identical retained input returns its original
receipt; conflicting or expired retries reject. Success follows graph validation
and the outgoing QoS transaction, not receipt into a plugin queue or completion
of a sink's external effects.

The host factory's optional `admission` resource dependency selects the actual
enabled `QosChannel`; all graph outputs must use that same channel. No second
source journal or volatile publication queue is allowed in this mode. The host
does not call `next`, and the SDK rejects an alternate `NEXT` publication.
Listener failures must therefore call `report_failure`, including unexpected
successful listener exit. It is synchronous, bounded, and independent of admission
capacity; a failure during startup is retained until graph processing begins.
Workers still require owned cancellation and joining before restart.

`AdmissionErrorKind` distinguishes busy/full, stale sessions, invalid sequences,
expired/conflicting retries, pending obligations and uncertain writes.
`AcceptanceUnknown` and `MetadataUnknown` mean the operation might have committed;
resolve/retry the same identity instead of advancing or allocating a new one.
Lost or malformed responses after submission remain uncertain. Cloned handles
share one retained host owner; replacement revokes callbacks even if a plugin
keeps a clone. Neither this interface nor successful loading grants once-only
external effects.

## Optional read-only source progress

The independent `drasi_computation_plugin_recovery_v1` symbol negotiates
`Factory::supports_source_progress()` and `create_with_progress`. It does not
extend service-v1, strict base metadata, ABI **1.0.0**, or wire version **1**.
Old hosts ignore the symbol; new hosts still load old binaries without granting
this service. Missing, malformed or unsupported requested services fail explicitly.

The host factory's optional `source_progress` dependency accepts an actual
`QuerySourceProgressResource`. It checks graph scope before foreign construction,
retains the real owner locally, and gives the plugin only a `NativeSourceProgress`
C capability. Call `reader()` to pass the same external reader to a native source.
The SDK requires the source's `recovery_reader()` to return that actual reader,
not a same-named substitute or a locally fabricated owner. A declared replay
retention boundary must name its consumer. A reader without guaranteed retention
is allowed, but does not establish replay guarantees.

Only the host proxy supplies local owner evidence to the graph. Recovery assertions
still compare the actual transaction owner's resource by pointer identity.
Retention declarations are captured at construction and checked on activation;
they are not recomputed or sent for each envelope. Admission and a separate replay
progress binding cannot be combined on one source.

Snapshots preserve readiness, admitting/recovered/bootstrap flags, persistence,
reset generation, source and stream checkpoint identities, unsigned sequences,
binary source positions, transport positions and failures. Each frame is bounded
to **1 MiB** and **4,096 combined checkpoint/transport entries**. Subscriptions
are independent and coalescing, with at most **16 per binding** and one active
wait per subscription. Successful reads mark only the returned version observed;
cancelled waits and failed serialization do not consume progress. Reads are
memory-only; waits use caller-polled watch operations, without a forwarding task,
host runtime, database access or writable progress mirror.

Proxy destruction, replacement and failed construction revoke retained capabilities.
Pending waits wake and stale reads fail explicitly. Plugins must still stop and
join their workers before replacement. No write, acknowledge, reset or commit
operation is exposed. Unbound fast sources allocate no progress capability or
subscription, and data envelopes remain unchanged.

The Host SDK's `native_recovery` **test-only cdylib** exercises the real SQLite
source against a host-owned RocksDB query, including uncommitted replay,
whole-transaction results, committed journal retirement, reconstruction and stale
callback rejection. Build it before Host SDK unit tests:

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo build -p drasi-host-sdk --example native_recovery
```

This proves the read-only service, not production database plugin registration.
The same test-only library also supplies the synthetic bootstrap fixtures below.

## Internal source/bootstrap coordination

The optional `bootstrap_coordination_v1` extension pairs a source with query-owned
bootstrap providers when initialization must share the source's actual engine
and leases. It is not a general plugin service or application SQL API.

The source factory constructs one `NativeSourceBootstrapCoordinator` without I/O.
Its `SourceBootstrapProgress` contains either the actual single owner or the
fixed consumer group and all actual member readers. These bootstrap readers are
active under the coordinator resource lifecycle, even before the live source is
ready. `create_source` receives the separately negotiated
`SourceBootstrapLiveProgress`; the returned source must use that exact live view,
not substitute the bootstrap view. Both refer to the same underlying owners,
which the Host verifies by identity.

Source and paired bootstrap resources depend on this graph-owned coordinator.
The source library alone owns its snapshot/live worker coordination. A paired
bootstrap factory validates `NativeSourceBootstrap::metadata()` and calls
`open()` to transfer the original source-allocated bootstrap handle. Existing
preparation, bounded snapshot polling, completion-state and awaited-stop
operations remain unchanged; no extra snapshot executor or SQL worker is added.

The binding preserves source/configuration/instance/graph identity. A component
generation may change while the coordinator resource remains valid, but a second
source instance cannot acquire it until the old instance releases its ownership.
Resource retirement permanently fences new acquisitions; cancelled or failed
cleanup does not authorize revoking readers or discarding worker leases.
Ordinary factories and standalone file bootstrap need no coordinator.

## Optional query-owned bootstrap

The independent `drasi_computation_plugin_bootstrap_v1` entry point exposes
`PluginDefinition::with_bootstrap_factories`. Bootstrap-only libraries use
`PluginDefinition::new_with_bootstrap(id, version, Vec::new(), bootstrappers, schemas)`
instead of registering a dummy processing component. Empty libraries still
reject, and the host validates the complete factory catalogue at load time.
Metadata-only inspection validates the advertised extension table without
calling the plugin entry point; catalogue discovery happens during loading.
This adds no component role and does
not change base metadata, ABI **1.0.0**, wire **1**, or existing optional tables.
The Host SDK exposes the negotiated `NativePlugin::bootstrap_factories()`.
Plugin-side factories return `NativeBootstrapProvider`; only the host-side
`NativeBootstrapFactory::create_provider` returns a real
`ComputationBootstrapProvider`, suitable for a query's `QueryBootstrapResource`.
Factories only configure providers: preparation and snapshot work remain
query-owned, and Server recipe integration is separate from this Host API.

A factory declares whether it requires source progress. Both SDK and Host reject
missing, undeclared or wrong-graph bindings. The provider's `recovery_reader()`
must preserve the actual supplied reader, including during preparation. Query
activation verifies the actual local progress owner, not matching names.
Dropping the host provider revokes foreign progress callbacks even if an old
snapshot handle remains retained.

Preparation and snapshot creation may use the borrowed `BootstrapState` service.
Its bounded request mailbox is served inside the host's original query-state
borrow, never by transferring a Rust pointer or spawning another executor.
Initialization state is **1..=65,536 bytes**. A failed, cancelled or unfinished
state request invalidates the operation even if the plugin ignores its error;
retained callbacks are revoked before the operation returns. Persist intent
before external initialization effects. The final `completion_state()` is
returned to the query and committed with its watermarks/completed marker, not
written eagerly by the bridge.

Completed bootstrap is reused on healthy restart. `has_pending_snapshot()`
defaults to false and requests an additional snapshot only when preparation has
produced new work, such as a source subscription without a checkpoint. Initial
bootstrap and explicit reset can request a snapshot without that refresh signal.
Preparation still permits exactly one snapshot before cleanup and reuse.

Snapshots transfer one envelope per caller-polled operation. Control frames are
bounded to **1 MiB**, with **4,096 watermarks** per response; envelopes retain the
existing **64 MiB** wire bound. Stream generations prevent an old stream from
reading or cancelling its replacement. No-snapshot replay, refresh and explicit
reset requirements stay distinct; failures never become empty snapshots.

Dropping a stream requests cancellation. The owner must await `stop()` before
releasing storage or replacing the provider. Cancelled/failed cleanup retains
the provider and blocks reuse until cleanup completes. Panicking stream state
remains unusable even after worker cleanup; replace that provider. The shared
plugin I/O drivers service timers/I/O, but the host polls all bootstrap operations.
Current-thread Host tests include a separately built fixture, real RocksDB
snapshot/handover commits, reconstruction without resnapshotting, and preserved
initialization intent after cancelled startup. These coordinated-recovery cases
are framework fixtures, not a migrated production live database source.
The independent
[`drasi-computation-bootstrap-sqlite`](../computation-plugins/bootstrappers/sqlite)
library now provides ordinary read-only file snapshots and is exercised through
real native C entry points, including errors, stream replacement and two hosted
queries on both runtime flavors. It deliberately has no durable handover or
source-progress binding.

## Remaining native limitations

Arbitrary Rust resource injection across the ABI is not implemented. Native query
construction, atomic/non-atomic publication, snapshots/outbox,
subscriptions, scheduling and selected graph-owned resource bindings are
implemented. Reconfiguration uses the graph's actual replacement/lifecycle
contract; it must not swap a running component's resources behind its owner.
Source injection is limited to the negotiated admission and progress bindings.
Unsupported capabilities/interfaces are rejected. Query semantic metadata and
query-row **data schemas** may be used by ordinary transformers/sinks, but they
do not themselves provide a recoverable query engine.
