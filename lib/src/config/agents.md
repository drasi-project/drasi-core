# agents.md — drasi-core/lib/src/config

## FFI Boundary Warning

`SourceSubscriptionSettings` defined in `schema.rs` crosses the dynamic plugin FFI
boundary. It is deconstructed into individual FFI-safe arguments (not passed as an
opaque pointer).

### Types that cross FFI

| Type | File | How it crosses |
|------|------|---------------|
| `SourceSubscriptionSettings` | `schema.rs` | Fields are serialized to JSON strings and passed as `FfiStr` args to `subscribe_fn` |

### What to update when changing `SourceSubscriptionSettings`

If you add or remove fields:

1. **FFI vtable function** — `components/plugin-sdk/src/ffi/vtables.rs` →
   `SourceVtable.subscribe_fn` — the function pointer signature may need new parameters

2. **Plugin-side vtable generation** — `components/plugin-sdk/src/ffi/vtable_gen.rs`
   - The `subscribe` vtable function that deconstructs `SourceSubscriptionSettings`
     into `FfiStr` args (source_id, query_id, nodes JSON, relations JSON, etc.)
   - The plugin-side reconstruction that builds `SourceSubscriptionSettings` from
     the FFI args

3. **Host-side proxy** — `components/host-sdk/src/proxies/source.rs` →
   `SourceProxy.subscribe()` — serializes `SourceSubscriptionSettings` fields into
   `FfiStr` values and calls the vtable

4. **Version bump** — `components/plugin-sdk/src/ffi/metadata.rs` → `FFI_SDK_VERSION`

### Serialization approach

Currently, `nodes` and `relations` fields are serialized as JSON strings across FFI
(via `serde_json::to_string`), while scalar fields like `source_id`, `query_id`, and
`enable_bootstrap` are passed directly as `FfiStr` or `bool`. If you add a new field,
decide whether to:
- Pass it as a direct `FfiStr`/primitive (for simple scalar values)
- Serialize it as JSON (for complex nested types like `Vec<NodeLabel>`)
