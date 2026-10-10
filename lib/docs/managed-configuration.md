# Optional managed configuration

DrasiLib can accept desired component definitions, reconcile them with its
ComputationGraph, and optionally commit every accepted definition
to an external transactional store. **Persistence is off by default.**

This is configuration durability, not automatic persistence of event data or
external effects. Existing index, WAL, outbox and checkpoint providers retain
their own processing-state contracts.

## Lightweight and managed use

`DrasiLib::builder().build()` and ordinary object-based source/reaction/transformer
injection continue to work without a factory registry or configuration database.

Opt into management with `with_management(ManagementOptions)` or the individual
builder methods:

- `with_component_factories(FactoryRegistry)`: statically registered factories or
  factories supplied by the existing Host SDK.
- `with_configuration_store(Arc<dyn ConfigurationStore>)`: enables durable
  acceptance and restoration. Omitting it keeps managed definitions in memory.
- `with_management_resources(Arc<dyn ManagementResourceResolver>)`: constructs
  declared provider recipes. Database, plugin loading and key-management
  implementations remain outside drasi-lib.

The builder restores saved declarations and attempts their construction. It does
not require every component to construct successfully. Call `start()` to activate
configured auto-start components. Start/stop are runtime operations: restart
uses the persisted auto-start policies, not the last observed Running state.

`stop()` followed by `start()` resumes the same instance. `shutdown()` is terminal:
create a new instance to restart after shutdown. In lightweight direct-addition
mode the caller supplies new source/query/reaction objects again, with the same
processing-storage identities; opaque objects are not serialized automatically.
In persistent managed mode, supply the factories, resource resolver and
configuration store to the new builder. It reloads the accepted definition without
resubmitting `apply_desired_state`, including declarations whose factory is
temporarily unavailable.

Hosts that initialize from a startup file only once can use
`RedbConfigurationStore::initialize_if_absent` before building the instance.
`initialize_if_absent_with` evaluates a fallible seed only for an uninitialized
namespace, so obsolete startup definitions do not block accepted stored state.
Both retain exclusive ownership through the presence check and atomic commit.
Record presence, not revision zero, distinguishes initialization: accepting an
unchanged empty definition legitimately keeps revision zero. Seeds use the same
`DesiredInstance::normalized()` validation and canonical ordering as ordinary
acceptance; normalization never constructs components or providers.

Dynamic libraries are **not required**. Static factories work equally well.
For dynamic plugins, the application loads/verifies libraries with Host SDK at
startup and passes `PluginRegistry::computation_factory_registry()`. Core never
downloads or opens an executable library based solely on persisted data.

The Host SDK's
[`managed_instance` example](../../components/host-sdk/examples/managed_instance.rs)
opens/restores a named instance using a caller-selected native library and
externally provisioned encryption key, and optionally applies a JSON desired
definition. It demonstrates the real APIs without requiring a Server or another
public wrapper class.

### QoS resource recipes

Managed definitions can use `DesiredPipe::Qos` and a graph-owned `qos` resource
recipe. `HostManagementResources::with_index_provider(name, provider)` registers
external processing storage for durable channels; a volatile channel needs none.
The same provider can back query resources with
`{"kind":"indexes","provider":"processing-storage"}`.
These are independent of the configuration store. Factory `record_schemas()`
supplies executable validators for custom persisted records. See the
[QoS guide](computation-graph-qos.md) for the complete recipe and retention rules.

## Resource dependencies

`DesiredTopology.resource_dependencies` maps each dependent resource to its
required resource IDs and roles. It is optional; independent resources retain
their existing behavior. For example:

```yaml
resource_dependencies:
  outgoing-journal:
    processing-storage: IndexBackend
```

The graph validates missing resources, roles, cycles and ownership before
effects. A borrowed resource cannot depend on graph-owned storage, because the
graph cannot control the borrowed user's lifetime. Providers construct before
their users; cleanup runs in the opposite dependency order. Failed or cancelled
cleanup retains the provider and blocks replacement. A failed constructor blocks
its descendants, not unrelated resources.

Resolvers opt into `resolve_with_dependencies`; constructors can implement
`construct_with_dependencies`. They receive only the actual handles named in
their declaration, not a global registry. Existing implementations reject a
nonempty dependency context by default. `management::resource_constructor`
binds a recipe for graph-owned deferred construction without starting a second
management service.

Changing a provider reconstructs its transitive resource users and affected
components. Removing a referenced provider rejects unless its users are removed
explicitly or through cascade. Subset snapshots include transitive prerequisites;
inspection exposes resource-to-resource links. Direct builders support
`resource_dependency`, and desired definitions expose
`resource_construction_order` for validation.

This is ownership ordering, not permission to discard processing obligations.
It neither migrates storage nor proves arbitrary path/identity changes safe
across accepted-definition restart.

