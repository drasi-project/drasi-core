# Drasi Host SDK

## Component lifecycle and managed updates

The host binds runtime services before calling `initialize` once for each
constructed object. That object may then go through repeated `start`/`stop`
cycles before final `dispose`. A replacement object has a new lifetime even
when its logical component UUID is retained. Failed initialization still needs
disposal; interrupted cleanup remains owned until it actually finishes.
`pause_processing` and `resume_processing` are temporary processing boundaries,
not destruction. `delete_owned_resources` is separate, explicit permanent
cleanup: neither an ordinary stop nor disposal implies deleting persisted data.

Managed updates require a registered factory recipe whose author implements
update planning. The host asks that factory whether to retain the object,
restart it, reconstruct it, or reject the change, and whether state can be
preserved or must be rebuilt from a bootstrap. A plan is not permission to
discard unconfirmed output, shared history, or another consumer's data. The
configuration owner records the accepted operation identity before execution
and confirms completion before processing resumes. Configuration durability
does not itself provide durable processing state.

**Current Core limitation:** every managed `Rebootstrap` plan is rejected,
including when a bootstrap provider is available. Initial bootstrap is
unaffected. Snapshot/live handover, exact queued-work treatment and downstream
reset acknowledgement still need a graph-owned protocol; see
[CW-92](../../lib/docs/computation-graph-remaining-work.md#cw-92).

Externally supplied objects without an update-capable recipe use explicit graph
changes and remove/add instead. Their normal stop/disposal hooks still run.
The ordinary Source/Reaction ABI **0.18.0** exposes lifecycle hooks but does not
advertise factory-managed updates. Native ABI **3.0.0** carries factory plans and
retained-object updates; earlier native ABI layouts are rejected. The generic
native adapter rejects update assessment requiring unresolved configuration
references or construction-time input bindings rather than inventing values.
Native libraries remain process-pinned: reconstructing a component is not
hot-swapping its library.

See the [native SDK guide](../computation-plugin-sdk/README.md) for plugin hooks,
capability declarations, and update request types.

## Native activation identity

Factories negotiating `activation_credentials_v1` accept an optional `identity`
dependency with `ResourceRole::Identity` and concrete type
`computation::NativeIdentityResource`. Static callers supply the actual existing
`Arc<dyn drasi_lib::identity::IdentityProvider>` with
`NativeIdentityResource::new(provider)` and `Arc::new(resource).resource()`, or use
`NativeFactory::create_component_with_identity(id, configuration, resource)`.
No provider object, trait, Arc or Rust string layout crosses the native ABI.

Host and Server share this reconstructible resource recipe:

```yaml
kind: nativeIdentity
provider:
  kind: your-loaded-identity-provider
  config_version: "1"
  config:
    password:
      kind: Secret
      name: database-password
```

Declare it as an `Identity` resource without resource dependencies, and bind its
ID to the component's `identity` slot. The recipe uses the already loaded ordinary
identity-provider catalogue, its `PluginConfiguration`/descriptor schema checks,
and existing instance-scoped secret resolution. Provider configuration references
remain references in persistence. Provider construction and credential I/O occur
on **each START**, never in a native component or resource constructor.
An injected provider (`with_injected_provider`) takes precedence over a configured
provider. Without an identity binding, literal-only startup is unchanged.
Ordinary native `ConfigurationValue` references remain construction-scoped;
credential rotation requires the explicit identity resource path above, not a
password reference resolved once by the component constructor.

Each activation supplies exactly the configured hostname/port in the ordinary
`CredentialContext`. Failed providers, oversized credentials and certificates
fail before native START, without literal or stale-credential fallback. Provider
errors are sanitized. Cancellation drops the caller-owned resolution future;
there is no cached credential or unrestricted fetch callback that can later
apply old credentials to a replacement.

A coordinated source passes its own Identity dependency to the coordinator it
creates. The Host passes only the construction marker to the coordinator; the
live source still receives credentials through its normal START. This does
not independently add authentication to a connector's bootstrap implementation.
Consumer-wrapper factories currently reject identity dependencies rather than
silently ignoring them.

## Native consumer completion

Native consumer-v1 factories use the Host's existing `DeliveryRunner`, not a
plugin-owned progress store. Programmatic construction uses
`NativeFactory::create_consumer`; graph factory specifications require exactly
one `consumer` dependency, an `IndexBackend` resource whose handle contains
`NativeConsumerResource { provider, options }`. Scope comes from the graph.
External completion and transactional state/completion are distinct persisted
modes; neither upgrades acceptance-only sinks or promises external exactly-once
effects. Ordinary factories require no delivery resource and acquire no ledger.
See the [native consumer contract](../computation-plugin-sdk/README.md#optional-host-owned-consumer-completion)
for lifecycle, storage, retry and handler restrictions.

Managed construction uses a graph-owned `IndexBackend` recipe:
`{"kind":"nativeConsumer","failureScope":"processRestart","maxStreams":16,
"receiptsPerStream":64}`. Its `resource_dependencies` must name exactly one actual
index-provider resource. The runner makes one attempt per delivery; a failed
operation keeps the completed prefix and the graph retries the input in place
under the sink delivery-failure model. The recipe reuses the
declared provider while isolating delivery storage by instance and graph. It
cannot substitute a producer's shared transaction group for the consumer's
own per-operation transaction. Component specifications bind this resource
under `consumer`. Actual constructed storage must meet the requested guarantee.

## Native producer consumer progress

Native standalone transformers and ordinary **Handled** sinks may
additionally bind an optional `input_progress` dependency. It is a `Checkpoint`
resource containing `QuerySourceProgressResource` for that exact graph/component.
The Host attaches `ComponentRecovery::with_volatile_input_progress`; the graph,
not the proxy or plugin, publishes completed input after handling and output
completion. The owner remains volatile (`persistent=false`) and implies neither
restart durability nor a remote transaction. Accepted sinks, sources, services,
transaction participants and consumer-wrapper factories cannot opt into this
tracker. A transformer factory may also support transaction participation; that
capability does not prevent tracking its separately constructed standalone
instances. An instance with this tracker cannot be converted into a transaction
participant. Omitting it preserves the ordinary fast path.

`NativeFactory::create_component_with_input_progress(id, configuration, scope,
input_owner, optional_consumer_set)` provides programmatic construction.
An upstream-facing input owner and downstream `consumer_progress` readers are
different resources and may coexist on a transformer. Only the latter crosses
the native interface; `input_progress` never reaches the plugin.

Native source/transformer factories advertising `consumer_progress_v1` require
one `consumer_progress` dependency: a `Checkpoint` resource containing the actual
`Arc<ConsumerProgressSet>`, not names, a synthetic aggregate or a single-owner
wrapper. Programmatic construction uses
`NativeFactory::create_component_with_consumer_progress(id, configuration, scope, set)`.
Graph/producer identity and exact plugin-side reader binding are checked.
Single-owner progress and source admission cannot be supplied alongside this
binding. Non-opt-in factories keep their existing construction and processing.

The Host attaches the actual set with `ComponentRecovery::with_consumer_progress`.
An optional immutable native cursor comparator is captured separately and attached
with `with_consumer_positions`. It interprets bounded source positions, not a
generic minimum scalar, and can run while source/transformer processing is pending.
Its opaque handle remains producer-owned; no Rust comparator or storage pointer
crosses the ABI. Explicit upstream retention is preserved independently and is
not proof of every consumer's durability or of an external effect.

Snapshots retain every member's own state and enforce whole-set limits (256
members, 4,096 entries, 1 MiB). Independent subscriptions are bounded to 16; failed
encoding and cancelled waits consume no observations. Stop/drop wakes and fences
old subscriptions, and cancelled/failed cleanup cannot become a successful restart.
Only successful stop/start allows fresh subscriptions. These services use the
hosting graph's actual resources; they neither create a graph nor independently
establish graph-wide fan-out/recovery guarantees.

Managed construction uses the `consumerProgress` resource recipe:

```yaml
kind: consumerProgress
producer: input
consumers:
  left: left-progress
  right: right-progress
```

Declare it as a `Checkpoint` resource, with exactly `left-progress` and
`right-progress` as `Checkpoint` resource dependencies. Each member is a
`sourceProgress` recipe for the corresponding actual component. The set is
durable only when every member is. Bind the set to
the producer's `consumer_progress` slot. A query binds its own member resource to
`source_progress`; an eligible native transformer/handled sink binds it to
`input_progress`. Matching names alone do not establish ownership. A transformer's
own input owner is separate from the set observing its downstream consumers.

Secret-bearing native configuration uses canonical component specifications with
`ConfigurationValue` references to declared configuration resources. Server's
readable graph `config` object contains literal values; embedding a reference-like
object there does not turn it into a reference or bypass secret validation.

## Native source data inspection

The Host negotiates the optional `source_schema_v1` extension and exposes its
actual `SourceSchemaReader` through the component's read capability. Schema reads
do not occupy `next()`'s operation slot and cannot retain a removed component
through an inspection handle. The SDK fences activation changes and the Host
fences proxy retirement; neither substitutes configuration metadata or an empty
schema for a failed read. Older binaries without the extension remain supported.
No application SQL read/write API is added to DrasiLib or the native ABI.

## Native output snapshot, replay and follow

The Host negotiates `output_reads_v1` and `output_follow_v1` only for factories
that declare `output_snapshots`, `output_replays` or `output_follows`, and opens
exactly the declared providers at construction. `NativeComponentProxy` exposes
them through `ComputationComponent::output_readers()` as real shared
`SnapshotProvider`, `ReplayProvider` and `SnapshotFollowProvider` objects; no
executor, row store or graph is created. Reads never use the component's data
operation slot. The proxy's fence ends reads and closes follow leases on stop,
restart and release, whatever the library does, and decoded snapshots, replays and
live items are validated (schema, owner, full images, logical progress, complete
tail, item order) before they are returned; malformed or oversized buffers are
`Invalid` or explicit limit errors. A lease's follow acknowledges only items it
actually returned. Providers hold only their own library's handles, released once
when the last provider reference is dropped.

## Native port format selection

When a plugin factory opts in (it declares `Capabilities::port_formats` or an
optional object `ports` configuration field), `NativeFactory`
resolves the literal `ports` configuration against the plugin's own declared
schemas and nothing else. `specification`/`specification_from_config` describe the
selected ports before construction; `validate` and the graph's definition check
reject a specification whose ports differ from the resolution, and an unknown
port or format, before `create`. The factory's `port_formats()` exposes the
catalogue; instances are built with the resolved ports and the Host re-checks the
constructed component's descriptor. No format is converted or defaulted. See the
plugin SDK's "Optional per-port format selection".

## Native bootstrap provider nodes

Each loaded bootstrap factory is registered as a graph component factory,
`NativeBootstrapNodeFactory`, with the `BootstrapProvider` role. A provider node
has one `snapshot` bootstrap output and no live ports. Connect it to a source's
`bootstrap` input, or directly to a query's or transformer's `bootstrap` input
as a local provider. `PluginRegistry` exposes `computation_bootstrap_factories()`
and `computation_bootstrap_metadata()` from the already loaded libraries.

A provider node serves one provider instance per bootstrap session, created for
the consumer named in the session request. Its specification carries the
factory's `configurationVersion` and `configuration`; configuration references
declare their `SecretStore` dependencies and remain references in persisted
definitions. Optional dependencies:

| Slot | Role | Use |
|---|---|---|
| `source_progress` | `Checkpoint` | One or more query progress owners. A session uses the owner whose query matches the request's origin consumer; durable factories require it. |

A source-paired factory takes no extra slot: it must be connected to the
`bootstrap` input of a coordination-capable native source and uses that
source's coordinator and progress owners. Construction rejects a paired
provider whose configured `source` is not the connected source or whose
configured `stream` is not one the connected source publishes.

A provider node accepts bootstrap requests that arrive before it starts and
serves them once it is running; requests fail only after it is disposed.

A durable session forwards the consumer's persisted bootstrap state to the
provider and returns the provider's state markers to the consumer. A consumer
with completed progress does not bootstrap again after restart. Stopping the
node stops in-flight sessions and awaits provider cleanup.

Native sources relay bootstrap sessions from their connected provider to the
requesting consumer and add the source's handover boundary. Every session,
including one for a consumer that joins later, gets its own snapshot and
boundary; the consumer skips live changes its snapshot covers. Only a source's
very first start, when none of its progress owners has a saved position or
completed bootstrap, holds the live stream until the first boundary; START then
carries `fresh_start: true` (`StartContext::fresh_start()`). Restarts and late
joiners never pause the live stream.

Native transformers that declare `quiescent_bootstrap` or `durable_bootstrap`
bootstrap over their `bootstrap` input edges when any are connected. Native
transformers that publish output snapshots (such as the native continuous
query) expose a `snapshot` bootstrap output served from their committed output
snapshot, so downstream queries and transformers bootstrap from them over an
edge.

## Native source bootstrap coordination

Sources negotiating `bootstrap_coordination_v1` may share their actual engine
with a separately loaded paired bootstrap factory. This is not a SQL or general
service API, and there is no coordinator resource or recipe to declare. The
source coordinates: when a provider is connected to its `bootstrap` input, the
source creates its library's coordinator (`NativeSourceBootstrapResource`) from
its own `source_progress` or `consumer_progress` owners and registers it for its
instance, graph and component. Registrations are scoped to the running graph, so
parallel runtimes that reuse instance and component IDs never share a
coordinator. A paired provider node finds the coordinator of the source its
`snapshot` output is connected to in its own graph, and serves each session with
the progress owner of the requesting consumer. In shared mode the source knows
its bootstrap consumers from its consumer progress set.

```text
paired provider.snapshot -> source.bootstrap
source.out               -> query.in
source.snapshot          -> query.bootstrap
```

Pairing requires Arc identity, not matching IDs, paths, configuration, or an
equal reconstructed member map. Coordinator-owned group/member read
capabilities activate when the coordinator is created, without snapshot,
subscription, or network I/O. The source still receives its separate normal
live binding, activated by source lifecycle. Cleanup retains an accepted STOP
operation and readers across cancelled/timed-out awaits and failed cleanup.
Successful final cleanup revokes every reader. Paired query providers retain
the real query recovery reader and use the existing bounded bootstrap
operations and retirement behavior. Standalone file bootstrap and ordinary
source construction are unchanged.

A successfully stopped paired provider may be replaced even while its old handle
is retained. The old lease cannot resume, cancel, or stop the replacement. Active
providers and incomplete, cancelled, or failed cleanup remain fenced until the
actual cleanup operation succeeds; dropping their handles does not erase that
obligation. A rejected replacement open, reader, or scope leaves the stopped
original usable. Dropping a never-started source permits reconstruction, but
dropping after failed or cancelled START/STOP does not bypass cleanup.
Retained subscriptions remain usable through
failed or cancelled coordinator cleanup and are revoked only at final retirement.

Programmatic assembly outside a graph may still use
`NativeFactory::create_source_bootstrap(...)`,
`create_component_with_source_bootstrap(...)`, and
`NativeBootstrapFactory::create_paired_query_provider(...)` with
`NativeSourceBootstrapProgress::{Single, Shared}` owners.

The focused Host coordination tests cover both progress modes, both Tokio runtime
flavors, real malformed/partial library discovery, constructor callback counters,
exactly-once release of transferred owners, actual owner identity,
retained-provider replacement, dropped cleanup obligations, and subscription retirement.
Build the discovery fixtures before running this suite:

```sh
cargo build -p drasi-host-sdk --example native_bootstrap_coordination_metadata \
  --example native_bootstrap_coordination_partial
cargo test -p drasi-host-sdk --lib bootstrap_coordination
```

## Integrated desired-state management

Managed graphs support shared QoS channels as well as component factories.
`HostManagementResources::with_index_provider` registers external processing
storage for `qos` recipes. Native factories expose their executable record
schemas so persistent channels use the same validators when replaying data.
See [pipe QoS](../../lib/docs/computation-graph-qos.md) for profiles, subscription
retirement and the difference between durable acceptance and completed effects.

QoS recipes optionally select `recovery` through the shared
`management::QosRecoveryConfig`. `kind: admission` names the actual source
`component`, `failureScope`, `maxProducers` and `receiptsPerProducer`;
`kind: replay` selects `failureScope` and `receiptCapacity` for retained
producer output. The host supplies the actual instance/graph identity, not
configuration-provided identity claims. Both modes require a durable,
backpressured channel and evidence that its storage survives the requested
failure. They cannot be combined. Omission is explicitly disabled: reopening
a tracked journal without the matching mode fails rather than silently
disabling or restoring an unrequested service. Plain volatile recipes retain
their ordinary path without receipt machinery.

Shared transactions use `{"kind":"sharedStorage","provider":"disk","component":"query"}`
for the producer's graph-owned `IndexBackend`, and `{"kind":"sharedQos",
"definition":{...},"recovery":{"kind":"replay","failureScope":"processRestart",
"receiptCapacity":64}}` for each graph-owned journal. The latter has exactly one
`IndexBackend` entry in `DesiredTopology.resource_dependencies`. The graph passes
the actual group handle into its constructor and releases journals before storage.
Its pipe also names the group in `shared_storage`. Same-named independent providers
cannot substitute for that group. The resolver protects persistent recovery
domains before acceptance, including when their live resources are unavailable.
Shared domains can change after successful instance stop and a graph-held proof
that producer output, required journal cursors and scheduled work have drained.
Storage gates and restart protection remain held through durable acceptance.
Initialized standalone query/transaction owners, persistent QoS journals and
native consumers also support verified drain. Consumers use completed-ledger
evidence only after successful storage cleanup; bootstrap providers hold their
own stopped lifecycle. Unsupported owners and unresolved obligations still
reject ordinary changes. Explicit revision-bound loss authorization only permits
complete domain removal, never data deletion, in-place reset or migration; see
[managed retirement](../../lib/docs/managed-configuration.md#explicit-loss-authorized-removal).
Resolver wrappers
must forward `validate_transition` and `resolve_with_dependencies`.

Load and verify libraries with the existing loader/lifecycle APIs, register
them in `PluginRegistry`, and supply `computation_factory_registry()` to
`DrasiLib::builder().with_component_factories(...)`. The registry includes
standard graph factories and the loaded native implementations. DrasiLib does
not load library paths from stored definitions.

`management::HostManagementResources` provides standard resource recipes for
memory indexes, query-result catalogs, source progress, middleware, transactional
factories and configuration references.
It also resolves `sourceTimeMergeStorage`: a version-pinned, graph-owned
`SourceTimeMergeStorage` for one merger with exactly one declared `IndexBackend`
dependency. Bind that resource to the `drasi/source-time-merge` factory's
`storage` slot. The owner derives its scope from the constructing instance and
graph, retains acquired transactions through failed/cancelled construction, and
awaits cleanup before releasing them. It requires an independent persistent
atomic provider, not a volatile provider or another producer's shared
transaction. See [source-time recipes](../../lib/docs/computation-graph-time-merge.md#factory-owned-construction).

Hosts can register a provider with
`with_versioned_index_provider(name, implementation, configuration_version, provider)`.
An `indexes` recipe then pins the same `implementation` and
`configurationVersion` alongside `provider`. The identity comes from the
embedding application's actual registration; it is never inferred from a Rust
type, database path or provider name. Both identity fields are required together,
and mismatches remain resource-construction failures. The older
`with_index_provider` and unversioned recipes remain supported, but cannot satisfy
a version-pinned recipe.

Use `GraphConfigurationSnapshot::reconstructible_topology()` to extract a
factory-only definition with explicit resource recipes. Captured object
properties are inspection data, not an automatic reconstruction contract, and
resolved secret values are never substituted into exported references. Restoring
configuration selects the same declared resource scope; it does not restore an
earlier processing position.

The `{"kind":"sourceProgress","component":"query","indexes":"query-indexes"}`
recipe creates a checkpoint resource for the named consumer in the current graph.
Declare the query's own index resource (`indexes`) as its only dependency: the
query commits its checkpoints there, so the progress resource declares exactly
the durability that index resource declares, and the query refuses a progress
binding that declares a different one. Bind that same resource ID to the consumer
and its replayable source using `source_progress`. The consumer publishes its
actual recovered state; the recipe cannot supply checkpoints, readiness or
durability claims. Resolution creates a separate owner per instance,
not a global cache keyed by component names. Sources across the native boundary
must explicitly negotiate recovery-v1; native ABI compatibility is not
recovery support. `{"kind":"queryCatalog"}` creates a catalog shared by queries
and a result outlet through their `catalog` dependencies.
Its `configuration` recipe can include local `secrets`, or use an externally
supplied `SecretStoreProvider`. These values are privileged configuration; use
an encrypted `ConfigurationStore` when persisting them. `HostConfigurationResolver`
is shared with Server rather than duplicating its secret/environment resolution.

Persistence is a separate opt-in provider, such as
`drasi-state-store-redb` with its `configuration` feature. Share one provider
across multiple instance IDs, and register the required factories on each
restart. Missing factories remain visible as failed declarations, not silently
removed components. See [managed configuration](../../lib/docs/managed-configuration.md)
for acceptance, reconstruction, snapshots and mutation-boundary contracts.

The ordinary `DrasiLib` facade also accepts native factory additions through
`add_source_from_factory(addition)` and `add_reaction_from_factory(addition)`.
Use `ComponentAddition::from_specification(specification, factory)` to supply
the implementation, or `ComponentAddition::declaration(specification)` followed
by `register_component_factory(factory)` when the implementation arrives later.
Declare output streams and connections explicitly. Sources use the native
`Source` role or an explicitly source-labelled service; reactions must declare
`ComponentSemanticKind::Reaction` on a sink or service descriptor.

The returned handle acknowledges declaration, not readiness. A failed source
constructor can leave its reaction waiting for the input connection; retry the
source before awaiting that reaction's creation. Ordinary `start_source` and
`start_reaction` retry failed unmanaged construction through the same graph.
For managed definitions, use `reconcile_desired_state`. Imperative additions
are not automatically adopted by a configuration store. Existing `add_source`
and `add_reaction` continue to accept legacy trait objects; native recipes do
not introduce legacy-producing factory constructors.

## Native fast-path measurement

`examples/native_fast_path.rs` runs a graph with bounded host input/output and a
separately loaded standard arithmetic transformer, on Tokio current-thread.
No admission, progress, transactions, durable pipes or consumer recovery are
enabled. Each output is checked; 2,048 warmup events precede measurement.

```sh
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3
cargo build --offline -p drasi-host-sdk --example native_fast_path --release
cargo build --offline -p drasi-computation-transformer-arithmetic --features dynamic-plugin --release
DRASI_NATIVE_ARITHMETIC_PLUGIN="$PWD/target/release/libdrasi_computation_transformer_arithmetic.dylib" \
  target/release/examples/native_fast_path 300000 32
