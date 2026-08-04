# AGENTS.md: drasi-core/components/plugin-sdk/src/ffi

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

Domain types from `drasi-core/core/src/models/` and `drasi-core/lib/src/` cross the FFI boundary through this directory. The general patterns are:

| Domain type category | FFI pattern | Key file |
|---------------------|-------------|----------|
| Trait methods (Source, Reaction, BootstrapProvider) | Function pointer vtables | `vtables.rs`, `vtable_gen.rs` |
| Rich event payloads (SourceEventWrapper, BootstrapEvent, QueryResult) | **Serialized MessagePack bytes** (`ptr+len+drop_fn`) in `#[repr(C)]` envelopes — never `repr(Rust)` opaque pointers (issue #602) | `payload.rs` (DTOs + codec), `vtables.rs` (envelopes), `vtable_gen.rs` |
| Simple enums (ComponentStatus, DispatchMode) | Mirrored `#[repr(C)]` enums | `types.rs` |
| Strings | `FfiStr` (borrowed) / `FfiOwnedStr` (owned) | `types.rs` |
| Complex structs (SubscriptionSettings) | Deconstructed into `FfiStr` args or JSON | `vtable_gen.rs` |

### When modifying vtables

**Adding a method to a trait vtable is a breaking change** — it changes the layout of the `#[repr(C)]` struct. You must:

1. Add the field to the vtable struct in `vtables.rs`
2. Implement the vtable function in `vtable_gen.rs`
3. Update the host-side proxy in `components/host-sdk/src/proxies/`
4. **Bump `FFI_SDK_VERSION`** in `metadata.rs` so the major.minor pair changes (see "Version compatibility" below)

### When modifying FFI types

If you change `FfiStr`, `FfiResult`, `FfiComponentStatus`, `FfiDispatchMode`, or any `#[repr(C)]` type:

1. Update the type definition in `types.rs`
2. Update all vtable functions that use it in `vtable_gen.rs`
3. Update all host-side proxies that consume it in `components/host-sdk/src/proxies/`
4. Bump `FFI_SDK_VERSION` in `metadata.rs`

### When adding a new event payload type

Do **not** transfer a `repr(Rust)` payload by boxing it and reconstructing it on the other side with `Box::from_raw`. That was the previous design and it is undefined behaviour: `repr(Rust)` has no stable layout across independently compiled cdylibs, and types like `bytes::Bytes` carry a `&'static` vtable pointer valid only in the producing module. It caused non-deterministic heap corruption. See the module docs in `payload.rs` and the `0.10.0` entry in `metadata.rs`.

To add or change an event payload, extend `payload.rs` and decode only through its hardened entry points; a parallel codec silently loses the payload size cap and the null `drop_fn` guard. Its module docs are the contract for the wire pattern. Any wire-format change bumps `FFI_SDK_VERSION` in `metadata.rs`.

### Version compatibility

Bump semantics live on the `FFI_SDK_VERSION` constant in `metadata.rs`; read them there. Two things the edit sites do not teach: the load-time check is skipped (with only a warning) for a plugin that does not export `drasi_plugin_metadata`, and fields appended to the registration struct must be gated on the reported version, per the `MIN_SDK_VERSION_WITH_*` gates in `loader.rs`, or reads walk off the end of old-layout allocations.
