# Drasi Host SDK

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

### Loading plugins

```rust
use drasi_host_sdk::{PluginLoader, PluginLoaderConfig};

let config = PluginLoaderConfig {
    plugin_dir: PathBuf::from("./plugins"),
    file_patterns: vec![
        "libdrasi_source_*".to_string(),
        "libdrasi_reaction_*".to_string(),
        "libdrasi_bootstrap_*".to_string(),
    ],
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
    log_registry: log_registry.clone(),
    source_event_history: source_events.clone(),
    reaction_event_history: reaction_events.clone(),
});

// Pass as raw pointer to plugin loader
let raw_ctx = CallbackContext::into_raw(ctx);
```

Plugins route all `log` and `tracing` events through the FFI log callback. The `CallbackContext` dispatches these into the correct DrasiLib `ComponentLogRegistry` keyed by instance and component ID.

## Plugin Load Sequence

1. **Library open** — `libloading::Library::new(path)` loads the `.so`/`.dylib`/`.dll`
2. **Metadata validation** — resolves `drasi_plugin_metadata()` symbol, checks SDK version (major.minor match) and target triple
3. **Initialization** — calls `drasi_plugin_init()` which returns an `FfiPluginRegistration` containing vtable arrays and callback setters
4. **Callback wiring** — calls `set_log_callback` and `set_lifecycle_callback` with host context pointers
5. **Proxy extraction** — wraps each `SourcePluginVtable`, `ReactionPluginVtable`, and `BootstrapPluginVtable` in their corresponding proxy types
6. **Library retention** — the `Arc<Library>` is stored in each proxy to keep the shared library loaded for as long as any proxy is alive

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
