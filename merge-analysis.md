# Merge Analysis: `ui` → `main-with-ui`

## Executive Summary

The `ui` branch and the `main-with-ui` branch have diverged significantly from their common ancestor (commit `38a66f1` — "MSSQL Source"). Both branches introduce sweeping architectural changes to drasi-lib that overlap in critical areas. Merging these branches requires careful reconciliation of two parallel—but complementary—visions for component lifecycle management, status reporting, and runtime context injection.

**Key Insight**: The two branches are not in conflict at the *goal* level—they are complementary. The `ui` branch provides the **centralized state management layer** (ComponentGraph) while `main-with-ui` provides the **plugin boundary layer** (FFI proxies, dynamic loading). The challenge is that both branches independently redesigned the *plumbing* between components and the host, so the plumbing must be unified.

---

## Branch Characterization

### `ui` Branch — ComponentGraph & Introspection

The `ui` branch introduces a **ComponentGraph** as the single source of truth for all component state and relationships. Key features:

| Feature | Description |
|---------|-------------|
| **ComponentGraph** | `petgraph`-based directed graph with `ComponentNode` entries, bidirectional `RelationshipKind` edges, and O(1) lookup by component ID |
| **ComponentStatusHandle** | Clonable status bridge: components call `set_status()` which updates local state + sends to graph via `mpsc` channel (fire-and-forget) |
| **Two-Phase Init** | Status handles created unwired in `new()`, then wired to graph in `initialize()` via `ComponentUpdateSender` |
| **Graph Update Loop** | Single consumer of `ComponentUpdate` messages; applies updates to graph under write lock, emits `ComponentEvent` via broadcast |
| **ComponentGraphSource** | Built-in introspection source that translates graph events into Cypher-queryable graph data (DrasiInstance, Source, Query, Reaction nodes) |
| **ComponentGraphBootstrapProvider** | Atomic snapshot bootstrap for the introspection source |
| **Registry-First Pattern** | Components are registered in the graph *before* runtime instances are created |
| **Broadcast Events** | `ComponentEvent` distributed via `broadcast::Sender` (one-to-many) — replaces the old `mpsc` ComponentEventSender/Receiver pair |
| **Added/Removed Status** | New `ComponentStatus::Added` and `ComponentStatus::Removed` variants for tracking full lifecycle |
| **QueryProvider Trait** | Reactions manage their own query subscriptions via injected `QueryProvider` (no more host-managed `enqueue_query_result`) |
| **No `enqueue_query_result`** | Removed from `Reaction` trait — reactions subscribe to queries themselves |

### `main-with-ui` Branch — Plugin Framework & FFI Proxies

The `main-with-ui` branch introduces a **plugin framework** with stable C ABI boundaries. Key features:

| Feature | Description |
|---------|-------------|
| **Plugin SDK** (`plugin-sdk`) | Descriptor traits (`SourcePluginDescriptor`, `ReactionPluginDescriptor`, `BootstrapPluginDescriptor`), registration, and `export_plugin!` macro |
| **Host SDK** (`host-sdk`) | `PluginLoader` for dynamic `.so`/`.dylib` loading, proxy types that wrap FFI vtables |
| **FFI Primitives** (`ffi-primitives`) | `FfiStr`, `FfiOwnedStr`, `FfiResult`, `FfiGetResult`, `SendPtr<T>` for safe cross-boundary data |
| **SourceProxy** | Wraps `SourceVtable`; implements `Source` trait; bridges async→sync via `std::thread::spawn` |
| **ReactionProxy** | Wraps `ReactionVtable`; implements `Reaction` trait; uses push-based `SyncSender<QueryResult>` for result delivery |
| **ChangeReceiverProxy** | Push-based change receiver using `std::sync::mpsc` + `tokio::sync::Notify` |
| **BootstrapProviderProxy** | Bridges bootstrap across plugin boundary with `FfiBootstrapSender` |
| **Lifecycle Callbacks** | `instance_lifecycle_callback` sends `ComponentEvent` via `ComponentEventSender` (mpsc) |
| **Log Callbacks** | `instance_log_callback` routes logs through `ComponentLogRegistry` |
| **InstanceCallbackContext** | Per-instance context with `event_tx: ComponentEventSender` for routing lifecycle events |
| **State Store Bridge** | `StateStoreVtable` wrapping host's `StateStoreProvider` for cross-FFI state access |
| **Host-managed subscriptions** | `ReactionManager` manages query→reaction subscriptions; calls `enqueue_query_result()` |
| **Identity Providers** | `IdentityProvider` trait and bridges for AWS/Azure credential injection |
| **Index Plugins** | RocksDB and Garnet storage backends as loadable plugins |
| **Cross-compilation** | Dockerfiles for GNU/musl/Windows targets |

