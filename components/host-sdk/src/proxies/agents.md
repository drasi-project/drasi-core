# agents.md — drasi-core/components/host-sdk/src/proxies

## This directory implements the host side of the FFI boundary

These proxy types wrap `#[repr(C)]` vtable structs into real DrasiLib trait
implementations (`Source`, `Reaction`, `BootstrapProvider`, etc.). They are the
**mirror image** of the vtable generation in `components/plugin-sdk/src/ffi/vtable_gen.rs`.

### Proxy types and their counterparts

| Proxy (this dir) | Implements trait | Wraps vtable | Plugin-side generation |
|-----------------|-----------------|-------------|----------------------|
| `SourceProxy` | `Source` | `SourceVtable` | `vtable_gen.rs: build_source_vtable()` |
| `SourcePluginProxy` | `SourcePluginDescriptor` | `SourcePluginVtable` | `vtable_gen.rs: build_source_plugin_vtable()` |
| `ReactionProxy` | `Reaction` | `ReactionVtable` | `vtable_gen.rs: build_reaction_vtable()` |
| `ReactionPluginProxy` | `ReactionPluginDescriptor` | `ReactionPluginVtable` | `vtable_gen.rs: build_reaction_plugin_vtable()` |
| `BootstrapProviderProxy` | `BootstrapProvider` | `BootstrapProviderVtable` | `vtable_gen.rs: build_bootstrap_provider_vtable()` |
| `BootstrapPluginProxy` | `BootstrapPluginDescriptor` | `BootstrapPluginVtable` | `vtable_gen.rs: build_bootstrap_plugin_vtable()` |
| `ChangeReceiverProxy` | `ChangeReceiver<SourceEventWrapper>` | `FfiChangeReceiver` | `vtable_gen.rs` (inline) |
| `BootstrapReceiverProxy` | (custom) | `FfiBootstrapReceiver` | `vtable_gen.rs` (inline) |

### When modifying these proxies

If a trait method is added/changed in `drasi-lib`, you need to update **both sides**:

1. **Plugin-side** — `components/plugin-sdk/src/ffi/vtables.rs` (vtable struct) and
   `components/plugin-sdk/src/ffi/vtable_gen.rs` (vtable function implementation)

2. **Host-side** — The corresponding proxy file in this directory (implement the new
   trait method by dispatching through the vtable)

### Opaque pointer reconstruction

Several proxies reconstruct Rust types from opaque `*mut c_void` pointers:

- `change_receiver.rs:54` — `Box::from_raw(opaque as *mut SourceEventWrapper)`
- `change_receiver.rs:92` — `Box::from_raw(opaque as *mut BootstrapEvent)`
- `bootstrap_provider.rs:140` — `Box::from_raw(opaque as *mut BootstrapEvent)`

`ReactionProxy` transfers ownership **to** the plugin via opaque pointers:

- `reaction.rs` — `Box::into_raw(Box::new(query_result)) as *mut c_void` passed to
  `enqueue_query_result_fn`. The plugin takes ownership via `Box::from_raw`.

**Safety**: These casts are only valid if the plugin and host agree on the type's memory
layout. This is enforced by SDK version validation in `loader.rs`.

If you change the definition of `SourceEventWrapper`, `BootstrapEvent`, or any other
opaque-pointer type (in `drasi-core/core/src/models/` or `drasi-core/lib/src/channels/`),
you **must** bump `FFI_SDK_VERSION` in `components/plugin-sdk/src/ffi/metadata.rs`.

### `Arc<Library>` lifetime management

Every proxy holds an `Arc<Library>` (or `Option<Arc<Library>>` for bootstrap) to keep
the cdylib loaded. When all proxies from a plugin are dropped, the library is unloaded.
Do not remove these fields — they prevent use-after-unload crashes.
