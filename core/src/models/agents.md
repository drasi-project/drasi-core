# agents.md — drasi-core/core/src/models

## FFI Boundary Warning

The types in this directory cross the dynamic plugin FFI boundary **by value, as
serialized (MessagePack via `rmp-serde`) bytes** — NOT as reinterpreted
`repr(Rust)` pointers. They therefore **must derive `serde::Serialize` and
`serde::Deserialize`** (see `SourceChange`, `Element`, `ElementValue`,
`ElementMetadata`, `ElementReference`, `FutureElementRef`).

> **History (issue #602):** these types used to cross as opaque `Box::into_raw`
> pointers that the other side reclaimed with `Box::from_raw`. That is **undefined
> behavior** across independently compiled cdylibs: `repr(Rust)` has no stable
> layout, and `bytes::Bytes` / `Arc<str>` carry pointers (incl. a `&'static`
> `Bytes` vtable) that are only valid in the producing module. The result was
> non-deterministic heap corruption (`free(): invalid pointer`). The SDK version
> check does **not** make the opaque pattern sound — it validates only the
> `#[repr(C)]` envelope ABI, never the `repr(Rust)` payload layout.

### Types that cross FFI (serialized)

| Type | File | How it crosses |
|------|------|----------------|
| `SourceChange` | `source_change.rs` | Inside `SourceEventPayload` / `BootstrapEventPayload`, serialized into `FfiSourceEvent.payload_ptr` / `FfiBootstrapEvent.payload_ptr` |
| `Element` | `element.rs` | Contained within `SourceChange` variants (serialized transitively) |
| `ElementMetadata` | `element.rs` | Contained within `Element` / `SourceChange::Delete` (serialized transitively) |
| `ElementValue` | `element_value.rs` | Contained within `Element` properties (serialized transitively) |

The serialized payload structs live in `components/plugin-sdk/src/ffi/payload.rs`.

### What to update when changing these types

1. **Keep `Serialize`/`Deserialize` derives intact.** Adding/removing/reordering
   fields is safe for the wire format **only** with the named-map encoding
   (`rmp_serde::to_vec_named`, which the SDK uses) — positional encoding is
   intolerant of `#[serde(skip_serializing_if)]` and field-count changes.

2. **Plugin SDK payloads** — `components/plugin-sdk/src/ffi/payload.rs`
   (`SourceEventPayload` / `BootstrapEventPayload` and their `from_*`/`into_*`).

3. **Version bump** — bump `FFI_SDK_VERSION` in
   `components/plugin-sdk/src/ffi/metadata.rs` if you change the wire format or any
   `#[repr(C)]` envelope in `vtables.rs`, so the host rejects incompatible plugins.

### Safety invariant

Never reintroduce `Box::into_raw` / `Box::from_raw` (or `&*ptr` reads, or
`std::ptr::read`) of these `repr(Rust)` types across the cdylib boundary. Cross
them only as serialized bytes. The `#[repr(C)]` **envelope** structs in
`vtables.rs` may cross as `Box`es (their layout is stable); the rich `repr(Rust)`
payloads may not.
