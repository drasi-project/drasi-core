# AGENTS.md: `drasi-core`

Rust workspace (91 members) implementing **continuous queries**: streams of `SourceChange` (`Insert`/`Update`/`Delete`/`Future`) incrementally maintained as result sets over a labeled property graph. Two entry points: `QueryBuilder` (`core/src/query/query_builder.rs`), the storage- and parser-agnostic engine, and `DrasiLib` (`lib/`), the embedded runtime wiring sources -> queries -> reactions. `lib/` has its own `AGENTS.md`; `lib/README.md` is the long-form reference.

## Layout

`Cargo.toml` `[workspace] members` is the authoritative inventory. Read it; do not trust prose plugin lists, they rot within months.

- **Language**: `query-ast` (the shared AST contract), parsed into by `query-cypher` and `query-gql`; `functions-cypher`/`functions-gql` bind builtins into core's `FunctionRegistry`.
- **Engine**: `core` (evaluation, path_solver, `interface` traits, in-memory + cached indexes) and `middleware` (seven feature-gated `SourceMiddleware` stages).
- **Runtime**: `lib`, plus `components/`: `sources/`, `reactions/`, `bootstrappers/`, `indexes/`, `state_stores/`, `wals/`, `secret_stores/`, `identity/`. `plugin-sdk`/`host-sdk`/`ffi-primitives` are the FFI dynamic-plugin seam (`components/plugin-architecture.md`).
- **Support**: `shared-tests` (backend compliance suite), `query-perf`, `examples/`, `xtask`.

## Build & test

- Rust **1.95.0**, pinned in `rust-toolchain.toml`, which also supplies `clippy`/`rustfmt`. The pre-commit hook runs plain `cargo fmt -- --check` on that toolchain, so do not use `+nightly`.
- CI runs `cargo test --workspace --exclude drasi-host-sdk` then `make test-host-sdk` (`.github/workflows/test.yml`); host-sdk tests need the cdylib plugins built first, and `test-ffi.yml` separately re-runs the host-sdk integration test on Linux/macOS/Windows.
- `make clippy` uses the same feature list as `ci-lint.yml`. `--all-features` is deliberately avoided: `bundled-jq` compiles jq from source and is flaky.
- libjq is **not** needed to build: `drasi-middleware` is `default = []` and only the opt-in `jq` feature dynamic-links system libjq (set `JQ_LIB_DIR`). Linting is the exception, since `make clippy` enables `drasi-middleware/all`, which includes `jq`.
- Not hermetic: a plain `cargo test --workspace` starts testcontainers (Redis and Postgres images among others), so Docker must be running. Some plugin builds need `protobuf-compiler`; the kafka crates pull in `cmake` via `rdkafka/cmake-build` under `dynamic-plugin`.
- `.rs` files carry the Apache 2.0 header; commits need `Signed-off-by:` (`git commit -s`). Lint policy lives in `[workspace.lints]` and `clippy.toml`; `make clippy` is the check.

## Before editing

- Storage is trait-injected and the contracts live in **two** crates: engine-side index/checkpoint/ session traits in `core/src/interface/`, runtime provider traits (`StateStoreProvider`, `WalProvider`, `SecretStoreProvider`, `IdentityProvider`) in `lib/src/`. Check both.
- `core/src/index_cache/` wraps `ElementIndex`, `ResultIndex` and `FutureQueue`; changing one of those three also breaks the in-memory, RocksDB and Garnet impls plus test mocks in `lib/`. The backend plugin contract is the `IndexBackendPlugin` trait (`core/src/interface/index_backend.rs`).
- `lib`/`DrasiLib` has **zero compile-time knowledge of any plugin**: `lib/Cargo.toml` names no plugin crate outside dev-dependencies. Instances arrive as trait objects, through the component and provider traits (injected via `DrasiLibBuilder`) or across the FFI seam. Do not add a plugin dependency to `lib`.
- Session atomicity is **opt-in**: `QueryBuilder` defaults to `NoOpSessionControl`; only a real backend supplies transactions.
- A crate is discovered as a dynamic plugin when it defines a `dynamic-plugin` feature **and** is named `drasi-<type>-<kind>` where `<type>` is one of `source`, `reaction`, `bootstrap`, `identity`, `secret-store` (`parse_plugin_type_kind` in `xtask/src/main.rs`; `readme.md` lists only the first three and is stale). `cargo run -p xtask -- list-plugins` is the ground truth.
- Neither parser supports `ORDER BY`, `LIMIT`, `SKIP`, `TOP` or `DISTINCT`: those nodes do not exist in `query-ast`, so it is a language gap, not an engine flag.