## Desired-state API

`management::DesiredInstance` contains `version: 1` and a `topology` describing
managed components, their connections and resource recipes. Each component has
its own `lifecycle.auto_start` policy. `DesiredInstance::from(topology)` creates
an instance-scoped definition from an assembled topology.

```rust,ignore
let previous = drasi.desired_configuration()?;
let receipt = drasi.apply_desired_state(
    previous.revision,
    "unique-client-request-id",
    desired,
).await?;

assert!(receipt.durable); // only when a ConfigurationStore is configured
let status = drasi.reconcile_desired_state().await?;
if !status.converged() {
    // Inspect status.error, status.resource_errors and component observations.
}
```

The supplied definition is authoritative for its managed members:

- Omitted managed components and resources are removed.
- Unchanged healthy components are preserved rather than reconstructed.
- Changed definitions use ComputationGraph's scoped reconciliation.
- Missing factories and constructor failures remain declared and inspectable.
- Registering an unavailable factory later triggers another reconciliation pass.
- Failed cleanup remains visible and retryable; persistence does not make
  lifecycle effects atomically rollbackable.

QueryGraphs and the internal observability source are runtime
implementation details, not user reconstruction recipes. In memory-only mode,
unmanaged object-injected components may coexist; applying managed definitions
does not implicitly adopt or delete them. Conflicting component or resource IDs
are rejected. All members belong to the same ComputationGraph.

In persistent mode, preconstructed builder components and ordinary imperative
configuration mutations are rejected with guidance to use `apply_desired_state`.
They cannot be promised automatic reconstruction. Use factory specifications for
sources, queries, transformers and sinks. Controller operations also reject
configuration writes to managed members that bypass this API. Read-only inspection, data processing
and runtime start/stop remain available.

This is an explicit contract: metadata on an arbitrary Rust object is not a
constructor. Component `configuration()` getters do not make such an object
automatically restorable.

### Changing recovery storage and its users

Standard Host and Server resolvers reject changes to persistent recovery domains
before acceptance unless the runtime can prove safe retirement. This includes
storage paths, producer definitions, destination membership, removal and downgrade.
Connected components and resource dependencies belong to the same domain;
unrelated domains and lifecycle-only settings remain independently editable.
Resolvers implementing persistent services must implement `validate_transition`,
and wrappers must forward it. Missing live resources never mean empty storage.

Verified drain supports constructed shared-storage domains, standalone persistent
QoS journals, initialized built-in queries/native transaction sequences, and
native consumers with validated completed ledgers and successful storage cleanup.
Bootstrap providers must supply their own positively stopped retirement gate;
source-progress resources must match their actual processing owner.
Quiesce producers and stop the instance after handling its outstanding output,
then submit the desired change. Ordinary stop may interrupt an active handoff
and require reconstruction. The graph
verifies successful component stop, blocks restart/configuration mutations, and
holds the actual storage-group gates while checking producer output, every live
journal's required cursors, and scheduled work. Stopped status alone, unused
storage declarations, or an unlocked progress sample are insufficient.

These leases remain held through configuration commit and authoritative readback.
A confirmed rejection releases the same old owners. Acceptance fences them and
reconstructs changed domains, preserving provider recipes even for same-path
replacement. Uncertain acceptance retains the leases until reconciliation can
read authoritative state. Cancelling a waiting API caller does not cancel the
management owner. Abandoning the proof itself fences affected ownership and
requires instance reconstruction; it never grants permission to discard work.
Explicit shutdown can revoke its own held storage gates only after terminally
closing the owners; it does not resolve or overwrite uncertain configuration.
The next instance restores whichever definition the store actually accepted.

Server returns HTTP 409 `CONFIGURATION_TRANSITION_REQUIRED` for a definite
pre-acceptance refusal, with no new revision or receipt. HTTP 503 unconfirmed
configuration is different: consult the request receipt and reconcile after
storage recovers. Unsupported providers, pending output or timers without explicit
loss permission, and unavailable initialization remain refused. Replacing storage is not data migration, and
acceptance still does not promise that the replacement can become ready.

### Explicit loss-authorized removal

`DesiredInstance.retirement: Option<RecoveryRetirementAuthorization>` permits
abandonment of pending obligations only when removing complete recovery domains.
Set `allow_data_loss: true`, `from_revision` to the current configuration revision,
and `resources`/`components` to the exact members of every changed domain.
All named members must be absent from the new topology; connected unmanaged
users, omitted members, extra members and in-place reuse are refused.
The authorization requires a durable store and commits with the existing
definition and idempotency receipt. The identical request remains retryable;
carrying accepted permission forward grants no new authorization.

