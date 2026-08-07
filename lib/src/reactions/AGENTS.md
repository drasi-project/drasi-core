# AGENTS.md: drasi-core/lib/src/reactions

## Why this trait is an ABI

The `Reaction` trait defined here is what reaction plugins implement. For dynamically loaded plugins its methods are dispatched through an `#[repr(C)]` vtable across a shared library boundary. Host and plugin are compiled separately, so the trait is an ABI contract, and a source compatible change can still be a breaking layout change.

`ReactionVtable` in `components/plugin-sdk/src/ffi/vtables.rs` is the authoritative list of what crosses and in what form. Read it there rather than any copy.

## Result delivery is host driven, and push based

Reactions do not subscribe to queries themselves. The host owns subscriptions and drives delivery.

For dynamic plugins the host does not call the reaction once per result. It hands the plugin a callback, and the plugin runs its own forwarder loop that pulls results and enqueues them into its priority queue. Look at the result push entry in `ReactionVtable` and its generator in `vtable_gen.rs` for the current shape.

Results cross as serialized bytes inside a `#[repr(C)]` envelope, not as pointers into the other side's heap. Do not pass `repr(Rust)` values across as `Box::into_raw` / `Box::from_raw`: that is what issue #602 fixed, and it caused heap corruption. See `lib/src/channels/AGENTS.md` for the full reasoning.

## Changing the `Reaction` trait

Adding, removing or changing a method means updating all four of these together:

1. `components/plugin-sdk/src/ffi/vtables.rs`, `ReactionVtable`: the function pointer field.
2. `components/plugin-sdk/src/ffi/vtable_gen.rs`: BOTH reaction vtable builders, the generic `build_reaction_vtable()` and its independent `_from_boxed` twin used by the descriptor path; they do not share code, so an entry changed in one must change in the other.
3. `components/host-sdk/src/proxies/reaction.rs`, `ReactionProxy`: the host side dispatch.
4. `components/plugin-sdk/src/ffi/metadata.rs`, `FFI_SDK_VERSION`: the vtable layout changed, so this is a major bump.

Watch the two omissions nothing catches: a trait method with a default body (the proxy compiles and silently serves the default instead of dispatching across FFI), and a forgotten `FFI_SDK_VERSION` bump (passes the load check, fails only at runtime).

## Changing `ReactionPluginDescriptor`

The factory trait in `components/plugin-sdk/src/descriptor.rs` is wrapped the same way, through `ReactionPluginVtable`, `build_reaction_plugin_vtable()` and `ReactionPluginProxy`. If the `config_schema_json()` output format changes, the schema merging in `drasi-server/src/api/` needs updating too.