---

## Conflict Analysis

### Critical Conflicts (Structural Redesigns)

These files have been independently redesigned on both branches and cannot be auto-merged:

#### 1. `lib/src/channels/events.rs` — **HARD**

| Aspect | `ui` | `main-with-ui` |
|--------|------|----------------|
| `ComponentEventSender/Receiver` | **Removed** (replaced by `ComponentEventBroadcastSender/Receiver` using `broadcast::Sender`) | **Retained** and used extensively (mpsc channel) |
| `ComponentStatus` enum | Adds `Added`, `Removed` variants | Same as merge-base |
| `SourceControl::FuturesDue` | **Removed** | Present |
| `EventChannels` struct | Removes `component_event_tx` field | Retains `component_event_tx` field |

**Resolution**: The merged version needs **both** channel types:
- The `broadcast::Sender<ComponentEvent>` (from `ui`) for the ComponentGraph → external subscribers flow
- The `mpsc::Sender<ComponentUpdate>` (from `ui`) for components → graph loop flow  
- The existing `ComponentEventSender` (mpsc from `main-with-ui`) is superseded by the ComponentGraph's `ComponentUpdateSender`, but the FFI callbacks currently write to `ComponentEventSender`. These callbacks must be adapted to write to `ComponentUpdateSender` instead.

#### 2. `lib/src/context/mod.rs` — **HARD**

| Aspect | `ui` | `main-with-ui` |
|--------|------|----------------|
| `SourceRuntimeContext` fields | `instance_id`, `source_id`, `state_store`, **`update_tx: ComponentUpdateSender`** | `instance_id`, `source_id`, **`status_tx: ComponentEventSender`**, `state_store`, **`identity_provider`** |
| `ReactionRuntimeContext` fields | adds **`query_provider: Arc<dyn QueryProvider>`**, **`update_tx`** | has **`status_tx`**, **`identity_provider`** |
| New type | **`QueryRuntimeContext`** (new) | Not present |

