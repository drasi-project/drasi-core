# components - plugin ecosystem

All Drasi plugins (sources, bootstrappers, reactions, indexes, identity, secret stores, state stores, WALs) plus the FFI backbone (ffi-primitives, plugin-sdk, host-sdk). Plugins build both statically linked and as dynamically loaded cdylibs that external hosts load via FFI. Read components/plugin-architecture.md before touching the FFI layer.

## The plugin contract (discovery is convention-driven)
- Existing ABI plugins follow drasi-{source|reaction|bootstrap|identity|secret-store}-<kind>. Native graph plugins declare `[package.metadata.drasi-plugin]` with `abi-family = "computation"`, an explicit `abi-version`, and `kind`. Both declare `dynamic-plugin` and crate-type ["lib","cdylib"]. Verify discovery with `cargo run -p xtask -- list-plugins`; native metadata is validated rather than inferred from the old category list.
- `export_plugin!` must stay gated behind the `dynamic-plugin` feature (prevents duplicate FFI symbols in static builds)
- The existing Source/Reaction plugin ABI is versioned by `FFI_SDK_VERSION` (plugin-sdk/src/ffi/metadata.rs); bump it when changing that family's layouts or wire formats. Native ComputationGraph plugins use their independently versioned computation-plugin-abi contract. Changes to a shared layout require updating every affected ABI family; adding a native-only interface must not change the existing 0.15 contract.
- Event payloads cross the FFI boundary only as serialized MessagePack - never as repr(Rust) pointers
- ffi-primitives must stay dependency-free (std only)
- Indexes, state stores, and WALs are NOT dynamic plugins - they link statically (wiring rules: see lib/AGENTS.md)
- Native libraries are process-pinned and expose only implemented ABI capabilities. Never pass Rust futures, trait objects, Arc/Bytes ownership or borrowed TransactionContext pointers across the new boundary. Transaction participants use the revocable host mailbox and have no commit operation.
- .tsp files inside plugin src/ directories are wire-format documentation, not codegen inputs - there is no codegen step to find, and they should not be deleted

## Writing or changing a plugin
- The README.md files in sources/ and reactions/ are normative RFC-2119 developer guides - read the relevant one before writing or changing a plugin (bootstrappers/README.md is a non-normative how-to); reactions/README.md section 14 is a conformance checklist to walk before finishing
- Reaction traps: `properties()` must return ALL config including secrets (it is the persistence hook - filtering corrupts saved config); the default `enqueue_query_result` silently drops results unless delegated to ReactionBase
- A system's source and bootstrapper must share element-ID generation and type mapping through its <system>-common crate - divergence silently corrupts query state
- reactions/snapshot-test is a publish=false FFI test harness, deliberately exempt from guide compliance
- Test choreography (Docker/testcontainers, host-sdk plugin builds) is owned by the root AGENTS.md "Done means" section
