# AGENTS.md: drasi-core/lib/src/sources

## Why this trait is an ABI

The `Source` trait defined here is what source plugins implement. For dynamically loaded plugins its methods are not called directly: they are dispatched through an `#[repr(C)]` vtable across a shared library boundary. Host and plugin are compiled separately, so the trait is an ABI contract, not just a Rust interface, and a change that looks source compatible can still be a breaking layout change.

`SourceVtable` in `components/plugin-sdk/src/ffi/vtables.rs` is the authoritative list of what crosses and in what form. Read it there. It tracks the trait, and any copy of it here would not.

Payload rule: event payloads are serialized and carried inside `#[repr(C)]` envelopes rather than reinterpreted as `repr(Rust)` values on the far side. Other pointers in this ABI mean other things, so read the signature rather than assuming. See `lib/src/channels/AGENTS.md` for why the payload rule exists.

## Changing the `Source` trait

Adding, removing or changing a method means updating all four of these together:

1. `components/plugin-sdk/src/ffi/vtables.rs`, `SourceVtable`: the function pointer field.
2. `components/plugin-sdk/src/ffi/vtable_gen.rs`: BOTH source vtable builders, the generic `build_source_vtable()` and its independent `_from_boxed` twin used by the descriptor path; they do not share code, so an entry changed in one must change in the other. Async entries must go through `dispatch_to_runtime()`, never `Handle::block_on`: tokio panics on nested runtimes; the helper's doc comment in `vtable_gen.rs` explains why.
3. `components/host-sdk/src/proxies/source.rs`, `SourceProxy`: the host side dispatch.
4. `components/plugin-sdk/src/ffi/metadata.rs`, `FFI_SDK_VERSION`: the vtable layout changed, so this is a major bump.

Watch the two omissions nothing catches: a trait method with a default body (the proxy compiles and silently serves the default instead of dispatching across FFI), and a forgotten `FFI_SDK_VERSION` bump (passes the load check, fails only at runtime).

## Changing `SourcePluginDescriptor`

The factory trait in `components/plugin-sdk/src/descriptor.rs` is wrapped the same way, through `SourcePluginVtable`, `build_source_plugin_vtable()` and `SourcePluginProxy`. If the `config_schema_json()` output format changes, the schema merging in `drasi-server/src/api/` needs updating too.
