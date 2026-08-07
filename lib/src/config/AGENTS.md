# AGENTS.md: drasi-core/lib/src/config

## FFI Boundary Warning

`SourceSubscriptionSettings` defined in `schema.rs` crosses the dynamic plugin FFI boundary, deconstructed into individual FFI-safe arguments with mixed encodings, not passed as one serialized value. The `subscribe_fn` signature in `components/plugin-sdk/src/ffi/vtables.rs` is the authoritative mapping.

### What to update when changing `SourceSubscriptionSettings`

If you add or remove fields:

1. **FFI vtable function** — `components/plugin-sdk/src/ffi/vtables.rs` → `SourceVtable.subscribe_fn` — the function pointer signature may need new parameters

2. **Plugin-side vtable generation** — `components/plugin-sdk/src/ffi/vtable_gen.rs`
   - The `subscribe` vtable function that deconstructs `SourceSubscriptionSettings` into `FfiStr` args (source_id, query_id, nodes JSON, relations JSON, etc.)
   - The plugin-side reconstruction that builds `SourceSubscriptionSettings` from the FFI args

3. **Host-side proxy** — `components/host-sdk/src/proxies/source.rs` → `SourceProxy.subscribe()` — serializes `SourceSubscriptionSettings` fields into `FfiStr` values and calls the vtable

4. **Version bump** — `components/plugin-sdk/src/ffi/metadata.rs` → `FFI_SDK_VERSION`

### Serialization approach

Encodings are mixed: some fields cross as JSON strings, some as plain `FfiStr` or primitives, and the resume position as a raw pointer plus length. The two easiest fields to omit when extending the mapping are the recovery ones (`resume_from`, `request_position_handle`), and the settings are rebuilt on the plugin side in both vtable builders, which must stay in sync. Check `subscribe_fn` and both reconstructions in `vtable_gen.rs` before deciding how a new field crosses.