**Resolution**: Merged contexts need:
- `update_tx: ComponentUpdateSender` (ui's approach — fire-and-forget to graph loop)
- `identity_provider: Option<Arc<dyn IdentityProvider>>` (main-with-ui's addition)
- `query_provider: Arc<dyn QueryProvider>` on `ReactionRuntimeContext` (ui's addition)
- `QueryRuntimeContext` (ui's addition)
- **Remove** `status_tx: ComponentEventSender` — replaced by `update_tx`

#### 3. `lib/src/lib_core.rs` — **HARD**

| Aspect | `ui` | `main-with-ui` |
|--------|------|----------------|
| `DrasiLib` fields | Adds `component_graph: Arc<RwLock<ComponentGraph>>`, `component_event_broadcast_tx` | Has `state_guard`, `middleware_registry`, no graph |
| Construction | Creates graph, spawns update loop, passes `update_tx` to managers | Creates managers with `event_tx` from EventChannels |
| Public API | `subscribe_all_component_events()`, `get_graph()` | Builder pattern integration |

**Resolution**: Merged version needs all of the above — ComponentGraph ownership, update loop, AND the builder pattern + state guard.

#### 4. `lib/src/sources/base.rs` — **HARD**

| Aspect | `ui` | `main-with-ui` |
|--------|------|----------------|
| Status mechanism | `ComponentStatusHandle` (fire-and-forget to graph) | `Arc<RwLock<ComponentStatus>>` + direct status methods |
| `initialize()` | Wires status handle via `context.update_tx` | Stores context for later use, uses `status_tx` |
| `clone_shared()` | New method for spawned tasks | Not present |
| `status_handle()` | Returns clonable handle | Not present |

**Resolution**: Adopt `ui`'s `ComponentStatusHandle` approach. Adapt `main-with-ui`'s `SourceProxy.initialize()` to pass `update_tx` instead of `status_tx`.

#### 5. `lib/src/reactions/common/base.rs` — **HARD**

| Aspect | `ui` | `main-with-ui` |
|--------|------|----------------|
| Status mechanism | `ComponentStatusHandle` | `Arc<RwLock<ComponentStatus>>` |
| Query subscription | Self-managed via `QueryProvider` + `subscribe_to_queries()` | Host-managed via `enqueue_query_result()` |
| Priority queue | Still present, fed by reaction's own subscription tasks | Fed by host's forwarding tasks |

**Resolution**: This is the most complex conflict. The `ui` branch's approach (reactions self-manage subscriptions) is fundamentally different from `main-with-ui`'s approach (host manages subscriptions). Since the `ui` branch removes `enqueue_query_result()` from the `Reaction` trait entirely, the `ReactionProxy`'s push-based result channel becomes incompatible. See detailed resolution below.

#### 6. `lib/src/reactions/traits.rs` — **HARD**

The `ui` branch:
- Removes `enqueue_query_result()` from `Reaction` trait
- Adds `QueryProvider` trait
- Changes subscription model docs

**Resolution**: Must decide whether reactions self-subscribe (ui) or host-subscribes (main-with-ui). For FFI plugins, host-managed subscription may be more practical since plugins can't easily get Arc references to query instances across the FFI boundary. See [Design Decision 1](#design-decision-1-reaction-subscription-model) below.

#### 7. `lib/src/sources/manager.rs` — **HARD**

Both branches restructured `SourceManager`:
- `ui`: Adds `graph: Arc<RwLock<ComponentGraph>>`, `update_tx`, registry-first pattern
- `main-with-ui`: Adds `event_tx: ComponentEventSender`, `event_history`, `log_registry`

**Resolution**: Merge both. The registry-first pattern from `ui` plus `event_history`/`log_registry` from `main-with-ui`.

#### 8. `lib/src/reactions/manager.rs` — **HARD**

- `ui`: Self-subscription model, graph integration
- `main-with-ui`: Host-managed subscription with forwarding tasks, `subscription_tasks` tracking

**Resolution**: Depends on [Design Decision 1](#design-decision-1-reaction-subscription-model).

#### 9. `lib/src/queries/manager.rs` — **HARD**

Both branches made extensive changes but both converge on the same general pattern. Need line-by-line reconciliation.

#### 10. `lib/src/queries/base.rs` — **MEDIUM**

- `ui`: Adds `ComponentStatusHandle`, `initialize()`, `dispatch_query_result()`
- `main-with-ui`: Has `event_tx: ComponentEventSender`, `task_handle`

**Resolution**: Adopt `ui`'s status handle, keep `main-with-ui`'s `task_handle`.

### Moderate Conflicts

#### 11. `lib/src/lifecycle.rs` — **MEDIUM**
Both branches modified but the changes are compatible. Need to merge lifecycle manager with both graph awareness and plugin support.

#### 12. `lib/src/lib.rs` — **MEDIUM**
New public exports from both branches. Straightforward additive merge plus adjusting import paths.

#### 13. `lib/src/builder.rs` — **LOW-MEDIUM**
Only on `ui` branch. Can be taken as-is but needs to integrate with `main-with-ui`'s `StateGuard` and `MiddlewareTypeRegistry`.

#### 14. `lib/Cargo.toml` — **LOW**
Both branches add dependencies. Straightforward merge.

#### 15. `lib/src/sources/future_queue_source.rs` — **MEDIUM**
Both branches modified. `ui` removes `FuturesDue` control signal, `main-with-ui` has other changes. Need reconciliation.

### Low-Conflict / Additive Files

#### Files unique to `ui` (adopt directly):
- `lib/src/component_graph.rs` — New file, no conflict
- `lib/src/sources/component_graph_source.rs` — New file, no conflict
- `lib/src/bootstrap/component_graph.rs` — New file, no conflict
- `lib/src/lib_core_ops/graph_ops.rs` — New file, no conflict

#### Files unique to `main-with-ui` (adopt directly, ~112 files):
- Entire `components/host-sdk/` crate
- Entire `components/plugin-sdk/` crate
- Entire `components/ffi-primitives/` crate
- All bootstrapper `descriptor.rs` files
- All component plugin restructuring
- Cross-compilation Dockerfiles
- GitHub agent/workflow files
- Index plugin crates (`components/indexes/garnet/`, `components/indexes/rocksdb/`)
- State store plugins

#### Component sources/reactions (both branches modified):
These files were modified on both branches but the changes are mostly independent refactors within each component. They primarily need to adapt to whichever trait signatures the merged code adopts.

---

## Key Design Decisions

### Design Decision 1: Reaction Subscription Model

**The Problem**: The `ui` branch removes `enqueue_query_result()` from the `Reaction` trait and has reactions self-subscribe to queries via `QueryProvider`. The `main-with-ui` branch has the host manage subscriptions and push results via `enqueue_query_result()`. The `ReactionProxy` (FFI) uses a push-based `SyncSender<QueryResult>` channel that maps directly to `enqueue_query_result()`.

**Recommendation**: **Hybrid approach** — keep `enqueue_query_result()` on the trait but make it optional/default-implemented. Native (non-FFI) reactions can self-subscribe using `QueryProvider`. FFI reactions (`ReactionProxy`) use host-managed subscriptions with `enqueue_query_result()`.

Rationale:
- FFI reactions **cannot** easily get `Arc<dyn Query>` references across the FFI boundary
- Native reactions benefit from self-subscription (simpler, the reaction knows what it needs)
- The `QueryProvider` approach is cleaner for the common case
- `ReactionProxy` already has the push-based plumbing working

Implementation:
```rust
#[async_trait]
pub trait Reaction: Send + Sync {
    // ... existing methods ...

    /// Whether this reaction manages its own query subscriptions.
    /// Default: true (reaction self-subscribes via QueryProvider).
    /// FFI proxies return false (host manages subscriptions).
    fn self_manages_subscriptions(&self) -> bool { true }

    /// Enqueue a query result for processing (host-managed subscription path).
    /// Only called when self_manages_subscriptions() returns false.
    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        Err(anyhow::anyhow!("Not supported: this reaction self-manages subscriptions"))
    }
}
```

### Design Decision 2: Status Channel Unification

**The Problem**: `ui` uses `ComponentUpdateSender` (mpsc to graph loop) while `main-with-ui` uses `ComponentEventSender` (mpsc directly). FFI lifecycle callbacks currently write to `ComponentEventSender`.

**Recommendation**: Adopt `ui`'s `ComponentUpdateSender` → graph loop → `broadcast::Sender<ComponentEvent>` architecture. Adapt `InstanceCallbackContext` to hold `ComponentUpdateSender` instead of `ComponentEventSender`.

This means:
- `InstanceCallbackContext.event_tx` changes type from `ComponentEventSender` to `ComponentUpdateSender`
- `instance_lifecycle_callback` sends `ComponentUpdate::Status { ... }` instead of `ComponentEvent { ... }`
- The graph loop becomes the sole event emitter — single point of truth

### Design Decision 3: Context Field Unification

**Recommendation**: Merge both branches' context fields:

```rust
pub struct SourceRuntimeContext {
    pub instance_id: String,
    pub source_id: String,
    pub state_store: Option<Arc<dyn StateStoreProvider>>,
    pub update_tx: ComponentUpdateSender,                          // from ui
    pub identity_provider: Option<Arc<dyn IdentityProvider>>,     // from main-with-ui
}

pub struct ReactionRuntimeContext {
    pub instance_id: String,
    pub reaction_id: String,
    pub state_store: Option<Arc<dyn StateStoreProvider>>,
    pub update_tx: ComponentUpdateSender,                          // from ui
    pub query_provider: Arc<dyn QueryProvider>,                   // from ui
    pub identity_provider: Option<Arc<dyn IdentityProvider>>,     // from main-with-ui
}

pub struct QueryRuntimeContext {                                   // from ui
    pub instance_id: String,
    pub query_id: String,
    pub update_tx: ComponentUpdateSender,
}
```

---

## Merge Strategy

### Recommended Approach: **Merge `ui` into `main-with-ui`** with staged conflict resolution

Rather than a single big-bang merge, resolve conflicts in stages:

### Phase 1: Foundation Layer (No Functional Changes)

**Goal**: Bring in the `ui` branch's new types and files without modifying existing plumbing.

1. **Cherry-pick new files from `ui`** (no conflicts):
   - `lib/src/component_graph.rs`
   - `lib/src/sources/component_graph_source.rs`
   - `lib/src/bootstrap/component_graph.rs`
   - `lib/src/lib_core_ops/graph_ops.rs`
   - `lib/src/lib_core_ops/mod.rs`

2. **Add new `ComponentStatus` variants** to `channels/events.rs`:
   - Add `Added`, `Removed` to `ComponentStatus` enum
   - Add `ComponentUpdateSender`/`ComponentUpdateReceiver` types
   - Add `ComponentEventBroadcastSender`/`ComponentEventBroadcastReceiver` types
   - **Keep** existing `ComponentEventSender`/`ComponentEventReceiver` temporarily

3. **Add `ComponentStatusHandle`** (from `component_graph.rs`) — standalone type, no integration yet.

4. **Update `lib/Cargo.toml`**: Add `petgraph` and `chrono` dependencies (from ui).

5. **Update `lib/src/lib.rs`**: Add new module declarations (`component_graph`, `lib_core_ops`, `bootstrap::component_graph`). Add new public exports.

**Validation**: `cargo build -p drasi-lib` compiles. All existing tests pass.

### Phase 2: Context Unification

**Goal**: Merge the runtime context types so both branches' plumbing can coexist.

1. **Merge `context/mod.rs`**:
   - Add `update_tx: ComponentUpdateSender` to `SourceRuntimeContext` and `ReactionRuntimeContext`
   - Keep `status_tx: ComponentEventSender` temporarily (for backward compat)
   - Add `query_provider` to `ReactionRuntimeContext`
   - Add `QueryRuntimeContext`

2. **Update `SourceBase`**: Add `ComponentStatusHandle` alongside existing status. Wire handle in `initialize()` when `update_tx` is available.

3. **Update `ReactionBase`**: Same pattern — add handle, keep old status mechanism working.

4. **Update `QueryBase`**: Add handle.

**Validation**: All existing tests still pass. Status flows through both old and new paths.

### Phase 3: Graph Integration

**Goal**: Wire ComponentGraph into DrasiLib and managers.

1. **Update `DrasiLib`** (`lib_core.rs`):
   - Add `component_graph: Arc<RwLock<ComponentGraph>>`
   - Add `component_event_broadcast_tx`
   - Spawn graph update loop in constructor
   - Pass `update_tx` to managers
   - Add `subscribe_all_component_events()` and `get_graph()` APIs

2. **Update `SourceManager`**:
   - Add `graph: Arc<RwLock<ComponentGraph>>` and `update_tx`
   - Implement registry-first pattern in `add_source()`
   - Keep `event_history` and `log_registry` from main-with-ui

3. **Update `QueryManager`**:
   - Same graph integration
   - Keep existing subscription machinery

4. **Update `ReactionManager`**:
   - Add graph integration
   - Keep host-managed subscription for `ReactionProxy`
   - Add `QueryProvider` support for native reactions

5. **Register `ComponentGraphSource`** during DrasiLib construction.

**Validation**: Graph is populated when components are added. Status updates flow through graph. `cargo test -p drasi-lib` passes.

### Phase 4: Callback Adaptation

**Goal**: Route FFI plugin callbacks through the ComponentGraph.

1. **Update `InstanceCallbackContext`** in `host-sdk/src/callbacks.rs`:
   - Change `event_tx: ComponentEventSender` → `update_tx: ComponentUpdateSender`
   - Update `instance_lifecycle_callback` to send `ComponentUpdate::Status` instead of `ComponentEvent`

2. **Update `SourceProxy.initialize()`** in `host-sdk/src/proxies/source.rs`:
   - Pass `context.update_tx` instead of `context.status_tx` to callback context

3. **Update `ReactionProxy.initialize()`** in `host-sdk/src/proxies/reaction.rs`:
   - Same change

4. **Handle `enqueue_query_result()` for `ReactionProxy`**:
   - Keep `enqueue_query_result()` on the `Reaction` trait (with default impl)
   - `ReactionProxy` returns `false` from `self_manages_subscriptions()`
   - `ReactionManager` checks this flag to decide subscription strategy

**Validation**: Plugin lifecycle events appear in ComponentGraph. FFI reaction results still flow correctly.

### Phase 5: Cleanup & Remove Old Plumbing

**Goal**: Remove deprecated status channels and unify on the ComponentGraph path.

1. **Remove `ComponentEventSender`/`ComponentEventReceiver`** from `channels/events.rs`
2. **Remove `status_tx` field** from runtime contexts
3. **Remove `component_event_tx`** from `EventChannels`
4. **Remove `SourceControl::FuturesDue`** (ui branch removes it; verify FutureQueueSource doesn't need it)
5. **Update all tests** to use new status reporting path
6. **Update integration tests** (`lib-integration-tests/`)
7. **Update component source/reaction implementations** to use `ComponentStatusHandle`

**Validation**: Full `cargo test` passes. `cargo clippy` clean. No dead code warnings.

### Phase 6: Builder & Polish

**Goal**: Integrate the builder pattern and finalize public API.

1. **Merge `builder.rs`** with main-with-ui's `StateGuard` and `MiddlewareTypeRegistry`
2. **Update examples** and documentation
3. **Update `query-perf`** benchmark tool

---

## File-by-File Merge Guide

### Files to take from `ui` branch directly (new, no conflict):
| File | Action |
|------|--------|
| `lib/src/component_graph.rs` | Copy from ui |
| `lib/src/sources/component_graph_source.rs` | Copy from ui |
| `lib/src/bootstrap/component_graph.rs` | Copy from ui |
| `lib/src/lib_core_ops/graph_ops.rs` | Copy from ui |
| `lib/src/lib_core_ops/mod.rs` | Copy from ui |

### Files to take from `main-with-ui` directly (new, no conflict):
| File | Action |
|------|--------|
| `components/host-sdk/**` | Keep as-is (112+ files) |
| `components/plugin-sdk/**` | Keep as-is |
| `components/ffi-primitives/**` | Keep as-is |
| `components/indexes/**` | Keep as-is |
| `components/state_stores/**` | Keep as-is |
| `components/bootstrappers/*/descriptor.rs` | Keep as-is |
| All CI/cross-compilation files | Keep as-is |

### Files requiring manual merge:
| File | Difficulty | Primary Source | Adaptations Needed |
|------|-----------|---------------|-------------------|
| `lib/src/channels/events.rs` | Hard | Start from ui, add back main-with-ui types temporarily | Phase 1 & 5 |
| `lib/src/context/mod.rs` | Hard | Merge both | See Design Decision 3 |
| `lib/src/lib_core.rs` | Hard | Start from main-with-ui, add graph from ui | Phase 3 |
| `lib/src/sources/base.rs` | Hard | Start from ui, ensure proxy compat | Phase 2 & 4 |
| `lib/src/reactions/common/base.rs` | Hard | Start from ui, keep enqueue path | Phase 2 & 4 |
| `lib/src/reactions/traits.rs` | Hard | Merge both | See Design Decision 1 |
| `lib/src/reactions/manager.rs` | Hard | Start from main-with-ui, add graph + hybrid subscription | Phase 3 & 4 |
| `lib/src/sources/manager.rs` | Hard | Start from ui, add event_history/log_registry | Phase 3 |
| `lib/src/queries/manager.rs` | Hard | Merge both | Phase 3 |
| `lib/src/queries/base.rs` | Medium | Start from ui, keep task_handle | Phase 2 |
| `lib/src/lifecycle.rs` | Medium | Merge both | Phase 3 |
| `lib/src/lib.rs` | Medium | Merge both exports | Phase 1 |
| `lib/src/builder.rs` | Medium | Start from ui, add StateGuard | Phase 6 |
| `lib/Cargo.toml` | Low | Merge dependencies | Phase 1 |
| `lib/src/sources/future_queue_source.rs` | Medium | Merge both | Phase 5 |
| `lib/src/sources/mod.rs` | Low | Merge both | Phase 1 |
| `lib/src/sources/tests.rs` | Medium | Adapt to new APIs | Phase 5 |
| `lib/src/queries/tests.rs` | Medium | Adapt to new APIs | Phase 5 |
| `lib/src/reactions/tests.rs` | Medium | Adapt to new APIs | Phase 5 |

### Component files needing trait adaptation:
These files implement `Source` or `Reaction` traits and need to match the merged trait signatures:

| File | Change Needed |
|------|--------------|
| `components/sources/*/src/lib.rs` (or `mock.rs`) | Adapt to merged `Source` trait |
| `components/reactions/*/src/*.rs` | Adapt to merged `Reaction` trait |
| `components/host-sdk/src/proxies/source.rs` | Use `update_tx` instead of `status_tx` |
| `components/host-sdk/src/proxies/reaction.rs` | Use `update_tx`, handle hybrid subscription |
| `components/host-sdk/src/callbacks.rs` | `InstanceCallbackContext` uses `ComponentUpdateSender` |

---

## Risk Assessment

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|-----------|
| Deadlock from graph lock contention | Medium | High | Graph update loop pattern from ui (single writer) mitigates this. Keep write lock scope minimal. |
| FFI callback sending wrong channel type | High | Medium | Type system will catch this at compile time if we change `InstanceCallbackContext.event_tx` type |
| Reaction subscription regression | Medium | High | Comprehensive integration tests for both native and FFI reactions |
| `FutureQueueSource` breakage from `FuturesDue` removal | Low | Medium | Verify FutureQueueSource doesn't rely on control signal (ui branch already validated this) |
| Test suite breakage | High | Low | Expected; methodical adaptation phase handles this |
| Bootstrap provider compatibility | Low | Low | `BootstrapProviderProxy` trait signature unchanged |

---

## Estimated Scope

| Phase | Files Modified | New Files | Complexity |
|-------|---------------|-----------|-----------|
| Phase 1: Foundation | ~5 | 5 (from ui) | Low |
| Phase 2: Context | ~6 | 0 | Medium |
| Phase 3: Graph Integration | ~8 | 0 | High |
| Phase 4: Callback Adaptation | ~4 | 0 | Medium |
| Phase 5: Cleanup | ~15 | 0 | Medium |
| Phase 6: Builder & Polish | ~5 | 0 | Low |
| **Total** | **~43** | **5** | |

---

## Appendix A: Architecture After Merge

```
┌─────────────────────────────────────────────────────────────────┐
│  DrasiLib                                                       │
│  ├── ComponentGraph (petgraph)        ← Source of truth         │
│  │   ├── Nodes: Instance, Source, Query, Reaction               │
│  │   ├── Edges: Owns, Feeds, Subscribes, Bootstraps            │
│  │   └── Graph Update Loop (sole mpsc consumer)                 │
│  │       └── Emits broadcast::Sender<ComponentEvent>            │
│  │                                                              │
│  ├── SourceManager                                              │
│  │   ├── Registry-first (graph → init → store)                  │
│  │   ├── ComponentLogRegistry                                   │
│  │   └── ComponentEventHistory                                  │
│  │                                                              │
│  ├── QueryManager                                               │
│  │   └── Registry-first                                         │
│  │                                                              │
│  ├── ReactionManager                                            │
│  │   ├── Registry-first                                         │
│  │   ├── Native reactions: self-subscribe via QueryProvider      │
│  │   └── FFI reactions: host-managed enqueue_query_result()     │
│  │                                                              │
│  ├── LifecycleManager                                           │
│  └── ComponentGraphSource (built-in introspection)              │
│                                                                 │
│  Status Flow:                                                   │
│  Component → ComponentStatusHandle.set_status()                 │
│           → mpsc::Sender<ComponentUpdate> (fire-and-forget)     │
│           → Graph Update Loop                                   │
│           → graph.apply_update()                                │
│           → broadcast::Sender<ComponentEvent>                   │
│           → ComponentGraphSource, EventHistory, External subs   │
│                                                                 │
│  FFI Plugin Status Flow:                                        │
│  Plugin → lifecycle_callback()                                  │
│        → InstanceCallbackContext.update_tx                      │
│        → ComponentUpdateSender (same mpsc as above)             │
│        → Graph Update Loop (same path)                          │
└─────────────────────────────────────────────────────────────────┘
         ↕ stable C ABI (vtables + opaque pointers)
┌─────────────────────────────────────────────────────────────────┐
│  Plugin Shared Libraries (.so/.dylib/.dll)                      │
│  ├── Own tokio runtime                                          │
│  ├── SourcePluginDescriptor → create_source() → SourceProxy    │
│  ├── ReactionPluginDescriptor → create_reaction() → ReactionPr │
│  └── BootstrapPluginDescriptor → create_bp() → BootstrapProxy  │
└─────────────────────────────────────────────────────────────────┘
```

## Appendix B: Test Strategy

After each phase, run:
```bash
cargo build -p drasi-lib                          # Compilation check
cargo test -p drasi-lib                           # Unit tests
cargo test -p lib-integration-tests               # Integration tests
cargo test -p drasi-host-sdk                      # Plugin proxy tests
cargo clippy --all-targets --all-features          # Lint check
```

Key test scenarios to validate after full merge:
1. Native source → query → native reaction (self-subscription) 
2. Native source → query → FFI reaction (host-managed subscription)
3. FFI source → query → native reaction
4. Component status appears in ComponentGraph
5. ComponentGraphSource emits correct Cypher-queryable data
6. Plugin lifecycle callbacks route through graph
7. Component add/remove/restart cycles
8. Deprovision with cleanup