The same actual storage gates and graph freeze remain mandatory. Permission
relaxes pending-work checks and permits a previously failed processing operation
on an actually stopped component, never failed cleanup or unhealthy transaction
owners. A native consumer may abandon partial/unknown completion only after its
actual delivery storage has successfully closed. Bootstrap stop and actual
provider/progress identity requirements are unchanged.

Accepted removal does not delete/reset journals, indexes, checkpoints or business
state, acknowledge input, or undo external effects. Restart follows the accepted
removed topology; explicitly restoring the original definition later can replay
the intact work. There is no reset/reuse mode, migration, or permanent storage
tombstone. Definite rejection resumes old owners; unknown acceptance retains
the leases until authoritative resolution or terminal shutdown.

## Acceptance, realization and retries

The management driver serializes configuration requests. It commits the target
definition and idempotency receipt **before** invoking factories or applying
graph lifecycle changes. It then reconciles in an owned task, so cancelling the
caller waiting for a response or a reconciliation pass does not abandon accepted
work.

`AcceptanceReceipt` contains the request ID, definition revision and `durable`.
Acceptance does not mean readiness. `management_status().await` combines the last
management pass with live component observations; `reconcile_desired_state()`
waits for another pass. Inspect operation, resource and component failures rather than
treating an accepted request as a fully operational graph.

Definition revisions differ from the ComputationGraph's `GraphRevision`.
Expected-revision checks prevent lost concurrent updates. Reusing a request ID
with the same definition returns its original receipt; different content under
that ID is an error. An identical current target need not advance the definition
revision or restart healthy components.

When a store reports an uncertain commit outcome, the API does not guess success
or rejection. Use `configuration_receipt(request_id)` and the current committed
definition, or repeat the same request. A failed acceptance before commit cannot
start constructing its components. The driver reloads authoritative state even
when the commit response fails: a confirmed new definition becomes the live
target without restarting the instance. If confirmation is unavailable or
inconsistent, `desired_configuration()` returns an error and status cannot claim
convergence. `reconcile_desired_state()` reloads the store before retrying.

Resource replacement follows the same scoped processing boundary as component
replacement. The graph pauses affected work, stops dependent owners and closes
the old graph-owned resource before invoking its replacement resolver. The new
target is visible while construction is pending. Failed construction leaves a
retryable, nonconverged resource, not a requirement to open two exclusive owners
of the same database or listener. Unrelated graph branches remain running.
Resources returned with an incorrect role are never injected; the graph retains
cleanup responsibility for graph-owned instances, including failed cleanup.

Factories must obey the existing cancellation-safe construction contract.
Resource resolution has a 30-second timeout and must also be cancellation safe.
Unavailable resources remain identified in the management report and graph
observations. Call `shutdown().await` to finish graph cleanup and release the
instance's exclusive configuration-store session; it is not equivalent to
merely dropping a Rust handle.

## Shared encrypted redb storage

The external `drasi-state-store-redb` crate exposes `RedbConfigurationStore` under
its opt-in `configuration` feature:

```toml
drasi-state-store-redb = { version = "0.2", features = ["configuration"] }
```

Use the matching local workspace dependency until the feature is released.

```rust,ignore
let store = Arc::new(RedbConfigurationStore::new("graphs.redb", key_from_host)?);

let a = DrasiLib::builder().with_id("a")
    .with_component_factories(factories.clone())
    .with_configuration_store(store.clone())
    .build().await?;
let b = DrasiLib::builder().with_id("b")
    .with_component_factories(factories)
    .with_configuration_store(store)
    .build().await?;
```

One provider/database supports many instance IDs in one process. Each instance
has one active configuration owner. Clone/share the provider; do not separately
open the same redb file for each instance. redb holds the process-level database
lock; concurrent independent processes sharing the file are not supported.

Commits use redb transactions with `Durability::Immediate`. The current
definition and request receipt commit together. Snapshots are atomic immutable
copies of committed configuration. Request/snapshot records are retained; there
is no automatic history pruning unless receipt expiry is explicitly enabled.

All definition, receipt and snapshot values use authenticated XChaCha20-Poly1305
encryption. The host supplies a 32-byte key; no key or plaintext fallback is
stored in the database. A key-check record rejects the wrong key at open.
Instance/request/snapshot names remain visible as table keys. Use appropriate
filesystem permissions and a real external key provider. Losing the key loses
access to stored configuration.

### Bounded request history

Default stores and memory-only management retain arbitrary request IDs and their
original receipts indefinitely. Applications opting into expiry use
`RedbConfigurationStore::new_with_options(path, key, ConfigurationStoreOptions {
receipt_batch_capacity: Some(capacity) })`, with a caller-chosen `NonZeroUsize`.
This bounds accepted receipts **per instance**, not total database bytes:
definitions can vary in size, and snapshots and instance namespaces remain separate.

