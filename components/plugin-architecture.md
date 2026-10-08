# Drasi Plugin Architecture

This document describes the plugin system architecture for Drasi Server, covering both the static (builtin) and dynamic (cdylib) plugin loading approaches.

## Overview

Drasi Server supports two build modes for plugins:

| Mode | Feature Flag | How Plugins Are Loaded |
|------|-------------|----------------------|
| **Static** (default) | `builtin-plugins` | Plugins are statically linked into the server binary |
| **Dynamic** | `dynamic-plugins` | Plugins are self-contained `.so`/`.dylib`/`.dll` files loaded at runtime |

Both modes use the same plugin source code — the `export_plugin!` macro generates FFI entry points only when the `dynamic-plugin` feature is enabled on a plugin crate.

## Architecture Diagram

```
┌──────────────────────────────────────────────────────────────────┐
│                      drasi-server (host binary)                  │
│                                                                  │
│  API routes, config persistence, OpenAPI spec, server lifecycle  │
│  Uses DrasiLib for query processing                              │
│                                                                  │
│  Build modes:                                                    │
│  • builtin-plugins (default): static linking, no FFI overhead    │
│  • dynamic-plugins: uses drasi-host-sdk to load cdylib plugins   │
├──────────────────────────────────────────────────────────────────┤
│                      drasi-host-sdk (library crate)              │
│                                                                  │
│  ┌──────────────────┐  ┌──────────────────────────────────────┐  │
│  │  PluginLoader    │  │  Proxy wrappers (impl DrasiLib traits)│  │
│  │                  │  │                                      │  │
│  │  load .so/.dll   │  │  SourceProxy       (wraps vtable)    │  │
│  │  validate meta   │  │  ReactionProxy     (wraps vtable)    │  │
│  │  call init()     │  │  SourcePluginProxy (wraps factory)   │  │
│  └──────────────────┘  └──────────────────────────────────────┘  │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │  Callbacks: host_log_callback, host_lifecycle_callback   │    │
│  │  State store vtable construction (host → plugin)         │    │
│  │  Schema merging: plugin JSON → utoipa OpenAPI schemas     │    │
│  └──────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────┘
                ↕ stable C ABI (#[repr(C)] vtables)
                ↕ serialized payloads inside #[repr(C)] envelopes
┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐
│ libdrasi_source  │ │ libdrasi_react   │ │ libdrasi_boot    │
│ _mock.so (cdylib)│ │ _log.so (cdylib) │ │ _pg.so (cdylib)  │
│                  │ │                  │ │                  │
│ Own tokio runtime│ │ Own tokio runtime│ │ Own tokio runtime│
│ Own deps (serde, │ │ Own deps (serde, │ │ Own deps (serde, │
│  tracing, etc.)  │ │  tracing, etc.)  │ │  tracing, etc.)  │
│                  │ │                  │ │                  │
│ drasi_plugin_    │ │ drasi_plugin_    │ │ drasi_plugin_    │
│ init() → Reg     │ │ init() → Reg     │ │ init() → Reg     │
└──────────────────┘ └──────────────────┘ └──────────────────┘
```

## Crate Responsibilities

| Crate | Location | Role |
|-------|----------|------|
| `drasi-plugin-sdk` | `drasi-core/components/plugin-sdk` | Plugin-side SDK: FFI types, vtables, `export_plugin!` macro, vtable generation, FfiLogger, FfiStateStoreProxy |
| `drasi-host-sdk` | `drasi-core/components/host-sdk` | Host-side SDK: `PluginLoader`, proxy types (impl Source/Reaction/SourcePlugin), callback wiring, schema merging |
| `drasi-server` | `drasi-server/` | Application: REST API, config persistence, OpenAPI spec, server lifecycle — uses `drasi-host-sdk` for dynamic loading |
| `drasi-lib` | `drasi-core/lib/` | Core processing: hosts the query engine and defines the channel event types that are the FFI wire payloads — no FFI code itself |
| `drasi-core` | `drasi-core/core/` | Engine and core data model — its types cross FFI only inside serialized payloads |

