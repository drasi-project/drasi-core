# agents.md — drasi-core/components/plugin-sdk/src/ffi

## This directory implements the plugin side of the FFI boundary

This is the **most critical directory** in the dynamic plugin system. It defines:

- `#[repr(C)]` vtable structs (`vtables.rs`) — the stable C ABI between host and plugins
- `#[repr(C)]` data types (`types.rs`) — FFI-safe wrappers for Rust types
- Vtable construction functions (`vtable_gen.rs`) — builds vtables from trait impls
- Plugin metadata (`metadata.rs`) — version/target info for compatibility validation
- Callback types (`callbacks.rs`) — log and lifecycle bridges
- State store proxy (`state_store_proxy.rs`) — plugin-side wrapper for host state store
- Bootstrap proxy (`bootstrap_proxy.rs`) — plugin-side wrapper for host bootstrap provider

### Relationship to domain types

Domain types from `drasi-core/core/src/models/` and `drasi-core/lib/src/` cross the
FFI boundary through this directory. The general patterns are:

| Domain type category | FFI pattern | Key file |
|---------------------|-------------|----------|
| Trait methods (Source, Reaction, BootstrapProvider) | Function pointer vtables | `vtables.rs`, `vtable_gen.rs` |
| Opaque Rust types (SourceEventWrapper, BootstrapEvent, QueryResult) | `*mut c_void` in `#[repr(C)]` envelopes or vtable args | `vtables.rs` (envelope defs), `vtable_gen.rs` (wrapping) |
| Simple enums (ComponentStatus, DispatchMode) | Mirrored `#[repr(C)]` enums | `types.rs` |
| Strings | `FfiStr` (borrowed) / `FfiOwnedStr` (owned) | `types.rs` |
| Complex structs (SubscriptionSettings) | Deconstructed into `FfiStr` args or JSON | `vtable_gen.rs` |

### When modifying vtables

**Adding a method to a trait vtable is a breaking change** — it changes the layout of the
`#[repr(C)]` struct. You must:

1. Add the field to the vtable struct in `vtables.rs`
2. Implement the vtable function in `vtable_gen.rs`
3. Update the host-side proxy in `components/host-sdk/src/proxies/`
4. **Bump `FFI_SDK_VERSION` major version** in `metadata.rs`

### When modifying FFI types

If you change `FfiStr`, `FfiResult`, `FfiComponentStatus`, `FfiDispatchMode`, or any
`#[repr(C)]` type:

1. Update the type definition in `types.rs`
2. Update all vtable functions that use it in `vtable_gen.rs`
3. Update all host-side proxies that consume it in `components/host-sdk/src/proxies/`
4. Bump `FFI_SDK_VERSION` in `metadata.rs`

### When adding new opaque pointer types

If a new domain type needs to cross FFI as an opaque pointer:

1. Define an `#[repr(C)]` envelope struct in `vtables.rs` (like `FfiSourceEvent`)
2. Add wrapping logic in `vtable_gen.rs` (box the Rust type, store as `*mut c_void`)
3. Add unwrapping logic in `components/host-sdk/src/proxies/` (cast back with `Box::from_raw`)
4. Add a `drop_fn` field to the envelope for safe cleanup
5. Bump `FFI_SDK_VERSION` in `metadata.rs`

### Version compatibility

`metadata.rs` defines `FFI_SDK_VERSION` which is checked at plugin load time.
The host (`components/host-sdk/src/loader.rs`) validates that major.minor versions match.
**Always bump the version when changing any `#[repr(C)]` type layout.**
