# AGENTS.md: `drasi-lib`

Orientation for editing `drasi-lib` (Rust path `drasi_lib::`). `lib/README.md` is the long-form reference; the per-directory `AGENTS.md` files under `lib/src/` own the FFI-boundary contracts.

## Shape
- `lib/src/lib.rs` re-exports the documented API surface. The `#[cfg_attr(not(test), doc(hidden))]` modules are public too, and plugin crates import them directly in production. Treat their contents as external API.
- The only construction path is `DrasiLib::builder()...build().await?` then `start().await?`. `DrasiLib::new` and `initialize` are `pub(crate)`, and `build()` calls `initialize()` for you.
- `DrasiLib` is Arc-internal and implements `Clone`, so cloning is cheap; clone it to share.
- `lib/src/lib_core.rs` holds the struct, lifecycle and public accessors; per-component CRUD is in `lib/src/lib_core_ops/*_ops.rs` and inspection logic in `lib/src/inspection.rs` (the `*_ops` inspection methods just forward). Check all three before concluding a method does not exist. `ComponentGraph` (`lib/src/component_graph/`) is the source of truth for status and relationships.

## Rules that prevent damage
- Errors follow a strict layering contract: the high-level surface (`DrasiLib`, the `*_ops` modules, `InspectionAPI`) returns `DrasiError`; everything beneath it, including plugin trait impls, returns `anyhow::Result`. Nothing enforces this at compile time, so read the `lib/src/error.rs` module docs before changing any error signature.
- To make a failure pattern-matchable, do not add a `DrasiError` variant (a public API change). Carry a typed error inside `anyhow` and `downcast_ref` at the edge: see `ComponentNotFoundError` (`lib/src/managers/mod.rs`) and `classify_component_error` (`lib/src/inspection.rs`).
- drasi-lib *defines* the plugin traits; plugin crates depend on it, never the reverse. Do not add a concrete plugin crate to `[dependencies]`; `drasi-index-rocksdb` is a dev-dependency.
- drasi-lib loads nothing dynamically: no `libloading`, no FFI code in `lib/src`. Dynamic loading lives in `components/host-sdk`, which hands drasi-lib ordinary Rust trait objects.
- No `.unwrap()` in non-test code. `unwrap_used` is a *clippy* lint, so only `cargo clippy` catches it, never `cargo build`; `clippy.toml` permits it in tests. New `.rs` files need the Apache-2.0 header. `RUSTFLAGS=-Dwarnings` comes from the `Makefile`, not from CI.

## Traps
- Sources and reactions are **instances, not configuration**: `DrasiLibConfig` (`lib/src/config/schema.rs`) has no `sources:`/`reactions:` section, and there is no config loader.
- A persistent (plugin) storage backend cannot be resolved from config alone in embedded mode: `build()` rejects it and demands `with_index_provider(name, ...)`. In-memory backends are fine.
- A query's inbound priority queue is fed by the **source's** `DispatchMode`, not the query's own: Channel uses `enqueue_wait()` (backpressure), Broadcast drops on full (`lib/src/queries/manager.rs`).
- In Channel mode a source has zero dispatchers until a query subscribes: call `SourceBase::wait_for_subscribers()` before a poll loop, and `SourceBase::clear_dispatchers()` if you write your own `stop()`. Skipping either silently drops events as the checkpoint advances.
- Start order Sources -> Queries -> Reactions (reverse on stop) is a correctness requirement, not style (`lib/src/lifecycle.rs`). The host owns reaction subscriptions; a `Reaction` must not.
- `stop()` is resumable, `shutdown()` is permanent; `start()` after it is an invalid-state error.
- `cargo test -p drasi-lib` is hermetic. A workspace-wide `cargo test` needs Docker: `lib-integration-tests` and backends such as `drasi-index-garnet` start testcontainers.
- Lint with `make clippy`, not `--all-features` (bundled-jq builds jq from source). `default = []`, so a query using a middleware whose feature is off fails at *runtime*, not compile time.
- `examples/lib/` crates are independent workspaces that no CI job builds; API breaks are silent.
