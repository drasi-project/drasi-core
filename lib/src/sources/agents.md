# agents.md — drasi-core/lib/src/sources

## FFI Boundary Warning

The `Source` trait defined in this directory is the primary interface that source plugins
implement. In the dynamic plugin system, `Source` methods are dispatched through an
`#[repr(C)]` vtable (`SourceVtable`) across the FFI boundary.

### What crosses FFI

| Trait method | FFI vtable field | Notes |
|-------------|-----------------|-------|
| `id()` | `id_fn` | Returns `FfiStr` (borrowed) |
| `type_name()` | `type_name_fn` | Returns `FfiStr` (borrowed) |
| `dispatch_mode()` | `dispatch_mode_fn` | Returns `FfiDispatchMode` enum |
| `auto_start()` | `auto_start_fn` | Returns `bool` |
| `start()` | `start_fn` | Returns `FfiResult` |
| `stop()` | `stop_fn` | Returns `FfiResult` |
| `status()` | `status_fn` | Returns `FfiComponentStatus` |
| `subscribe()` | `subscribe_fn` | Returns `*mut FfiSubscriptionResponse` |
| `initialize()` | `initialize_fn` | Takes `*const FfiRuntimeContext` |
| `set_bootstrap_provider()` | `set_bootstrap_provider_fn` | Takes `*mut BootstrapProviderVtable` |
| `deprovision()` | `deprovision_fn` | Returns `FfiResult` |

### What to update when changing the `Source` trait

If you add, remove, or change methods on the `Source` trait:

1. **FFI vtable struct** — `components/plugin-sdk/src/ffi/vtables.rs` → `SourceVtable`
   - Add/remove the corresponding function pointer field

2. **Vtable generation (plugin side)** — `components/plugin-sdk/src/ffi/vtable_gen.rs`
   - Update `build_source_vtable()` to generate the new function pointer
   - If the method has async, wrap with `block_on` pattern (see existing methods)

3. **Host proxy** — `components/host-sdk/src/proxies/source.rs` → `SourceProxy`
   - Implement the new trait method, dispatching through the vtable

4. **Version bump** — `components/plugin-sdk/src/ffi/metadata.rs` → `FFI_SDK_VERSION`
   - Adding a method to the vtable changes its layout → major version bump

### What to update when changing `SourcePluginDescriptor`

The factory trait `SourcePluginDescriptor` (in `components/plugin-sdk/src/descriptor.rs`)
is also wrapped in an FFI vtable:

1. **FFI vtable** — `components/plugin-sdk/src/ffi/vtables.rs` → `SourcePluginVtable`
2. **Vtable generation** — `components/plugin-sdk/src/ffi/vtable_gen.rs` →
   `build_source_plugin_vtable()`
3. **Host proxy** — `components/host-sdk/src/proxies/source.rs` → `SourcePluginProxy`
4. **OpenAPI schema** — If `config_schema_json()` output format changes, update
   schema merging in `drasi-server/src/api/`