```

Use `.so` or `.dll` as appropriate. The System-backed allocation counter measures
**host allocations only**, not the library's separate allocator. CPU, latency,
throughput and process RSS include native calls and plugin work. For interleaved
baseline/current runs with separately built matching plugins:

```sh
python3 lib/tests/measure-fast-path.py BASELINE_HOST CURRENT_HOST OUTPUT.json \
  --baseline-plugin BASELINE_PLUGIN --current-plugin CURRENT_PLUGIN --events 300000
```

Keep the benchmark source, compiler, release profile and shared dependency
versions identical. The report retains binary/plugin hashes and every run; this
small arithmetic workload is not a production capacity benchmark.

The Drasi Host SDK provides the host-side counterpart to the [Drasi Plugin SDK](../plugin-sdk/README.md). While the Plugin SDK helps authors **build** cdylib plugins, the Host SDK helps the server **load, validate, and interact** with them at runtime.

## Overview

When the Drasi Server is built with the `dynamic-plugins` feature, it uses this crate to:

1. **Discover** plugin shared libraries (`.so`/`.dylib`/`.dll`) in a directory
2. **Validate** plugin metadata (SDK version, target triple) before initialization
3. **Initialize** plugins by calling their `drasi_plugin_init()` entry point
4. **Wire callbacks** for log routing and lifecycle event capture
5. **Wrap FFI vtables** in proxy types that implement standard DrasiLib traits (`Source`, `Reaction`, `BootstrapProvider`, `SourcePluginDescriptor`, etc.)

The result is that the rest of the server code works with normal Rust trait objects — the FFI boundary is completely hidden behind the proxies.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  Host (drasi-server)                                    │
│                                                         │
│   PluginLoader ──► LoadedPlugin                         │
│                      ├── SourcePluginProxy              │
│                      ├── ReactionPluginProxy            │
│                      └── BootstrapPluginProxy           │
│                            │                            │
│                    create_source()                       │
│                            │                            │
│                       SourceProxy ─── impl Source        │
│                                                         │
│   StateStoreVtableBuilder ──► StateStoreVtable ─────┐   │
│   IdentityProviderVtableBuilder ──► IdentityVtable ─┤   │
│   CallbackContext ──► log/lifecycle callbacks ───────┤   │
│                                                      │   │
│ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ FFI boundary ─ ─ ─ ─ ─ ─ ─ ─ ─│─ │
│                                                      ▼   │
│   Plugin (.so / .dylib / .dll)                           │
│     FfiStateStoreProxy ── impl StateStoreProvider        │
│     FfiIdentityProviderProxy ── impl IdentityProvider    │
│     FfiTracingLayer ── forwards logs to host              │
└─────────────────────────────────────────────────────────┘
```

