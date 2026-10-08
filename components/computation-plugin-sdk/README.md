# Native ComputationGraph plugin SDK

This is a separate plugin family, not a replacement or layout change to the
Source/Reaction/Bootstrap SDK. It uses `drasi-computation-plugin-abi` **1.0.0**
with native **wire version 2**, independent of workspace package versions.
Legacy ABI **0.17.0** (also accepting 0.16 fast-mode plugins) is independently versioned. Rebuild native plugins with this SDK; the
unreleased JSON-envelope wire prototype (version 1) is explicitly rejected.

## Authoring

A plugin exports configuration-only factories for the existing
`drasi_lib::computation::v1` `EnvelopeSource`, `Transformer`, `EnvelopeSink` and
`ComputationService` interfaces. It does not wrap them in legacy Source/Reaction
adapters or introduce another graph executor.

1. Implement the appropriate native graph trait, including the real
   `ComputationComponent::configuration()` persistence hook.
2. Implement this SDK's `Factory`. Its `metadata()` declares implementation and
   configuration versions, a `ConfigSchema`, fixed port/schema descriptors, sink
   completion and explicit `Capabilities`. Its synchronous `create()` receives a
   `CreateRequest` and `ControlSender`, returning a `CreatedComponent`.
3. Register factories and executable schemas in `PluginDefinition::new`.
4. Export only behind the plugin's `dynamic-plugin` feature:

```rust,ignore
use std::sync::Arc;
use drasi_computation_plugin_sdk::{PluginDefinition, export_computation_plugin};
use drasi_lib::computation::v1::GraphChangeCodec;

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

## Scheduling and ownership

The host owns start, data processing, stop and cancellation. Each operation is a
producer-owned opaque handle with cooperative, nonblocking poll, retained wake
callbacks, cancellation and release. Mutable data/lifecycle calls cannot overlap.
Cancellation drops the future immediately; it does **not** promise rollback of
already performed state changes or external effects. Failed, panicked, cancelled
and invalid operations return structured failures, not empty successful output.

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

Metadata and configuration are UTF-8 JSON. Native wire version 2 uses named
MessagePack records and bulk MessagePack binary buffers, not JSON envelopes or
per-byte integer arrays. `BinaryEnvelopeCodec` encodes the full schema descriptor,
typed images, sequence, stream, source position, context identity, annotations
and lineage. It is separate from the unchanged persisted JSON `EnvelopeCodec`;
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

The binary envelope is a named map with `format: 2`, `id`, `change_set`, `schema`,
`operations`, `system`, `lineage`, `context_identity` and `annotations`.
Identities contain `namespace` and binary `value`; schemas contain `id`, `version`,
`encoding` and binary `definition`. Operations retain the `Add`, `Update` and
`Delete` variants and their ordinals. Record image kinds are `0` full, `1` patch,
and `2` partial. Timestamps retain their RFC3339 representation and full precision.
Lineage and annotations are transmitted newest first; decoding restores their
original semantics, including duplicate annotation keys. Unknown fields, tags,
versions, schemas and invalid record/change-set contracts fail explicitly.

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

## Optional host-owned consumer completion

The independently negotiated `drasi_computation_plugin_consumer_v1` extension
leaves base ABI 1.0 and wire 2 unchanged. Declare `Factory::consumer_mode()` as
`External` or `Transactional`, with a handled, non-snapshot sink descriptor.
Return `Component::Consumer` for a `ComputationComponent + DeliveryHandler`, or
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
extend service-v1, strict base metadata, ABI **1.0.0**, or wire version **2**.
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

## Optional query-owned bootstrap

The independent `drasi_computation_plugin_bootstrap_v1` entry point exposes
`PluginDefinition::with_bootstrap_factories`. It adds no component role and does
not change base metadata, ABI **1.0.0**, wire **2**, or existing optional tables.
The Host SDK exposes the negotiated `NativePlugin::bootstrap_factories()`.
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
initialization intent after cancelled startup. These are framework fixtures,
not migrated production database plugins.

## Remaining native limitations

Dynamic query-role snapshot/outbox hosting, general
graph resource injection and in-place reconfiguration are not implemented.
Source injection is limited to the negotiated admission and progress bindings.
Query-role metadata and unsupported capabilities/interfaces are rejected. Query-row
**data schemas** may still be consumed or produced by ordinary transformers/sinks;
this does not advertise a recoverable query engine.
