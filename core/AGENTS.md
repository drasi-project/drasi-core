# core - drasi-core query engine

The continuous query engine: evaluates parsed Cypher/GQL ASTs against a labeled property graph and emits result diffs as elements change. Foundation crate of the workspace.

## Invariants (change with extreme care)
- Solution signatures and result keys are SpookyHash values persisted as DB keys by external index backends - changing the hash algorithm, the hashed fields, or their order breaks persisted state on upgrade
- Element/ElementValue serde shapes cross the plugin FFI boundary (MessagePack payloads in plugin-sdk) - an FFI compatibility surface; index/WAL backends persist their own separate storage models that must be updated in tandem; typespec/core-types.tsp must be kept in sync by hand when core/src/models changes (nothing validates it)
- Timestamps are epoch milliseconds throughout; effective_from validation rejects nanosecond-scale values
- in_memory_index is the reference implementation of the index traits: external backends (components/indexes/*) must match its behavior

## Testing
- The behavioral suite lives in shared-tests, not here - `cargo test -p drasi-core` alone skips it; run `cargo test -p shared-tests` after engine changes (CI runs it via `--workspace`)
- The `parallel_solver` feature is excluded from CI clippy and tests - build and test with `--features parallel_solver` when touching path_solver, because CI will not catch breakage
