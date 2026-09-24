# Native ComputationGraph plugin SDK

This is a separate plugin family, not a replacement or layout change to the
Source/Reaction/Bootstrap SDK. It uses `drasi-computation-plugin-abi` **1.0.0**
with native **wire version 2**, independent of workspace package versions.
Legacy ABI **0.15.0** is unchanged. Rebuild native plugins with this SDK; the
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

## Explicit ABI 1.0 limitations

Dynamic query-role snapshot/outbox hosting, source recovery-progress handles,
graph resource injection and in-place reconfiguration are not implemented.
Query-role metadata and unsupported capabilities/interfaces are rejected. Query-row
**data schemas** may still be consumed or produced by ordinary transformers/sinks;
this does not advertise a recoverable query engine.
