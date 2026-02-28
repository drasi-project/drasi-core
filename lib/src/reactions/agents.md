# agents.md — drasi-core/lib/src/reactions

## FFI Boundary Warning

The `Reaction` trait defined in this directory is the primary interface that reaction
plugins implement. In the dynamic plugin system, `Reaction` methods are dispatched
through an `#[repr(C)]` vtable (`ReactionVtable`) across the FFI boundary.

### What crosses FFI

| Trait method | FFI vtable field | Notes |
|-------------|-----------------|-------|
| `id()` | `id_fn` | Returns `FfiStr` (borrowed) |
| `type_name()` | `type_name_fn` | Returns `FfiStr` (borrowed) |
| `query_ids()` | `query_ids_fn` | Returns `FfiStringArray` (owned) |
| `auto_start()` | `auto_start_fn` | Returns `bool` |
| `start()` | `start_fn` | Returns `FfiResult` |
| `stop()` | `stop_fn` | Returns `FfiResult` |
| `status()` | `status_fn` | Returns `FfiComponentStatus` |
| `initialize()` | `initialize_fn` | Takes `*const FfiRuntimeContext` |
| `deprovision()` | `deprovision_fn` | Returns `FfiResult` |
| `enqueue_query_result()` | `enqueue_query_result_fn` | Takes `*mut c_void` (opaque `Box<QueryResult>`), returns `FfiResult`. Host transfers ownership. |

### What to update when changing the `Reaction` trait

If you add, remove, or change methods on the `Reaction` trait:

1. **FFI vtable struct** — `components/plugin-sdk/src/ffi/vtables.rs` → `ReactionVtable`
   - Add/remove the corresponding function pointer field

2. **Vtable generation (plugin side)** — `components/plugin-sdk/src/ffi/vtable_gen.rs`
   - Update `build_reaction_vtable()` to generate the new function pointer

3. **Host proxy** — `components/host-sdk/src/proxies/reaction.rs` → `ReactionProxy`
   - Implement the new trait method, dispatching through the vtable

4. **Version bump** — `components/plugin-sdk/src/ffi/metadata.rs` → `FFI_SDK_VERSION`
   - Adding a method to the vtable changes its layout → major version bump

### Host-managed query subscriptions

Reactions do **not** subscribe to queries themselves. The host (`ReactionManager`) manages
query subscriptions and forwards `QueryResult` values into the reaction via
`enqueue_query_result()`. This is true for both static and dynamic plugins.

- `QueryResult` crosses FFI as an **opaque pointer** (`Box::into_raw` / `Box::from_raw`)
- Ownership transfers from host to plugin — no serialization, no copying
- The plugin-side vtable function (`enqueue_query_result_fn` in `vtable_gen.rs`) reconstructs
  the `QueryResult` via `Box::from_raw` and calls `reaction.enqueue_query_result()`
- The host-side proxy (`ReactionProxy` in `host-sdk/src/proxies/reaction.rs`) boxes the
  `QueryResult` and passes the raw pointer

If you change the `QueryResult` struct layout (in `lib/src/channels/events.rs`), you must
bump `FFI_SDK_VERSION` since both sides cast the same `*mut c_void`.

There is no `QueryProvider` trait — it was removed. Do not re-add it.

### What to update when changing `ReactionPluginDescriptor`

The factory trait `ReactionPluginDescriptor` (in `components/plugin-sdk/src/descriptor.rs`)
is also wrapped in an FFI vtable:

1. **FFI vtable** — `components/plugin-sdk/src/ffi/vtables.rs` → `ReactionPluginVtable`
2. **Vtable generation** — `components/plugin-sdk/src/ffi/vtable_gen.rs` →
   `build_reaction_plugin_vtable()`
3. **Host proxy** — `components/host-sdk/src/proxies/reaction.rs` → `ReactionPluginProxy`
4. **OpenAPI schema** — If `config_schema_json()` output format changes, update
   schema merging in `drasi-server/src/api/`