Native computation ABI 1.0 also has an optional service-v1 entry point; its base
tables and strict metadata stay frozen. New hosts retain genuine old native
binaries for fast mode. Negotiated single-output sources may bind the host's
actual outgoing QoS admission service, with immutable identity, bounded requests,
typed uncertain outcomes and independent listener-failure reporting. Neither an
arbitrary store nor a Rust graph object crosses that interface. Native HTTP/gRPC
adopts this path; legacy shared admission is still pending. See the
[native SDK contract](computation-plugin-sdk/README.md#optional-graph-owned-source-admission).

The independent recovery-v1 extension now supplies bounded, read-only source
progress and cancellation-safe subscriptions. Plugins receive no local owner
object or mutation operation; the host retains the actual query/middleware
resource for graph recovery assertions and revokes the service before replacement.
Production database plugin registration remains separate. See the
[progress contract](computation-plugin-sdk/README.md#optional-read-only-source-progress).

Native bootstrap-v1 separately negotiates query-owned provider factories, bounded
snapshot polling and borrowed initialization-state requests. It reuses the host's
revocable request mailbox and existing plugin I/O drivers, with no new executor or
component role. The query retains final commit and cleanup ownership. See the
[bootstrap contract](computation-plugin-sdk/README.md#optional-query-owned-bootstrap).

Native consumer-v1 negotiates external or transactional handled consumers. The
Host retains the existing delivery runner, progress, retries and storage ownership;
the plugin sees a bounded batch and stable operation keys, plus a borrowed state
capability for transactional handlers. A graph-owned `NativeConsumerResource`
binds the provider/options without exposing Rust storage handles across the ABI.
See the [consumer contract](computation-plugin-sdk/README.md#optional-host-owned-consumer-completion).

Managed native consumer retirement uses the Host's existing delivery runner and
unchanged awaited storage shutdown. Only a validated, completely handled ledger
from a healthy owner supplies drain evidence; successful stop alone does not.
A held lifecycle excludes reopening until configuration is resolved, without
retaining indexes in inspection snapshots. Native bootstrap retirement similarly
holds its existing call gate after successful stop. Acceptance or abandonment
revokes the old provider; terminal shutdown can revoke a held lease, and later
rejection cannot reopen it. These are Host/framework services, not new ABI calls.
Revision-bound, durable loss authorization can abandon a completely removed
domain's pending obligations, including a partial consumer after actual successful
cleanup. It neither deletes stored data nor bypasses cleanup or transaction-owner
health checks; production plugin migration is not part of this service.

## Lifecycle observations

Graph-provided `ComponentUpdateSender` handles use a bounded, coalescing mailbox.
A burst of callbacks cannot hide readiness, the first unobserved failure, or the
latest status. At most three observations are pending; this is not a history of
every transition. The host callback does not block, poll, or spawn forwarding
tasks. Closing or replacing the observer revokes its old callback handles.
Plugins must still stop and join their workers: coalescing does not make late
callbacks from an unjoined worker safe across a restart of that same instance.
After failed initialization, a status handle can replace its closed observer;
keeping the old one would silently disconnect the retried instance. A live
observer is not replaced.

The ABI 0.17 SDK catches source/reaction initialization panics, waits for the
initializer to finish unwinding, and reports the original failure through the
lifecycle callback. That instance permanently rejects initialization, activation,
subscriptions and bootstrap/delivery work. Stop and deprovision remain available
for cleanup; successful cleanup does not make the damaged instance reusable.
Graph-owned reconstruction factories run only after cleanup succeeds. Without a
factory, the owner must replace the instance explicitly. Rejected initialization
still releases newly transferred context resources.

This applies to both concrete and boxed SDK wrappers, not to arbitrary panics in
every foreign function. ABI 0.16 loading compatibility does not retrofit these
new SDK protections into old binaries. Pending legacy lifecycle calls still
block their caller; broader cancellation and worker ownership remain unfinished.

The shared Rust `context::workers` helpers reserve ownership before spawning and
join in place. Cancellation and timeout retain unjoined handles; abort is only a
request, not proof of exit. Source, query and reaction base cleanup now uses this
contract. Custom listener, pruning and heartbeat tasks still require explicit
adoption; the helpers do not discover or fence arbitrary plugin-owned work.

`ComponentUpdateSender` is now a Rust wrapper rather than an MPSC type alias.
Runtime-context and status-handle constructors accept ordinary MPSC senders too;
those retain their original queue/backpressure semantics. Explicit context struct
literals use `update_tx: sender.into()`. Lifecycle callback layouts and payloads
are unchanged by this wrapper; no new FFI fields or recovery RPCs are added.

## Plugin Types

Drasi supports three types of plugins:

### Source Plugins
Ingest data from external systems (PostgreSQL, HTTP, gRPC, etc.) and emit `SourceChange` events.

**Trait**: `drasi_lib::sources::Source` **Descriptor**: `drasi_plugin_sdk::descriptor::SourcePluginDescriptor`

### Reaction Plugins
Consume query results and take actions (webhooks, SSE, logging, etc.).

**Trait**: `drasi_lib::reactions::Reaction` **Descriptor**: `drasi_plugin_sdk::descriptor::ReactionPluginDescriptor`

### Bootstrap Plugins
Provide initial data snapshots to populate queries when sources are connected.

**Trait**: `drasi_lib::bootstrap::BootstrapProvider` **Descriptor**: `drasi_plugin_sdk::descriptor::BootstrapPluginDescriptor`

## How Dynamic Plugin Loading Works

### Loading Sequence

```
1. Server starts with --features dynamic-plugins
   ↓
2. PluginLoader scans plugin directory for matching .so/.dylib/.dll files
   ↓
3. For each plugin file:
   a. dlopen() the shared library
   b. Resolve drasi_plugin_metadata() → PluginMetadata
   c. Validate SDK version (major.minor must match host)
   d. Validate target triple (must match host)
   e. Resolve drasi_plugin_init() → FfiPluginRegistration
   f. Call init → plugin initializes its tokio runtime, installs FfiLogger
   g. Wire log callback (plugin → host logging)
   h. Wire lifecycle callback (plugin → host events)
   i. Extract descriptor vtables into proxy types
   ↓
4. Register proxy types into PluginRegistry
   ↓
5. Host uses descriptors to create instances on demand:
   descriptor.create_source(id, config_json, auto_start) → SourceProxy
```

### Plugin Entry Points

Every cdylib plugin exports exactly two symbols:

```rust
// Returns version/compatibility metadata (safe to call with any ABI)
#[no_mangle]
pub extern "C" fn drasi_plugin_metadata() -> *const PluginMetadata

// Initializes the plugin and returns descriptor factories
#[no_mangle]
pub extern "C" fn drasi_plugin_init() -> *mut FfiPluginRegistration
```

### Version Validation

| Field | Check | Severity | Rationale |
|-------|-------|----------|-----------|
| `sdk_version` | Explicit ABI allowlist: 0.16.x / 0.17.x | **REJECT** | Only the documented compatible prefix may cross versions |
| `target_triple` | Exact match | **REJECT** | Cannot load x86_64 `.so` on aarch64 |
| `plugin_version` | Log only | **INFO** | Plugin's own version — no compatibility constraint |

The current SDK contract is `0.17.0`. State-store key listing returns an explicit
success/error result; a failed lookup cannot look like an empty store. Bootstrap
calls forward subscription settings and runtime context properties through both
proxies. Source sequences, including an explicit replay request from zero, remain
preserved. Hosts reject missing/null metadata and incompatible SDK versions before
initialization. ABI 0.16 plugins remain loadable for existing fast-mode behavior;
rebuild to use the new state-store declaration and ownership contract. A plugin's
package/registry SDK release version is separate from this FFI contract.

| Host ABI | Plugin ABI | Supported |
|---|---|---|
| Legacy 0.17.x | Legacy 0.17.x, same target | Yes |
| Legacy 0.17.x | Legacy 0.16.x, same target | Existing behavior; no new recovery services |
| Legacy 0.16.x | Legacy 0.17.x | No; upgrade host first |
| Legacy 0.17.x | Legacy 0.15.x or older, unknown future ABI | No |
| Legacy 0.17.x | Missing/null/malformed metadata or wrong target | No |
| Native computation 1.0.0 | Native computation 1.0.0 | Unchanged; independently versioned |

ABI 0.17 appends a fixed-size versioned durability callback after the ABI 0.16
`StateStoreVtable` prefix. It reports the actual provider's process-restart,
power-loss and storage-loss declarations. Unsupported/failed descriptions return
an error; the infallible Rust trait logs that failure and returns `Unknown`,
which cannot satisfy a recovery assertion. The final 0.17 proxy releases its
transferred table and provider; 0.16 proxies retain their original persistent
pointer lifetime. No per-message recovery fields are added.
Storage evidence alone does not establish replay, transaction participation,
handled delivery or once-only external effects. Legacy plugin paths still cannot
satisfy whole-path recovery assertions without the shared recovery services.

Bootstrap settings/properties are JSON, bounded by the SDK payload limit.
Malformed values return a failed bootstrap, not defaults. The bootstrap sequence
counter is still provider-local across FFI; this does not establish gap-free
snapshot/live handover or a shared sequence allocator.

## FFI Boundary Design

### Serialized Payload Pattern

Rich event payloads cross the FFI boundary as serialized MessagePack bytes carried inside `#[repr(C)]` envelope structs. The envelope has a stable layout; the payload is self-describing, so neither side reinterprets the other's `repr(Rust)` memory.

```rust
// Plugin creates a SourceChange (rich Rust type)
let change = SourceChange::new(/* ... */);

// SDK serializes it and wraps the bytes in an FFI envelope
#[repr(C)]
struct FfiSourceEvent {
    payload_ptr: *const u8,    // MessagePack-encoded SourceEventPayload
    payload_len: usize,
    payload_drop_fn: Option<extern "C" fn(*mut u8, usize)>,
    op: FfiChangeOp,           // routing hint mirrored from the payload
    timestamp_us: i64,         // routing hint mirrored from the payload
}

// Host deserializes into its own owned value, then frees the producer's buffer
// through payload_drop_fn.
```

> Earlier versions passed these types as opaque `Box::into_raw` pointers that the other > side reclaimed with `Box::from_raw`. That is undefined behaviour: `repr(Rust)` has no > stable layout across independently compiled cdylibs, and `bytes::Bytes` carries a > `&'static` vtable pointer valid only in the producing module. It caused > non-deterministic heap corruption. See issue #602, the module docs in > `drasi-core/components/plugin-sdk/src/ffi/payload.rs`, and the `0.10.0` entry in > `metadata.rs`. Do not reintroduce that pattern for payloads.

### Vtable Pattern

Component interactions use `#[repr(C)]` vtable structs (tables of function pointers):

```rust
#[repr(C)]
pub struct SourceVtable {
    pub state: *mut c_void,           // Plugin's concrete state
    pub id_fn: extern "C" fn(*const c_void) -> FfiStr,
    pub start_fn: extern "C" fn(*mut c_void) -> FfiResult,
    pub stop_fn: extern "C" fn(*mut c_void) -> FfiResult,
    pub status_fn: extern "C" fn(*const c_void) -> FfiComponentStatus,
    pub subscribe_fn: extern "C" fn(...) -> *mut FfiSubscriptionResponse,
    pub drop_fn: extern "C" fn(*mut c_void),
    // ... more methods
}
```

The host wraps each vtable in a proxy type that implements the real DrasiLib trait:

```rust
// Host-side proxy (in drasi-host-sdk)
impl Source for SourceProxy {
    async fn start(&self) -> Result<()> {
        let result = (self.vtable.start_fn)(self.vtable.state);
        result.into_result()
    }
    // ...
}
```

### Cross-cdylib Ownership Contract (Shared System Allocator)

A number of `#[repr(C)]` structures are transferred across the cdylib boundary as owned `Box`es: the producing side calls `Box::into_raw`, and the consuming side reclaims and frees them with `Box::from_raw`. This happens in **both directions** and covers the event envelopes (`FfiSourceEvent`), subscription response structures, the vtables themselves, and the plugin registration structs. Event payloads are excluded: they cross as serialized bytes per the payload rule above, and only their `#[repr(C)]` envelope is boxed.

For this to be sound, the allocator that produced a pointer on one side must be ABI-compatible with the `dealloc` performed on the other side. This holds **only** because no crate in the workspace sets `#[global_allocator]`: every allocation routes through the default **System** allocator, which is process-global `libc` `malloc`/`free` on all supported targets.

> **Requirement:** Plugins and the host **must** share the default System allocator. > A plugin (or the host) that installs a custom global allocator — `jemalloc`, > `mimalloc`, `tcmalloc`, `snmalloc`, etc., via `#[global_allocator]` — turns every > cross-boundary `Box::from_raw` into **silent heap corruption**. The failure mode is > arbitrary corruption, not a clean crash.

This constraint is enforced at build time by a `cargo-deny` `[bans]` rule (see the workspace `deny.toml` and `.github/workflows/cargo-deny.yml`) that rejects any crate pulling in a known custom-allocator crate transitively. See issue [#378](https://github.com/drasi-project/drasi-core/issues/378) for background.

### Reverse Vtables (Host → Plugin)

Some services flow from host to plugin:

- **StateStoreProvider**: Host owns the state store, plugins access it via `StateStoreVtable`
- **BootstrapProvider**: Bootstrap plugin A → host → source plugin B (mediated via `BootstrapProviderVtable`)

The host builds a vtable from its own trait implementation and passes it to the plugin, which wraps it in a local proxy (`FfiStateStoreProxy`, `FfiBootstrapProviderProxy`).

State-store reads preserve the difference between a missing key and a failed
read, including a provider panic. The existing ABI's boolean slots conflate
absence and errors, so the proxy uses the error-capable get/count operations for
existence checks and the counted batch operation for deletion results. These
corrections do not change ABI 0.15 layouts. Complete list-error reporting,
durability forwarding and persistent service-vtable lifetime ownership remain
versioned-interface work, tracked by the
[replacement requirement ledger](../lib/tests/runtime_parity/requirements.tsv).

## Runtime Model

### Multiple Tokio Runtimes

Each cdylib plugin runs its own tokio runtime, initialized during `drasi_plugin_init()`. The host also has its own runtime. This means:

- No tokio version coupling between host and plugins
- No shared thread pools
- Plugins can use any tokio features independently
- ~20µs FFI overhead per async call (negligible)

### Async → Sync Bridge

DrasiLib traits have async methods, but FFI vtable functions must be `extern "C"` (sync). The SDK bridges this with `std::thread::spawn` + `block_on`:

```
Host calls vtable.start_fn(state)
  → Plugin spawns OS thread
  → OS thread calls runtime.block_on(source.start())
  → Async work completes on plugin's tokio runtime
  → OS thread returns FfiResult
  → Host receives result
```

This avoids nesting tokio runtimes (which would panic) by running `block_on` on a fresh OS thread.

## Logging and Tracing

Plugin source/reaction wrappers own a bounded status-channel forwarder rather
than retaining an unread 16-entry receiver. Normal lifecycle operations keep
their existing callbacks; background error updates also reach the instance
callback. Dropping a wrapper revokes that callback before stopping its
forwarder. The host callback still logs and rejects an update when its observer
queue is full; this is not a lossless lifecycle-event delivery guarantee.

### Plugin → Host Log Bridge

Plugins use standard `log::info!()` / `log::error!()` macros. The `export_plugin!` macro installs an `FfiLogger` that forwards all log records to the host via a callback:

```
Plugin: log::info!("connected")
  → FfiLogger.log(record)
  → Serialize to FfiLogEntry { level, plugin_id, message }
  → Call host_log_callback(entry_ptr)
  → Host: log::log!(level, "[plugin:{}] {}", id, message)
```

The `tracing` crate's `log` feature causes tracing events to fall back to `log` records when no tracing subscriber is set in the plugin's cdylib, so both logging frameworks work.

## Plugin Development Guide

### Creating a New Source Plugin

1. **Create crate** with `crate-type = ["lib", "cdylib"]`:

```toml
# Cargo.toml
[package]
name = "drasi-source-mydb"

[lib]
crate-type = ["lib", "cdylib"]

[features]
dynamic-plugin = []

[dependencies]
drasi-lib = { workspace = true }
drasi-plugin-sdk = { workspace = true }
# ... your deps
```

2. **Implement the Source trait** (same code for static and dynamic):

```rust
use drasi_lib::sources::Source;

pub struct MyDbSource { /* ... */ }

#[async_trait]
impl Source for MyDbSource {
    fn id(&self) -> &str { &self.id }
    fn type_name(&self) -> &str { "mydb" }
    async fn start(&self) -> anyhow::Result<()> { /* connect */ }
    async fn stop(&self) -> anyhow::Result<()> { /* disconnect */ }
    async fn status(&self) -> ComponentStatus { /* ... */ }
    async fn subscribe(&self, settings: SourceSubscriptionSettings)
        -> anyhow::Result<SubscriptionResponse> { /* ... */ }
    // ...
}
```

3. **Implement the descriptor** (factory + config schema):

```rust
use drasi_plugin_sdk::descriptor::SourcePluginDescriptor;

pub struct MyDbSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for MyDbSourceDescriptor {
    fn kind(&self) -> &str { "mydb" }
    fn config_version(&self) -> &str { "1.0.0" }
    fn config_schema_name(&self) -> &str { "MyDbSourceConfig" }
    fn config_schema_json(&self) -> String {
        // Return utoipa OpenAPI schema as JSON
    }
    async fn create_source(&self, id: &str, config: &Value, auto_start: bool)
        -> anyhow::Result<Box<dyn Source>> {
        let config: MyDbConfig = serde_json::from_value(config.clone())?;
        Ok(Box::new(MyDbSource::new(id, config, auto_start)))
    }
}
```

4. **Register with `export_plugin!`**:

```rust
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "mydb-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [MyDbSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
```

5. **Build**:

```sh
# Static (linked into server binary):
cargo build  # with drasi-server's builtin-plugins feature

# Dynamic (standalone .so):
cargo build --lib -p drasi-source-mydb --features drasi-source-mydb/dynamic-plugin
```

### Creating a Reaction Plugin

Same pattern as source, but implement `Reaction` trait and `ReactionPluginDescriptor`:

```rust
use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::descriptor::ReactionPluginDescriptor;

pub struct MyReaction { /* ... */ }

#[async_trait]
impl Reaction for MyReaction {
    fn id(&self) -> &str { &self.id }
    fn type_name(&self) -> &str { "myreaction" }
    fn query_ids(&self) -> Vec<String> { self.query_ids.clone() }
    async fn start(&self) -> anyhow::Result<()> { /* ... */ }
    async fn stop(&self) -> anyhow::Result<()> { /* ... */ }
    // ...
}

pub struct MyReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for MyReactionDescriptor {
    fn kind(&self) -> &str { "myreaction" }
    async fn create_reaction(&self, id: &str, query_ids: Vec<String>,
        config: &Value, auto_start: bool) -> anyhow::Result<Box<dyn Reaction>> {
        // ...
    }
}
```

## OpenAPI Schema Flow

Plugin DTOs serve double duty: serde deserialization AND OpenAPI schema generation. The schema crosses the FFI boundary as a JSON string.

```
Plugin                              Host
──────                              ────
#[derive(utoipa::ToSchema)]
struct MyConfig { ... }
        ↓
config_schema_json() → JSON string  →  Parse JSON
                                        ↓
                                    Merge into global OpenAPI spec
                                        ↓
                                    Build OneOf union of all plugin configs
                                        ↓
                                    /api/v1/openapi.json
```

## Build System

### Static Build (default)

```sh
cargo build                    # debug
cargo build --release          # release
```

All plugins are statically linked. No `.so` files needed.

### Dynamic Build

```sh
make build-dynamic             # build server + all plugins (debug)
make build-dynamic-release     # build server + all plugins (release)
make build-dynamic-server      # server only
make build-dynamic-plugins     # plugins only
```

Plugins are built individually (not in batch) to avoid feature unification issues where adaptive plugins inherit cdylib entry points from their base dependencies.

### Testing

```sh
# Static tests
cargo test --lib                                                    # 196 tests
cargo test --test error_resilience_test                             # 11 tests

# Dynamic tests (requires: make build-dynamic-plugins)
cargo test --no-default-features --features dynamic-plugins --lib   # 185 tests

# Host-SDK unit and actual-library integration tests, including FFI recovery
cd ../drasi-core && make test-host-sdk

# Smoke tests (builds and runs server with all plugins)
make test-smoke
```

## FAQ

### Why cdylib instead of dylib?

The original `dylib` approach required:
- Identical Rust compiler version (symbol hashes must match)
- Shared runtime loaded with `RTLD_GLOBAL`
- Single-invocation build (to prevent symbol hash mismatches)
- `libstd-*.so` copying alongside plugins

The `cdylib` approach eliminates all of these constraints. Each plugin is fully self-contained with a stable C ABI boundary.

### Can plugins use different tokio versions?

Yes. Each cdylib plugin statically links its own tokio. Nothing that crosses the boundary depends on tokio's layout: payloads are serialized, and the `#[repr(C)]` envelopes and vtables have a stable layout whose compatibility is checked at load time via `FFI_SDK_VERSION`. Missing or null metadata is rejected; see `loader.rs`.

### What happens if a plugin panics?

All FFI entry points are wrapped in `std::panic::catch_unwind` by the `export_plugin!` macro. A panic in a plugin is caught and converted to an `FfiResult::Err` — the host receives an error message instead of undefined behavior.

### Can I debug plugins?

Yes. Since cdylib plugins are standard shared libraries, you can:
- Use `RUST_LOG=debug` for log output
- Attach gdb/lldb to the host process (plugin code is in the .so)
- Use `nm -D plugin.so` to verify exported symbols
- Use `ldd plugin.so` to check dependencies

### How do I test a plugin in isolation?

Plugins implement the same DrasiLib traits for both static and dynamic builds. Write unit tests against the trait implementation directly (no FFI needed):

```rust
#[tokio::test]
async fn test_my_source() {
    let source = MyDbSource::new("test", config);
    source.start().await.unwrap();
    assert_eq!(source.status().await, ComponentStatus::Running);
}
```

For integration testing of the FFI layer, see the host-sdk integration tests at `drasi-core/components/host-sdk/tests/integration_test.rs`.