### Data flow

- **Host → Plugin**: The host passes `StateStoreVtable`, `IdentityProviderVtable`, and callback function pointers into the plugin via `FfiRuntimeContext`. The plugin wraps these in proxy types from the Plugin SDK.
- **Plugin → Host**: The plugin returns `SourceVtable`, `ReactionVtable`, and `BootstrapProviderVtable` structs. The Host SDK wraps these in proxy types that implement DrasiLib traits.

## Modules

| Module | Description |
|---|---|
| `loader` | `PluginLoader` and `PluginLoaderConfig` — discovers and loads plugins from a directory |
| `callbacks` | `CallbackContext` and `InstanceCallbackContext` — routes plugin logs and lifecycle events into DrasiLib registries |
| `proxies::source` | `SourceProxy` (wraps `SourceVtable` → `impl Source`) and `SourcePluginProxy` (wraps `SourcePluginVtable` → `impl SourcePluginDescriptor`) |
| `proxies::reaction` | `ReactionProxy` (wraps `ReactionVtable` → `impl Reaction`) and `ReactionPluginProxy` (wraps `ReactionPluginVtable` → `impl ReactionPluginDescriptor`) |
| `proxies::bootstrap_provider` | `BootstrapProviderProxy` (wraps `BootstrapProviderVtable` → `impl BootstrapProvider`) and `BootstrapPluginProxy` (wraps `BootstrapPluginVtable` → `impl BootstrapPluginDescriptor`) |
| `proxies::change_receiver` | `ChangeReceiverProxy` and `BootstrapReceiverProxy` — proxy types for data channel receivers passed to plugins |
| `state_store_bridge` | `StateStoreVtableBuilder` — wraps a host `Arc<dyn StateStoreProvider>` into a `StateStoreVtable` for plugin consumption |
| `identity_bridge` | `IdentityProviderVtableBuilder` — wraps a host `Arc<dyn IdentityProvider>` into an `IdentityProviderVtable` for plugin consumption |

