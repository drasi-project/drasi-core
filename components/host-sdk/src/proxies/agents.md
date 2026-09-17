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

### Rich event payloads cross as serialized bytes (issue #602)

`SourceEventWrapper`, `BootstrapEvent`, and `QueryResult` are `repr(Rust)` types
that embed `bytes::Bytes` / `Arc<str>`. They cross the boundary **by value as
serialized MessagePack bytes**, NOT as reinterpreted opaque pointers:

- `change_receiver.rs` — `decode_source_event` / `decode_bootstrap_event`
  deserialize the host's own value from `FfiSourceEvent.payload_ptr` /
  `FfiBootstrapEvent.payload_ptr`, then free the plugin's buffer via the
  plugin-supplied `payload_drop_fn`.
- `reaction.rs` — `QueryResult` is serialized (`encode_query_result`) into an
  `FfiQueryResult` buffer; the plugin deserializes its own copy.
- Reconstruction helpers live in `drasi_plugin_sdk::ffi::payload`
  (`decode_source_event_payload`, `decode_bootstrap_event_payload`,
  `decode_query_result`).

> **Never** `Box::from_raw` / `Box::into_raw` / `std::ptr::read` / `&*ptr`-read a
> `repr(Rust)` payload type across the cdylib boundary. `repr(Rust)` has no stable
> cross-compilation layout, and `bytes::Bytes` carries a `&'static` vtable valid
> only in the producing module — doing so caused #602's heap corruption
> (`free(): invalid pointer`). The `#[repr(C)]` **envelope** structs
> (`FfiSourceEvent`, `FfiBootstrapEvent`, `FfiQueryResult`) themselves are POD and
> may be freed across the boundary with `Box::from_raw`.

**Safety**: The SDK version check (`loader.rs`) validates only the `#[repr(C)]`
envelope/vtable ABI — it does **not** and cannot guarantee `repr(Rust)` payload
layout agreement. That is why payloads must be serialized.

If you change the definition of `SourceEventWrapper`, `BootstrapEvent`,
`QueryResult`, or the payload structs (in `drasi-core/core/src/models/`,
`drasi-core/lib/src/channels/`, or `plugin-sdk/src/ffi/payload.rs`), keep their
`serde` derives intact and bump `FFI_SDK_VERSION` in
`components/plugin-sdk/src/ffi/metadata.rs`.

### `Arc<Library>` lifetime management

Every proxy holds an `Arc<Library>` (or `Option<Arc<Library>>` for bootstrap) to keep
the cdylib loaded. When all proxies from a plugin are dropped, the library is unloaded.
Do not remove these fields — they prevent use-after-unload crashes.
