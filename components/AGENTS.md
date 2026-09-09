# components - plugin ecosystem

All Drasi plugins (sources, bootstrappers, reactions, indexes, identity, secret stores, state stores, WALs) plus the FFI backbone (ffi-primitives, plugin-sdk, host-sdk). Plugins build both statically linked and as dynamically loaded cdylibs that external hosts load via FFI. Read components/plugin-architecture.md before touching the FFI layer.

## The plugin contract (discovery is convention-driven)
- Crate names must follow drasi-{source|reaction|bootstrap|identity|secret-store}-<kind> and declare a `dynamic-plugin` feature with crate-type ["lib","cdylib"] - xtask silently skips nonconforming crates when building and publishing plugins; verify with `cargo run -p xtask -- list-plugins`
- `export_plugin!` must stay gated behind the `dynamic-plugin` feature (prevents duplicate FFI symbols in static builds)
- Bump `FFI_SDK_VERSION` (plugin-sdk/src/ffi/metadata.rs) on ANY `#[repr(C)]` layout or wire-format change, including changes made in ffi-primitives or host-sdk
- Event payloads cross the FFI boundary only as serialized MessagePack - never as repr(Rust) pointers
- ffi-primitives must stay dependency-free (std only)
- Indexes, state stores, and WALs are NOT dynamic plugins - they link statically (wiring rules: see lib/AGENTS.md)
- .tsp files inside plugin src/ directories are wire-format documentation, not codegen inputs - there is no codegen step to find, and they should not be deleted

## Writing or changing a plugin
- The README.md files in sources/ and reactions/ are normative RFC-2119 developer guides - read the relevant one before writing or changing a plugin (bootstrappers/README.md is a non-normative how-to); reactions/README.md section 14 is a conformance checklist to walk before finishing
- Reaction traps: `properties()` must return ALL config including secrets (it is the persistence hook - filtering corrupts saved config); the default `enqueue_query_result` silently drops results unless delegated to ReactionBase
- A system's source and bootstrapper must share element-ID generation and type mapping through its <system>-common crate - divergence silently corrupts query state
- reactions/snapshot-test is a publish=false FFI test harness, deliberately exempt from guide compliance
- Test choreography (Docker/testcontainers, host-sdk plugin builds) is owned by the root AGENTS.md "Done means" section