Obtain each new operation's ID with `drasi.new_configuration_request_id().await`
(or `ConfigurationSession::new_request_id`). Save that ID and reuse it for all
retries of that operation. Do not generate a replacement ID to resolve an uncertain
outcome. The helper also works without expiry, returning an ordinary UUID.
The management driver refuses ID generation while acceptance is unconfirmed.
For one-time initialization, obtain an ID from a temporary configuration session,
close that session, then supply the ID to `initialize_if_absent`.

Expiry IDs identify a persisted namespace and batch. A batch accepts at most
`capacity` distinct requests, including accepted no-op changes. Once full, generating
another ID atomically removes that batch's receipts and advances the batch number.
All its IDs expire together, including generated-but-unused IDs. This is **not**
a time-to-live or sliding retention window. An unused ID submitted to a full batch
returns `RequestBatchFull`; old/foreign batch IDs return `RequestExpired`.
Arbitrary or malformed IDs return `GeneratedRequestIdRequired`. Reusing a retained
ID still returns its original receipt, or rejects changed content.

Expiry means the old outcome is no longer available, **not that it failed**.
Inspect the current committed definition and decide whether a genuinely new
operation is appropriate. Expired IDs never become fresh requests after ordinary
reopening; constant-size batch metadata replaces permanent expired-ID tombstones.
Snapshot restoration does not rewind this metadata.

Enabling expiry on an existing database expires its legacy receipts immediately,
while preserving current definitions and named snapshots. It updates the database
format so older software cannot reopen it under indefinite-retry semantics.
The capacity is persisted; reopening requires the same explicit policy. Disabling
or resizing it is rejected rather than silently changing retry guarantees.
Standard Host/Server configuration recipes do not expose these owner options.

### Snapshot cleanup

`list_configuration_snapshots(after, limit)` returns a bounded page of names and
saved revisions, without returning definitions. Pass the last returned name as the
next exclusive cursor; ordering is provider-defined.
`delete_configuration_snapshot(name, expected_revision)` removes only the matching
saved revision, returns false for an absent name, and rejects a revision mismatch.
Both memory management and redb support these operations. There is no automatic
age-based deletion. Cleanup does not change current configuration, request
receipts, processing state, or external effects.

### Encryption-key rotation

`store.rotate_key(replacement_key).await` atomically re-encrypts all live values,
including receipt-policy metadata, in one immediate-durability transaction.
Close every configuration session first, normally through each instance's
`shutdown().await`. Rotation refuses live sessions and blocks new opens until
the actual storage worker finishes, even if its waiting caller is cancelled.

Provision the replacement key externally **before** rotation and retain both keys
until success or reopening confirms the active key. A precommit failure preserves
the old key. A failure with an uncertain commit result fences the provider:
drop/reopen it with the externally retained keys to establish which is active,
rather than attempting writes with an unconfirmed in-memory key.

Rotation reads and rewrites one record at a time but an atomic rewrite can need
substantial temporary disk space. It does not re-encrypt old free pages, backups,
or filesystem snapshots and is **not secure erasure** of old ciphertext. Backup
key retention and secure disposal remain operator responsibilities.

## Secrets and snapshots

Host SDK's `HostManagementResources` supports `configuration` recipes with named
local secrets, or an externally supplied `SecretStoreProvider`. Graph fields keep
references such as `secret:DB_PASSWORD`; values resolve during construction.
Locally supplied secret values live in the encrypted desired definition and
commit atomically with references to them. Changing the resource recipe causes
dependent components to be reconciled with the new resolver.

`desired_configuration`, `snapshot_desired_configuration(name)` and
`load_configuration_snapshot(name)` are **privileged Rust APIs**. Their returned
definitions may contain literal secrets in provider recipes or component fields.
The database stores them encrypted, but returned Rust values are plaintext.
Do not log or serve them through unprivileged endpoints. Public topology
inspection remains separate.

`restore_configuration_snapshot(name, expected_revision, request_id)` applies an
old definition as a new accepted revision. It does not rewind processing state,
index data, credentials held in external vaults, or external side effects.
Existing state/configuration compatibility checks still apply.

## Implementation boundaries

`ConfigurationStore` / `ConfigurationSession` are narrow transactional interfaces,
not aliases for `StateStoreProvider::set_many`: a generic state store may permit
partial writes and is not sufficient for durable configuration acceptance.
Custom session wrappers must forward request-ID generation and snapshot
housekeeping to preserve their provider's policy. Snapshot housekeeping defaults
to an explicit unsupported-operation error for providers that do not implement it.

`HostManagementResources` supplies standard memory-index, middleware,
transactional-registry and configuration-resolver recipes. Applications supply
additional `ManagementResourceResolver` implementations for their providers.
No opaque provider object is inferred from its type or serialized as a pointer.
Existing descriptor-backed plugin factories and adapters remain available through
Host SDK / Plugin SDK; legacy plugin loading and ABI 0.15 are unchanged.
