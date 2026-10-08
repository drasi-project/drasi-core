# Drasi Host SDK

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
index-provider resource. Optional `retry` uses `DeliveryRetryPolicy`; omitted
retry means its existing default, not unlimited attempts. The recipe reuses the
declared provider while isolating delivery storage by instance and graph. It
cannot substitute a producer's shared transaction group for the consumer's
own per-operation transaction. Component specifications bind this resource
under `consumer`. Actual constructed storage must meet the requested guarantee.

## Managed native bootstrap

`nativeBootstrap` recipes create one graph-owned `Bootstrap` resource for the
named query `component`. Supply the negotiated `implementation`,
`configurationVersion` and an unresolved `configuration` map using the same
`ConfigurationValue` format as component specifications. `PluginRegistry` exposes
`computation_bootstrap_factories()` and `computation_bootstrap_metadata()` from
the already loaded libraries; no provider is cached in a second runtime registry.

If the factory requires progress, `sourceProgress` names the actual checkpoint
resource also bound to the query's `source_progress` slot. Declare that resource
as a `Checkpoint` dependency of the bootstrap resource. Configuration references
likewise declare their `SecretStore` dependencies. Missing, extra, wrongly typed
or foreign-owner bindings fail; secret fields cannot be persisted as literals.
The query binds the bootstrap resource under `bootstrap`. One recipe/provider
cannot be shared between different queries or instances. Snapshot generations
are owned by bootstrap-v1, not invented component generations for resources.

Query stop joins bootstrap work before releasing query storage. Graph resource
retirement also stops the provider and revokes its progress binding; a retained
old handle cannot restart it. Recipes preserve references through persistence
and reconstruction. These contracts are qualified with test-only native
libraries; production bootstrap/consumer plugin migrations remain separate.

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
The `{"kind":"sourceProgress","component":"query"}` recipe creates a checkpoint
resource for the named consumer in the current graph. Bind that same resource ID
to the consumer and its replayable source using `source_progress`. The consumer
publishes its actual recovered state; the recipe cannot supply checkpoints,
readiness or durability claims. Resolution creates a separate owner per instance,
not a global cache keyed by component names. Sources across the native boundary
must explicitly negotiate recovery-v1; ordinary ABI 1.0 compatibility is not
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

## Native fast-path measurement

`examples/native_fast_path.rs` runs a graph with bounded host input/output and a
separately loaded standard arithmetic transformer, on Tokio current-thread.
No admission, progress, transactions, durable pipes or consumer recovery are
enabled. Each output is checked; 2,048 warmup events precede measurement.

```sh
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3
cargo build --offline -p drasi-host-sdk --example native_fast_path --release
cargo build --offline -p drasi-computation-standard --features dynamic-plugin --release
DRASI_NATIVE_STANDARD_PLUGIN="$PWD/target/release/libdrasi_computation_standard.dylib" \
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

Optional recovery-v1 source progress binds only an actual
`QuerySourceProgressResource` under `source_progress`; it is mutually exclusive
with the source's optional `admission` binding. The proxy retains the real local
owner for recovery assertions while the plugin receives a bounded, revocable
read-only capability. Old native binaries remain available for their existing
fast behavior. See the [native progress contract](../computation-plugin-sdk/README.md#optional-read-only-source-progress).

Host unit tests also load a separately built, test-only SQLite replay plugin.
`make build-test-plugins` builds it, or use
`CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo build -p drasi-host-sdk --example native_recovery`.
Set `DRASI_NATIVE_RECOVERY_PLUGIN` to select an explicit fixture path.

The independent `computation` module loads native graph factories without changing
the legacy ABI 0.15:

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

Native ABI 1.0 uses wire version 2: bounded MessagePack and bulk binary envelope
buffers. Version-1 prototype native libraries must be rebuilt and are rejected
before plugin entry. Native response buffers remain producer-owned while being
decoded, then are released through their callback; graph envelopes contain
fully owned local values. Legacy ABI 0.15 and persisted JSON envelopes are unchanged.

`NativeFactory` implements the existing `ComponentFactory` and, for explicitly
opted-in factories, `TransactionalTransformerFactory`. Its `specification(id,
configuration)` helper builds an exact graph recipe with plugin provenance;
`create_component` is configuration-only. Proxies implement the real native
source/transformer/sink/service interfaces. `configuration()` reads the actual
component, not a second mutable registry. The graph publishes configuration at
construction boundaries rather than invoking getters during in-flight data;
native ABI 1.0 does not support in-place reconfiguration. The graph retains
submitted specifications for pending/failed construction.

The graph polls all asynchronous operations and owns cancellation. Native control
operations use separate callbacks rather than waiting behind mutable data calls.
Step-scoped transaction requests are serviced inside the host's actual borrowed
`TransactionContext`, through a revocable mailbox with no commit API. No Rust
future, trait object, task-local, `Arc` or `Bytes` ownership crosses the C ABI.
See the [native SDK](../computation-plugin-sdk/README.md) for authoring,
ownership and explicitly unsupported query/recovery/resource capabilities.

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

## Plugin Load Sequence

1. **Discovery** — scans the plugin directory for files matching configured glob patterns (the `DEFAULT_PLUGIN_FILE_PATTERNS` — `libdrasi_*` / `drasi_*` — match every plugin type), groups them by base name (stripping all known extensions: `.dylib`, `.so`, `.dll`, `.rlib`, `.rmeta`, `.d`)
2. **Extension filtering** — for each plugin group, selects only cdylib files (`.dylib`, `.so`, `.dll`). Non-cdylib Cargo artifacts (`.rlib`, `.d`, `.rmeta`) are silently ignored. If multiple cdylib extensions exist for the same plugin, an error is logged and the plugin is skipped (ambiguous)
3. **Library open** — `libloading::Library::new(path)` loads the `.so`/`.dylib`/`.dll`
4. **Metadata validation** — resolves `drasi_plugin_metadata()` symbol, checks SDK version (major.minor match) and target triple
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
