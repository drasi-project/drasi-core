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
is no automatic history pruning or key rotation.

All definition, receipt and snapshot values use authenticated XChaCha20-Poly1305
encryption. The host supplies a 32-byte key; no key or plaintext fallback is
stored in the database. A key-check record rejects the wrong key at open.
Instance/request/snapshot names remain visible as table keys. Use appropriate
filesystem permissions and a real external key provider. Losing the key loses
access to stored configuration.

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

`HostManagementResources` supplies standard memory-index, middleware,
transactional-registry and configuration-resolver recipes. Applications supply
additional `ManagementResourceResolver` implementations for their providers.
No opaque provider object is inferred from its type or serialized as a pointer.
Existing descriptor-backed plugin factories and adapters remain available through
Host SDK / Plugin SDK; legacy plugin loading and ABI 0.15 are unchanged.
