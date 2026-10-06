# lib - drasi-lib embedded runtime

Embedded Drasi: sources -> continuous queries -> reactions running fully in-process inside your own Rust application. This crate has its own clippy.toml (stricter than the workspace's), rustfmt.toml, and deny.toml (a separate license/advisory policy).

## Architecture rules (stable conventions; only the runtime-flavor rule has an automated guard)
- Error layering: public API (`DrasiLib`, the `*_ops` extensions, inspection) returns `crate::error::Result`/`DrasiError`; internal modules and plugin traits use `anyhow` with `.context()`; never construct public-API errors with `anyhow!()` - see src/error.rs
- To make a new failure kind pattern-matchable, do NOT add a `DrasiError` variant (the enum is not non_exhaustive, so that breaks the public API); carry a typed error inside `anyhow` and `downcast_ref` at the boundary (existing examples: ComponentNotFoundError in src/managers/, SourceError in src/sources/traits.rs)
- Modules exported `pub` but `#[doc(hidden)]` in src/lib.rs are NOT private: plugin crates and the host SDK import them in production (e.g. sources::base, bootstrap) - treat their contents as external API despite the hidden docs
- Dependency direction: source/reaction/bootstrap implementations live in components/* and depend on this crate, never the reverse; index backends are injected through the builder and are never runtime dependencies (a rocksdb dev-dependency exists solely to test that injection path)
- End-to-end tests needing components/* crates that depend on drasi-lib (sources/reactions/bootstrappers) go in ../lib-integration-tests - dev-depending on those from lib would create a cycle; index backends do not depend on drasi-lib and may be lib dev-dependencies
- Everything must run on a current_thread tokio runtime - no block_in_place or rt-multi-thread-only APIs (guarded by lib-integration-tests/tests/runtime_flavor_tests.rs)
- A query's inbound priority-queue enqueue strategy is chosen by the SOURCE's dispatch_mode, not the query's (Channel = blocking backpressure, Broadcast = drop on full; src/queries/manager.rs); the query's own dispatch_mode only governs outbound result dispatch to reactions

## Plugin FFI compatibility surfaces (changes here are wire-format changes: update plugin-sdk/host-sdk marshaling and bump FFI_SDK_VERSION per components/AGENTS.md)
- BootstrapProvider calls cross the FFI boundary through a fixed-arity vtable: the `settings` argument and BootstrapContext properties are dropped today and silently never reach dynamically loaded providers; a new field must be threaded through the vtable and both bootstrap proxies
- src/channels event types are the wire format: QueryResult crosses the cdylib boundary serialized as-is (its serde shape IS the schema - changes compile clean but break independently built plugins); SourceEventWrapper/BootstrapEvent cross via mirror payload structs in components/plugin-sdk/src/ffi/payload.rs
- SourceSubscriptionSettings (src/config/schema.rs) crosses as individual per-field arguments through the subscribe path
