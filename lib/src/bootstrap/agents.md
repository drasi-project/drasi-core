# agents.md — drasi-core/lib/src/bootstrap

## FFI Boundary Warning

The `BootstrapProvider` trait and supporting types in this directory cross the dynamic
plugin FFI boundary. Bootstrap providers are unique because they involve **cross-plugin
communication**: a bootstrap plugin (Plugin A) provides data to a source plugin (Plugin B)
mediated by the host.

### Types that cross FFI

| Type | How it crosses | FFI wrapper |
|------|---------------|-------------|
| `BootstrapProvider` trait | Wrapped as `BootstrapProviderVtable` (plugin→host) AND reverse-wrapped as vtable (host→plugin) | Both directions |
| `BootstrapRequest` | Deconstructed into individual FFI args (query_id, node_labels, etc.) | Multiple `FfiStr` + `*const FfiStr` arrays |
| `BootstrapContext` | Deconstructed into individual FFI args (server_id, source_id) | Multiple `FfiStr` args |
| `BootstrapEvent` | Opaque pointer (`Box<BootstrapEvent>` → `*mut c_void`) | `FfiBootstrapEvent.opaque` |

### Cross-plugin bootstrap flow

```
Source Plugin B calls set_bootstrap_provider(provider)
  → Host wraps host-side BootstrapProvider into BootstrapProviderVtable
  → Plugin B stores vtable, calls vtable.bootstrap_fn() when subscribing
  → Host receives call, dispatches to Bootstrap Plugin A
  → Plugin A sends BootstrapEvents via FfiBootstrapSender
  → Host forwards events back to Plugin B via channel
```

### What to update when changing `BootstrapProvider`

1. **FFI vtable** — `components/plugin-sdk/src/ffi/vtables.rs` → `BootstrapProviderVtable`

2. **Plugin-side vtable generation** — `components/plugin-sdk/src/ffi/vtable_gen.rs`
   - `build_bootstrap_provider_vtable()` — host→plugin direction
   - The function that wraps a `BootstrapProvider` impl into vtable fn pointers

3. **Plugin-side proxy** — `components/plugin-sdk/src/ffi/bootstrap_proxy.rs`
   - `FfiBootstrapProviderProxy` — plugin-side wrapper that calls vtable to reach host

4. **Host-side proxy** — `components/host-sdk/src/proxies/bootstrap_provider.rs`
   - `BootstrapProviderProxy` — host-side wrapper for plugin-provided bootstrap providers
   - `build_ffi_bootstrap_sender()` — creates `FfiBootstrapSender` that bridges
     `std::sync::mpsc` → `tokio::sync::mpsc`

5. **Version bump** — `components/plugin-sdk/src/ffi/metadata.rs` → `FFI_SDK_VERSION`

### What to update when changing `BootstrapRequest` or `BootstrapContext`

These types are NOT passed as opaque pointers — they are deconstructed into individual
`FfiStr` arguments. If you add fields:

1. **Vtable function signature** — `components/plugin-sdk/src/ffi/vtables.rs` →
   `BootstrapProviderVtable.bootstrap_fn` — add the new parameter

2. **Plugin-side vtable gen** — `components/plugin-sdk/src/ffi/vtable_gen.rs` →
   the `bootstrap_fn` implementation that deconstructs `BootstrapRequest`

3. **Plugin-side proxy** — `components/plugin-sdk/src/ffi/bootstrap_proxy.rs` →
   `FfiBootstrapProviderProxy.bootstrap()` that calls the vtable with new args

4. **Host-side proxy** — `components/host-sdk/src/proxies/bootstrap_provider.rs` →
   `BootstrapProviderProxy.bootstrap()` that constructs `FfiStr` args

### What to update when changing `BootstrapPluginDescriptor`

1. **FFI vtable** — `components/plugin-sdk/src/ffi/vtables.rs` → `BootstrapPluginVtable`
2. **Vtable generation** — `components/plugin-sdk/src/ffi/vtable_gen.rs`
3. **Host proxy** — `components/host-sdk/src/proxies/bootstrap_provider.rs` → `BootstrapPluginProxy`