## Usage

### Native ComputationGraph plugins

Plugin-side `NativeComponent` interfaces and shared messages no longer depend on
the runtime. This host package is where those operations become graph-owned
components, runtime configuration schemas and actual storage/progress bindings.
The current native ABI is **2.0.0**, with wire version **2**. Rebuild native
libraries against the matching SDK; ABI-1 binaries do not provide the required
lifecycle/update contract and are rejected.

Optional recovery-v1 source progress binds only an actual
`QuerySourceProgressResource` under `source_progress`; it is mutually exclusive
with the source's optional `admission` binding. The proxy retains the real local
owner for recovery assertions while the plugin receives a bounded, revocable
read-only capability. Progress support is an explicit capability, not a
consequence of ABI compatibility. See the [native progress contract](../computation-plugin-sdk/README.md#optional-read-only-source-progress).

Host unit tests also load a separately built, test-only SQLite replay plugin.
`make build-test-plugins` builds it, or use
`CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo build -p drasi-host-sdk --example native_recovery`.
Set `DRASI_NATIVE_RECOVERY_PLUGIN` to select an explicit fixture path.

The independent `computation` module loads native graph factories without changing
the independent ordinary ABI (0.18; older layouts are rejected):

```rust,ignore
use drasi_host_sdk::computation;
use drasi_lib::computation::v1::FactoryRegistry;

let mut factories = FactoryRegistry::standard();
if let Some(plugin) = computation::try_load(path)? {
    plugin.register_factories(&mut factories)?;
    // Serializable plugin/factory/schema/capability discovery.
    let metadata = plugin.metadata();
    // Optional explicit participants for a TransactionalTransformerRegistry:
    let participants = plugin.transactional_factories();
} else {
    // Neither native symbol exists: legacy discovery may continue here.
}
```

`try_load` returns an error, **not** `None`, for partial native entry points, bad
headers, missing metadata, incompatible versions/targets/schemas or unsupported
capabilities. Do not fall back to legacy registration after such an error.
`load` requires this native family. Native libraries are pinned for process
lifetime, including identified candidates whose initialization fails; hot
unloading is unsupported.

Registration also includes supported native query-authoring recipe identities,
so exported query specifications reconstruct through the same factory registry.
It does not choose an ordinary-query default. For ordinary builder/API queries,
explicitly pass `plugin.query_factory()?` to `with_query_factory`, or register
that selection with `register_query_factory`. Missing or ambiguous selection is
an error, never a linked-reference fallback. See the
[query plugin guide](../computation-plugins/transformers/query/README.md).

Native ABI 2.0 uses wire version 2: bounded MessagePack and bulk binary envelope
buffers. Build native libraries and the host from matching sources; unknown
wire versions are rejected before plugin entry. Native response buffers remain
producer-owned while being decoded, then are released through their callback;
graph envelopes contain fully owned local values. JSON storage uses its own
v1 envelope definition. Both representations preserve event-local controls,
annotations and lineage.

`NativeFactory` implements the existing `ComponentFactory` and, for explicitly
opted-in factories, `TransactionalTransformerFactory`. Its `specification(id,
configuration)` helper builds an exact graph recipe with plugin provenance;
`create_component` is configuration-only. Proxies implement the real native
source/transformer/sink/service interfaces. `configuration()` reads the actual
component, not a second mutable registry. The graph publishes configuration at
construction and successful update boundaries rather than invoking getters
during in-flight data. Managed changes require explicit factory planning and
the component's update capability; creation support alone is insufficient.
Core currently rejects every `Rebootstrap` plan. The graph retains submitted
specifications and accepted update intent for pending/failed work.

The graph polls all asynchronous operations and owns cancellation. Native control
operations use separate callbacks rather than waiting behind mutable data calls.
Step-scoped transaction requests are serviced inside the host's actual borrowed
`TransactionContext`, through a revocable mailbox with no commit API. No Rust
future, trait object, task-local, `Arc` or `Bytes` ownership crosses the C ABI.
See the [native SDK](../computation-plugin-sdk/README.md) for authoring,
ownership, optional read/recovery/resource capabilities and their limits.

For a directory containing both families, use
`PluginLoader::load_all_families` and match `LoadedPluginFamily::Legacy` or
`LoadedPluginFamily::Computation`. It opens each candidate once, preserves
reported loading failures, and never falls back after a malformed native
declaration. Verification/allowlisting must happen before this call, as before.
The existing `load_all`/`load_plugin_from_path` interfaces remain legacy-only.

`PluginRegistry::register_computation_plugin` retains native factories under a
family/version-qualified registration identity. The graph factory registry and
optional transactional transformer registry can then be obtained from that
same host registry. Native metadata exposes each factory's role, ports, schemas,
configuration version and actual capabilities.

### Loading plugins

```rust
use std::path::PathBuf;
use drasi_host_sdk::{PluginLoader, PluginLoaderConfig, default_plugin_file_patterns};

let config = PluginLoaderConfig {
    plugin_dir: PathBuf::from("./plugins"),
    // Discovers every plugin type (source, reaction, bootstrap, secret-store,
    // identity, and any future type) via the shared `drasi_` prefix. Supply a
    // narrower list only if you deliberately want to load a subset.
    file_patterns: default_plugin_file_patterns(),
};

let loader = PluginLoader::new(config);
let plugins = loader.load_all(
    log_ctx,            // *mut c_void — host callback context
    log_callback,       // LogCallbackFn
    lifecycle_ctx,      // *mut c_void — host callback context
    lifecycle_callback, // LifecycleCallbackFn
)?;

for plugin in plugins {
    // Each LoadedPlugin contains factory proxies
    for source_factory in plugin.source_plugins {
        // source_factory implements SourcePluginDescriptor
        println!("Loaded source plugin: {}", source_factory.kind());
    }
}
```

### Creating component instances

```rust
// SourcePluginProxy implements SourcePluginDescriptor
let source: Box<dyn Source> = source_factory
    .create_source("my-source-1", &config_json, true)
    .await?;

// The returned SourceProxy implements Source — use it normally
source.start().await?;
let status = source.status().await;
```

### Injecting host services

The host can inject a `StateStoreProvider` and `IdentityProvider` into plugins:

```rust
use drasi_host_sdk::{StateStoreVtableBuilder, IdentityProviderVtableBuilder};

// Build FFI vtables from host-side trait objects
let state_store_vtable = StateStoreVtableBuilder::build(my_state_store.clone());
let identity_vtable = IdentityProviderVtableBuilder::build(my_identity_provider.clone());

// These vtables are passed to plugins via FfiRuntimeContext during initialization.
// The plugin wraps them in FfiStateStoreProxy / FfiIdentityProviderProxy.
```

### Callback wiring

```rust
use drasi_host_sdk::CallbackContext;

let ctx = Arc::new(CallbackContext {
    instance_id: "my-instance".to_string(),
    runtime_handle: tokio::runtime::Handle::current(),
    log_registry: log_registry.clone(),
    source_event_history: source_events.clone(),
    reaction_event_history: reaction_events.clone(),
});

// Pass as raw pointer to plugin loader
let raw_ctx = CallbackContext::into_raw(ctx);
```

Plugins route all `log` and `tracing` events through the FFI log callback. The `CallbackContext` dispatches these into the correct DrasiLib `ComponentLogRegistry` keyed by instance and component ID.

## OCI Registry and Signature Verification

The `OciRegistryClient` supports downloading plugins from OCI registries and optional cosign signature verification via `CosignVerifier`:

- Use `OciRegistryClient::with_verifier(config, verifier)` to enable signature verification on download
- `VerificationConfig` allows configuring trusted identities (issuer + subject pattern)
- `download_plugin()` returns `DownloadResult` containing the plugin path and an optional `VerificationResult`
- `PluginMetadata` now includes `git_commit` and `build_timestamp` fields for build provenance

### Registry compatibility

Every published artifact declares its ABI explicitly; compatibility is never
inferred from a package version, plugin type or missing field.

| OCI annotation | Meaning |
|---|---|
| `io.drasi.plugin.abi-family` | `ordinary` (Source/Reaction, `drasi-plugin-sdk`) or `computation` (native) |
| `io.drasi.plugin.abi-version` | That family's ABI, read from the binary's marker at build time |
| `io.drasi.plugin.sdk-version` / `core-version` / `lib-version` | Cargo package versions |

`PluginResolver` rejects an artifact before download when its family or ABI is
missing, unknown or incompatible, using the same rule as the binary loader:
ordinary artifacts need the host's FFI ABI major.minor plus matching SDK, core
and lib package major.minor; native artifacts need the host's native ABI major
with an older or equal minor and the host target. Explicitly pinned references
are checked the same way, and the resolved digest is the one whose annotations
were checked. `download_plugin` then requires the manifest annotation, metadata
layer and the binary's own marker (`drasi-plugin-ffi-abi=` or
`drasi-computation-plugin-abi=`) to agree exactly, rejects a binary carrying the
other family's marker, and re-checks host compatibility before writing anything.
`HostVersionInfo::abi_version()` and `computation_abi_version()` report the
loaders' ABIs. Artifacts without these declarations must be rebuilt and
republished with `cargo xtask build-plugins` / `publish-plugins`, which read the
marker from each binary (including cross-compiled targets) and refuse, before
using credentials or uploading, any metadata that disagrees with it.

