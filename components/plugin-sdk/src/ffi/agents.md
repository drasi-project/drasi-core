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
| Rich event payloads (SourceEventWrapper, BootstrapEvent, QueryResult) | **Serialized MessagePack bytes** (`ptr+len+drop_fn`) in `#[repr(C)]` envelopes — never `repr(Rust)` opaque pointers (issue #602) | `payload.rs` (DTOs + codec), `vtables.rs` (envelopes), `vtable_gen.rs` |
| Simple enums (ComponentStatus, DispatchMode) | Mirrored `#[repr(C)]` enums | `types.rs` |
| Strings | `FfiStr` (borrowed) / `FfiOwnedStr` (owned) | `types.rs` |
| Complex structs (SubscriptionSettings) | Deconstructed into `FfiStr` args or JSON | `vtable_gen.rs` |

### When modifying vtables

**Adding a method to a trait vtable is a breaking change** — it changes the layout of the
`#[repr(C)]` struct. You must:

1. Add the field to the vtable struct in `vtables.rs`
2. Implement the vtable function in `vtable_gen.rs`
3. Update the host-side proxy in `components/host-sdk/src/proxies/`
4. **Bump `FFI_SDK_VERSION`** in `metadata.rs` so the major.minor pair changes
   (see "Version compatibility" below)

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
The host (`components/host-sdk/src/loader.rs`) validates that the **major.minor**
versions match exactly; any difference rejects the plugin with a descriptive
error before any unsafe FFI call.

**Always bump the version when changing any `#[repr(C)]` type layout.** Because
the loader compares the full `major.minor` pair, any ABI-breaking change must
change at least the minor version:

- **Pre-1.0 (current):** a minor bump (e.g. `0.8.x` → `0.9.0`) is sufficient and
  is the convention for ABI-breaking changes while the SDK is unstable. The
  patch component is reserved for ABI-compatible changes.
- **Post-1.0:** ABI-breaking changes must bump the **major** version, matching
  SemVer expectations once the SDK is declared stable.
