# agents.md — drasi-core/lib/src/channels

## FFI Boundary Warning

Several types in this directory cross the dynamic plugin FFI boundary. They are passed
as opaque pointers inside `#[repr(C)]` structs or have their fields extracted into
FFI-safe representations.

### Types that cross FFI

| Type | File | How it crosses | FFI wrapper |
|------|------|---------------|-------------|
| `SourceEventWrapper` | `events.rs` | Opaque pointer (`Box<SourceEventWrapper>` → `*mut c_void`) | `FfiSourceEvent.opaque` |
| `BootstrapEvent` | `events.rs` | Opaque pointer (`Box<BootstrapEvent>` → `*mut c_void`) | `FfiBootstrapEvent.opaque` |
| `SubscriptionResponse` | `events.rs` | Converted field-by-field to `FfiSubscriptionResponse` | `FfiSubscriptionResponse` |
| `ComponentStatus` | `events.rs` | Mapped to `FfiComponentStatus` enum | `FfiComponentStatus` |
| `DispatchMode` | `dispatcher.rs` | Mapped to `FfiDispatchMode` enum | `FfiDispatchMode` |

### What to update when changing these types

#### Adding/removing fields on `SourceEventWrapper` or `BootstrapEvent`

These cross as opaque pointers, so field changes don't break the FFI envelope layout.
However, you must still:

1. Bump `FFI_SDK_VERSION` in `components/plugin-sdk/src/ffi/metadata.rs` — the host and
   plugin must agree on the struct layout since they both cast the same `*mut c_void`.

2. If metadata fields are extracted (e.g., `source_id` from `SourceEventWrapper`), update
   the extraction in `components/plugin-sdk/src/ffi/vtable_gen.rs`.

#### Adding variants to `ComponentStatus` or `DispatchMode`

1. **FFI enum** — Add the variant to the corresponding `#[repr(C)]` enum in
   `components/plugin-sdk/src/ffi/types.rs` (`FfiComponentStatus` or `FfiDispatchMode`)

2. **Plugin SDK mapping** — Update the match arms in
   `components/plugin-sdk/src/ffi/vtable_gen.rs` that convert between the Rust enum
   and the FFI enum

3. **Host SDK mapping** — Update the match arms in the corresponding proxy files:
   - `ComponentStatus`: `components/host-sdk/src/proxies/source.rs` and
     `components/host-sdk/src/proxies/reaction.rs` (both have `status()` methods)
   - `DispatchMode`: `components/host-sdk/src/proxies/source.rs` (`dispatch_mode()`)

4. **Version bump** — Bump `FFI_SDK_VERSION` minor version

#### Changing `SubscriptionResponse`

This struct is deconstructed into FFI parts on the plugin side and reconstructed on
the host side:

1. **Plugin side** — `components/plugin-sdk/src/ffi/vtable_gen.rs` — look for
   `FfiSubscriptionResponse` construction (the `subscribe` vtable function)

2. **FFI struct** — `components/plugin-sdk/src/ffi/vtables.rs` — update
   `FfiSubscriptionResponse` definition

3. **Host side** — `components/host-sdk/src/proxies/source.rs` — the `subscribe()`
   method on `SourceProxy` reconstructs `SubscriptionResponse` from FFI parts

4. **Host side receivers** — `components/host-sdk/src/proxies/change_receiver.rs` —
   `ChangeReceiverProxy` and `BootstrapReceiverProxy` reconstruct the channel types