## Plugin Load Sequence

1. **Discovery** — scans the plugin directory for files matching configured glob patterns (the `DEFAULT_PLUGIN_FILE_PATTERNS` — `libdrasi_*` / `drasi_*` — match every plugin type), groups them by base name (stripping all known extensions: `.dylib`, `.so`, `.dll`, `.rlib`, `.rmeta`, `.d`)
2. **Extension filtering** — for each plugin group, selects only cdylib files (`.dylib`, `.so`, `.dll`). Non-cdylib Cargo artifacts (`.rlib`, `.d`, `.rmeta`) are silently ignored. If multiple cdylib extensions exist for the same plugin, an error is logged and the plugin is skipped (ambiguous)
3. **Library open** — `libloading::Library::new(path)` loads the `.so`/`.dylib`/`.dll`
4. **Metadata validation** — resolves `drasi_plugin_metadata()` symbol, checks the ordinary FFI ABI version (major.minor match, independent of the SDK package version) and target triple
5. **Initialization** — calls `drasi_plugin_init()` which returns an `FfiPluginRegistration` containing vtable arrays and callback setters
6. **Callback wiring** — calls `set_log_callback` and `set_lifecycle_callback` with host context pointers
7. **Proxy extraction** — wraps each `SourcePluginVtable`, `ReactionPluginVtable`, and `BootstrapPluginVtable` in their corresponding proxy types
8. **Library retention** — the `Arc<Library>` is stored in each proxy to keep the shared library loaded for as long as any proxy is alive

