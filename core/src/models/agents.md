# agents.md — drasi-core/core/src/models

## FFI Boundary Warning

The types in this directory cross the dynamic plugin FFI boundary as **opaque pointers**
inside `#[repr(C)]` envelope structs. Both the host binary and cdylib plugins statically
link this crate, so the in-memory layout of these types **must remain identical** on both
sides.

### Types that cross FFI

| Type | File | How it crosses | FFI wrapper |
|------|------|---------------|-------------|
| `SourceChange` | `source_change.rs` | Opaque pointer inside `SourceEventWrapper` | `FfiSourceEvent.opaque` |
| `Element` | `element.rs` | Contained within `SourceChange` variants | (transitive) |
| `ElementMetadata` | `element.rs` | Metadata extracted for FFI envelope fields | `FfiSourceEvent.entity_id`, `.label` |
| `ElementValue` | `element_value.rs` | Contained within `Element` properties | (transitive) |

### What to update when changing these types

If you add, remove, or reorder fields on any of these types, you **must** also update:

1. **Plugin SDK vtable generation** — `components/plugin-sdk/src/ffi/vtable_gen.rs`
   - Search for the type name to find where metadata is extracted or the type is wrapped/unwrapped
   - `SourceChange` metadata extraction: look for `extract_source_change_metadata`
   - `SourceEventWrapper` wrapping: look for `FfiSourceEvent` construction

2. **Plugin SDK FFI types** — `components/plugin-sdk/src/ffi/types.rs`
   - If new enum variants are added (e.g., new `SourceChange` variant), update the
     corresponding `Ffi*` enum (e.g., `FfiChangeOp`)

3. **Host SDK proxy reconstruction** — `components/host-sdk/src/proxies/change_receiver.rs`
   - Where opaque pointers are cast back to concrete types via `Box::from_raw`

4. **Version bump** — `components/plugin-sdk/src/ffi/metadata.rs`
   - Bump `FFI_SDK_VERSION` minor (or major for breaking layout changes) so the host
     rejects plugins built against the old layout

### Safety invariant

The opaque pointer pattern relies on both sides agreeing on the memory layout of these
types. The `drasi_plugin_metadata()` check (SDK version + target triple) guards against
mismatches, but **only if you bump the version when layouts change**.
