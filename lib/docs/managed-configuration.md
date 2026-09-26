# Optional managed configuration

DrasiLib can accept a complete desired set of computation graphs, reconcile it
with its live graph instances, and optionally commit every accepted definition
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

## Desired-state API

`management::DesiredInstance` contains `version: 1` and a list of `DesiredGraph`
entries, each with an `auto_start` policy and a complete `DesiredTopology`.

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
    // Inspect status.graphs and each graph's coherent runtime observations.
}
```

The whole supplied managed graph set is authoritative:

- Missing managed graphs/components are removed.
- Unchanged healthy components are preserved rather than reconstructed.
- Changed definitions use ComputationGraph's scoped reconciliation.
- Missing factories and constructor failures remain declared and inspectable.
- Registering an unavailable factory later triggers another reconciliation pass.
- Failed cleanup remains visible and retryable; persistence does not make
  lifecycle effects atomically rollbackable.

Generated internal query graphs and the internal observability source are runtime
implementation details, not user reconstruction recipes. In memory-only mode,
unmanaged object-injected components may coexist; applying managed definitions
does not implicitly adopt or delete them. A conflicting unmanaged graph name is
rejected.

In persistent mode, preconstructed builder components and ordinary imperative
configuration mutations are rejected with guidance to use `apply_desired_state`.
They cannot be promised automatic reconstruction. Use factory specifications for
sources, queries, transformers and sinks. Managed graph controls also reject
configuration writes that bypass this API. Read-only inspection, data processing
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
waits for another pass. Inspect per-graph and per-component failures rather than
treating an accepted request as a fully operational graph.

Definition revisions differ from each running graph's `GraphRevision`.
Expected-revision checks prevent lost concurrent updates. Reusing a request ID
with the same definition returns its original receipt; different content under
that ID is an error. An identical current target need not advance the definition
revision or restart healthy components.

When a store reports an uncertain commit outcome, the API does not guess success
or rejection. Use `configuration_receipt(request_id)` and the current committed
definition, or repeat the same request. A failed acceptance before commit cannot
start constructing its components.

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