### Package and configuration versions

`LoadedPlugin::plugin_version()` reads the package version from the retained
library's metadata export. Runtime registration preserves it per descriptor via
the registry's `register_*_with_package_version` methods. This is separate from
`config_version()`, which describes the accepted configuration format.

Existing registration methods remain supported and leave the package version
unknown. Replacing a descriptor also replaces its version metadata; registering
another kind from the same plugin ID does not rewrite earlier registrations.
Hosts must not substitute a configuration version when the package version is
unknown. No plugin vtable or wire-layout change is required.

### Plugin File Naming

Plugin shared libraries must follow the naming convention `lib<plugin_name>.<ext>` (on Unix) or `<plugin_name>.dll` (on Windows). The loader uses glob patterns to match plugin files:

- **Only cdylib extensions are loaded**: `.dylib` (macOS), `.so` (Linux), `.dll` (Windows)
- **Non-cdylib artifacts are ignored**: `.rlib`, `.rmeta`, `.d` files produced by Cargo alongside the cdylib are silently skipped
- **One cdylib per plugin**: If both `.dylib` and `.so` exist for the same plugin base name, the loader reports an ambiguity error and skips the plugin

## Integration Tests

The host-sdk includes integration tests that load real cdylib plugins and exercise the full pipeline:

```sh
# Prerequisites: build the test plugins as cdylib shared libraries
make build-dynamic-plugins

# Run the integration tests
cargo test -p drasi-host-sdk --test integration_test
```

The tests cover:
- Plugin discovery and loading
- Metadata validation (SDK version, target triple)
- Source/Reaction/Bootstrap factory invocation
- Trait method dispatch through FFI (start, stop, status, subscribe, etc.)
- Log and lifecycle callback routing
- State store and identity provider injection
- Error handling and panic safety

## License

Licensed under the [Apache License, Version 2.0](http://www.apache.org/licenses/LICENSE-2.0).
