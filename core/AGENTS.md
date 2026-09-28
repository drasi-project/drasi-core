# core - drasi-core query engine

The continuous query engine: evaluates parsed Cypher/GQL ASTs against a labeled property graph and emits result diffs as elements change. Foundation crate of the workspace.

## Invariants (change with extreme care)
- Query solving, change detection, projection, aggregation and middleware evaluation have one implementation in `query/evaluator.rs`. `ContinuousQuery` keeps the standalone API; graph transactions own the same `QueryEvaluator` without a nested legacy runtime. Do not fork those algorithms.
- Solution signatures and result keys are SpookyHash values persisted as DB keys by external index backends - changing the hash algorithm, the hashed fields, or their order breaks persisted state on upgrade
- Use `crate::hashing::SpookyHasher` (or `drasi_core::hashing::SpookyHasher` downstream). It preserves the pinned `hashers` 1.0.1 algorithm without its unconditional stdout writes. Fragmented-input equivalence and stdout-silence tests protect both contracts; do not substitute a different hash implementation.
- Query-part retractions use the input identity's last actually evaluated transaction/realtime clocks. Keep these markers in the result-index transaction with aggregate changes; duplicate/stale timer hints must not retract an earlier contribution twice.
- Element/ElementValue serde shapes cross the plugin FFI boundary (MessagePack payloads in plugin-sdk) - an FFI compatibility surface; index/WAL backends persist their own separate storage models that must be updated in tandem; typespec/core-types.tsp must be kept in sync by hand when core/src/models changes (nothing validates it)
- Disconnected MATCH components enumerate actual slot candidates through `ElementIndex::get_slot_elements`; reads must include current-transaction writes. Required products and optional null defaults share the same solver and row-identity rules. Index wrappers must forward this capability or reject it explicitly, never substitute empty results.
- Timestamps are epoch milliseconds throughout; effective_from validation rejects nanosecond-scale values
- in_memory_index is the reference implementation of the index traits: external backends (components/indexes/*) must match its behavior

## Testing
- The behavioral suite lives in shared-tests, not here - `cargo test -p drasi-core` alone skips it; run `cargo test -p shared-tests` after engine changes (CI runs it via `--workspace`)
- The `parallel_solver` feature is excluded from CI clippy and tests - build and test with `--features parallel_solver` when touching path_solver, because CI will not catch breakage
