# ComponentGraph Architecture Review

**Date:** 2026-03-18  
**Scope:** `lib/src/component_graph.rs` and its integration across the drasi-lib system  
**Objective:** Validate that the ComponentGraph can serve as the single source of truth for all components in a drasi-lib instance, identify inconsistencies, weaknesses, bottlenecks, and non-idiomatic Rust.

---

## 1. Executive Summary

The ComponentGraph is a well-designed directed graph backed by `petgraph::StableGraph` that tracks components (Sources, Queries, Reactions) and their relationships. The core data structure is sound, with good use of bidirectional relationships, a clean event broadcast system, and a fire-and-forget status update channel that decouples components from the graph lock.

However, the ComponentGraph is **not yet fully authoritative**. Several critical gaps exist between its intended role as "the single source of truth" and its current integration. The most significant issues are:

1. **Dual source of truth** for status and component listings (HashMap vs. graph)
2. **No dependency validation on deletion** (`can_remove` exists but is never called)
3. **No state transition enforcement** in the graph itself
4. **Transient `Added` status creates a dead zone** for state validation
5. **Silent edge failures** when relationship targets don't yet exist
6. **`topological_order()` and `list_by_kind()` are dead code** outside of unit tests

The following sections detail each finding with file locations, severity ratings, and recommended actions.

---

## 2. Findings

### 2.1 CRITICAL — Dual Source of Truth for Component Status

**Severity:** Critical  
**Files:** `lib/src/sources/manager.rs:259-270`, `lib/src/reactions/manager.rs:243-256`, `lib/src/queries/manager.rs:1381+`, `lib/src/inspection.rs:74-80`

**Problem:** All `get_*_status()` and `list_*()` methods read status from the runtime component instance (`source.status().await`), not from the ComponentGraph. The graph's `list_by_kind()` method exists but is never called outside unit tests.

```rust
// SourceManager::get_source_status (line 259)
pub async fn get_source_status(&self, id: String) -> Result<ComponentStatus> {
    let source = { self.sources.read().await.get(&id).cloned() };
    if let Some(source) = source {
        Ok(source.status().await)  // <-- reads from component, not graph
    } else {
        Err(anyhow::anyhow!("Source not found: {id}"))
    }
}
```

**Impact:** The ComponentGraph can drift from the runtime status. Status updates flow through the `update_tx` mpsc channel and are applied asynchronously, so during the window between a component calling `set_status()` and the graph update loop processing it, the graph and the component disagree. More importantly, the graph is never consulted for status queries — it is a write-only sink for status.

**Recommendation:** If the ComponentGraph is to be the source of truth, `list_sources/queries/reactions` and `get_*_status()` should read from the graph via `list_by_kind()` / `get_component().status`. The component-local `ComponentStatusHandle.status` should be treated as a cache for the component's own use, not the authoritative record.

---

### 2.2 CRITICAL — Deletion Does Not Validate Dependencies

**Severity:** Critical  
**Files:** `lib/src/sources/manager.rs:315-373`, `lib/src/queries/manager.rs:1474+`, `lib/src/reactions/manager.rs:282+`, `lib/src/lib_core_ops/source_ops.rs:83-107`

**Problem:** The graph provides `can_remove(id)` which checks whether a component has dependents (via `Feeds` edges), and `can_remove_component()` is exposed on `DrasiLib`. However, **none of the delete/remove methods actually call it**. A source can be deleted while queries still reference it, leaving orphaned subscriptions.

```rust
// SourceManager::delete_source — no can_remove check
pub async fn delete_source(&self, id: String, cleanup: bool) -> Result<()> {
    // ... stops source, deprovisions, removes from HashMap and graph ...
    // MISSING: graph.can_remove(&id)?
}
```

Similarly, `DrasiLib::remove_source()` calls `delete_source()` without checking `can_remove_component()`.

**Impact:** Deleting a source that feeds queries will leave queries subscribed to a non-existent source. Deleting a query that feeds reactions will leave reactions subscribed to a non-existent query. The graph relationships will be silently cleaned up by `StableGraph::remove_node()`, but the runtime subscriptions will not.

**Recommendation:** All `delete_*` / `remove_*` methods must call `can_remove()` before proceeding, or cascade-stop dependents. The graph should be the gatekeeper.

---

### 2.3 HIGH — `Added` Status Creates a State Machine Dead Zone

**Severity:** High  
**Files:** `lib/src/managers/state_validation.rs:102-106`, `lib/src/sources/manager.rs:168`, `lib/src/queries/manager.rs:1270`, `lib/src/reactions/manager.rs:133`

**Problem:** Components are added to the graph with `ComponentStatus::Added`, but the state validation logic treats `Added` as a transient event where **no operations are valid**:

```rust
// state_validation.rs:102-106
(ComponentStatus::Added, _) | (ComponentStatus::Removed, _) => {
    Err("Component is in a transient state".to_string())
}
```

However, `add_source()` immediately reads `source.status()` (which returns `Stopped` from the `ComponentStatusHandle` default), not the graph's `Added` status. So the actual component is in `Stopped` state while the graph says `Added`. The graph status is never transitioned from `Added` to `Stopped` — it jumps directly from `Added` to `Starting` when the component starts.

**Impact:** If any code path were to read status from the graph for validation (which is the stated goal), `Added` would block all operations. The `Added` → `Starting` transition is implicit and undocumented.

**Recommendation:** Either:
- (a) Remove `Added` and `Removed` from `ComponentStatus` — they are events, not states. Track them as event-only values in `ComponentEvent.status` but use a separate `ComponentState` enum for the graph node, or
- (b) Explicitly transition from `Added` → `Stopped` in the graph immediately after `add_component()`, before returning control to the manager.

---

### 2.4 HIGH — No State Transition Enforcement in the Graph

**Severity:** High  
**Files:** `lib/src/component_graph.rs:605-618`

**Problem:** The `update_status_with_message()` method accepts any status transition without validation. The state machine defined in `state_validation.rs` is only consulted by managers before issuing operations — the graph itself has no guard rails.

```rust
// component_graph.rs:605-618
fn update_status_with_message(&mut self, id: &str, status: ComponentStatus, message: Option<String>)
    -> anyhow::Result<Option<ComponentEvent>>
{
    let node = self.get_component_mut(id).ok_or_else(/* ... */)?;
    node.status = status.clone();  // <-- unconditional overwrite
    Ok(self.emit_event(id, &kind, status, message))
}
```

This means a buggy component could send `Running → Starting` or `Stopped → Stopping` and the graph would accept it. The state validation lives entirely in the managers and only guards the cold-path operations (start/stop/delete), not the hot-path status updates from components.

**Impact:** A misbehaving plugin could corrupt the graph's state representation, which would propagate to all subscribers and the snapshot API.

**Recommendation:** Add a `is_valid_transition(from, to) -> bool` check inside `update_status_with_message()`. Invalid transitions should be logged as warnings and rejected (returning `None` instead of emitting an event).

---

### 2.5 HIGH — Silent Edge Failures on Add

**Severity:** High  
**Files:** `lib/src/queries/manager.rs:1276-1281`, `lib/src/reactions/manager.rs:139-143`

**Problem:** When adding a query, the graph creates `Feeds` relationships to its sources. But if the source doesn't exist yet in the graph, the edge is silently skipped:

```rust
// queries/manager.rs:1276-1281
for sub in &config.sources {
    if graph.contains(&sub.source_id) {
        let _ = graph.add_relationship(&sub.source_id, &config.id, RelationshipKind::Feeds);
    }
    // else: silently skipped — no edge, no error, no warning
}
```

The same pattern exists for reactions. If a reaction references a query that doesn't exist yet, the `Feeds` edge is silently dropped.

**Impact:** The graph's dependency tracking becomes incomplete. `get_dependents()`, `can_remove()`, and `topological_order()` will return incorrect results for any component added before its dependencies. If components are added out of order (sources after queries), the graph will never have the correct edges.

**Recommendation:** Either:
- (a) **Require dependencies exist** (strict mode): Return an error if a referenced source/query doesn't exist in the graph.
- (b) **Deferred edge creation**: When a source is later added, scan existing queries for references to it and create edges retroactively.
- (c) **At minimum**, log a warning when an edge is skipped due to a missing dependency.

---

### 2.6 HIGH — `start_all()` Bypasses Graph Status Updates

**Severity:** High  
**Files:** `lib/src/sources/manager.rs:488-523`

**Problem:** The `start_all()` method calls `source.start().await` directly on each source but never sends a `Starting` status update through `update_tx`. Compare with `start_source()` (line 203) which does send the `Starting` event.

```rust
// start_all — no Starting event sent through update_tx
for source in sources {
    if !source.auto_start() { continue; }
    if let Err(e) = source.start().await {  // <-- starts directly
        failed_sources.push((source_id, e.to_string()));
    }
}
```

**Impact:** During bulk startup, the graph never sees a `Starting` transition for auto-started sources. The status jumps from `Added` directly to whatever the source sets internally (usually `Running`). Event subscribers and the event history miss the `Starting` event.

**Recommendation:** `start_all()` should send `ComponentUpdate::Status { status: Starting }` through `update_tx` before calling `source.start()`, matching the behavior of `start_source()`.

---

### 2.7 HIGH — No `remove_relationship()` Method

**Severity:** High  
**Files:** `lib/src/component_graph.rs`

**Problem:** The graph has `add_relationship()` but no `remove_relationship()`. The only way to remove edges is by removing the entire node (`remove_component()`). This means:

- A query cannot be unsubscribed from a source without removing the query entirely.
- A reaction cannot be unsubscribed from a query without removing the reaction.
- Dynamic reconfiguration (changing which sources a query reads from) cannot be reflected in the graph.

**Impact:** The graph cannot accurately represent dynamic subscription changes. If `update_source()` or `update_query()` changes the set of subscriptions, the graph edges become stale.

**Recommendation:** Add:
```rust
pub fn remove_relationship(&mut self, from_id: &str, to_id: &str, forward: RelationshipKind) -> anyhow::Result<()>
```
And use it during reconfiguration flows.

---

### 2.8 HIGH — No Duplicate Edge Prevention

**Severity:** High  
**Files:** `lib/src/component_graph.rs:632-654`

**Problem:** `add_relationship()` does not check whether the same edge already exists. If `add_relationship("source-1", "query-1", Feeds)` is called twice, two duplicate `Feeds` edges are created (and two duplicate `SubscribesTo` edges). `petgraph::StableGraph` is a multigraph and allows this.

**Impact:** `get_dependents()` and `get_dependencies()` would return duplicate entries. `edge_count()` would be inflated. `snapshot()` would contain redundant edges.

**Recommendation:** Check for existing edges before adding:
```rust
let already_exists = self.graph
    .edges_directed(from_idx, Direction::Outgoing)
    .any(|e| e.target() == to_idx && e.weight() == &forward);
if already_exists {
    return Ok(()); // or return Err
}
```

---

### 2.9 MEDIUM — `topological_order()` and `list_by_kind()` Are Dead Code

**Severity:** Medium  
**Files:** `lib/src/component_graph.rs:580-586, 728-771`, `lib/src/lifecycle.rs`

**Problem:** The `LifecycleManager` does not use `topological_order()` for startup/shutdown ordering. Instead, it hard-codes the order: Sources → Queries → Reactions for startup, Reactions → Queries → Sources for shutdown. And it iterates over the managers' HashMaps, not the graph.

```rust
// lifecycle.rs — hard-coded order, no graph consultation
pub async fn start_components(&self) -> Result<()> {
    self.source_manager.start_all().await?;    // 1. Sources
    // ... manually iterate query configs ...    // 2. Queries
    self.reaction_manager.start_all().await?;   // 3. Reactions
}
```

Similarly, `list_by_kind()` is never called outside tests.

**Impact:** These methods add to the API surface and maintenance burden but provide no value. If the graph's lifecycle ordering were used, it would correctly handle complex dependency chains (e.g., a query that feeds another query). The hard-coded three-tier ordering cannot.

**Recommendation:** Either wire `topological_order()` into `LifecycleManager` (preferred — it would enable multi-level dependency chains) or mark these methods `#[cfg(test)]` to clarify they're not production code.

---

### 2.10 MEDIUM — `ComponentStatusHandle` Uses `RwLock` for a Status Flag

**Severity:** Medium (performance)  
**Files:** `lib/src/component_graph.rs:118-178`

**Problem:** `ComponentStatusHandle` wraps the status in `Arc<RwLock<ComponentStatus>>`. This means every `get_status()` call acquires a read lock and every `set_status()` acquires a write lock + a read lock (for `update_tx`). For a simple enum value that is read far more often than written, this is heavyweight.

```rust
pub struct ComponentStatusHandle {
    component_id: String,
    status: Arc<RwLock<ComponentStatus>>,        // <-- could be AtomicU8 or ArcSwap
    update_tx: Arc<RwLock<Option<ComponentUpdateSender>>>,  // <-- could be OnceLock
}
```

**Impact:** In high-throughput scenarios with many components, the RwLock contention on status reads could become measurable. The `update_tx` lock is also unnecessary after wiring — it's set once and never changed.

**Recommendation:**
- Use `ArcSwap<ComponentStatus>` or map `ComponentStatus` variants to `AtomicU8` for lock-free reads.
- Use `OnceLock<ComponentUpdateSender>` for `update_tx` (set once, read many).
- Alternatively, use `tokio::sync::watch` which is designed exactly for this single-producer/multi-consumer latest-value pattern.

---

### 2.11 MEDIUM — `try_send()` Silently Drops Status Updates

**Severity:** Medium  
**Files:** `lib/src/component_graph.rs:163-171`

**Problem:** `ComponentStatusHandle::set_status()` uses `try_send()` which drops the update if the channel is full (capacity 1000). The result is silently ignored with `let _ = tx.try_send(...)`.

```rust
pub async fn set_status(&self, status: ComponentStatus, message: Option<String>) {
    *self.status.write().await = status.clone();
    if let Some(ref tx) = *self.update_tx.read().await {
        let _ = tx.try_send(ComponentUpdate::Status { ... });  // <-- drops on full
    }
}
```

**Impact:** If a burst of status updates exceeds the channel capacity, the graph will permanently miss status transitions. The component's local status will be correct, but the graph and all subscribers will be stale.

**Recommendation:** At minimum, log a warning when `try_send()` returns `Err(TrySendError::Full(...))`. Consider using `.send().await` for critical transitions (Starting, Running, Error, Stopped) and `try_send()` only for optional metrics. Alternatively, increase channel capacity or use an unbounded channel for status updates (they are small and infrequent relative to data events).

---

### 2.12 MEDIUM — Inspection API Queries the HashMap, Not the Graph

**Severity:** Medium  
**Files:** `lib/src/inspection.rs:74-80`

**Problem:** The `InspectionAPI` delegates to managers which query their internal HashMaps. It has no reference to the `ComponentGraph` at all.

```rust
pub struct InspectionAPI {
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
    // MISSING: component_graph: Arc<RwLock<ComponentGraph>>,
}
```

**Impact:** Any inspection query (`list_sources`, `get_source_status`, etc.) bypasses the graph entirely. This undermines the graph's role as the single source of truth.

**Recommendation:** The `InspectionAPI` should either:
- (a) Hold a reference to the `ComponentGraph` and use it for status/listing queries, or
- (b) Be refactored to delegate to `ComponentGraph::list_by_kind()` and `ComponentGraph::get_component()` for metadata, falling back to managers only for runtime-specific data (properties, query results).

---

### 2.13 MEDIUM — `remove_source()` Stops Only `Running` but Not `Starting`

**Severity:** Medium  
**Files:** `lib/src/lib_core_ops/source_ops.rs:83-107`

**Problem:** `DrasiLib::remove_source()` checks for `Running` status but not `Starting`:

```rust
if matches!(status, ComponentStatus::Running) {
    self.source_manager.stop_source(id.to_string()).await...;
}
```

But `delete_source()` in the manager does handle both:

```rust
if matches!(status, ComponentStatus::Running | ComponentStatus::Starting) {
    source.stop().await...;
}
```

**Impact:** If `remove_source()` is called while a source is `Starting`, the outer code won't stop it, but the inner `delete_source` will. This is redundant but not harmful because the inner code handles it. However, it's inconsistent and could mask timing issues.

**Recommendation:** Align `remove_source()` to also check `Starting`, or simplify by removing the outer status check entirely and letting `delete_source()` handle it (which it already does).

---

### 2.14 MEDIUM — `delete_source()` Uses a Hard-Coded 100ms Sleep

**Severity:** Medium  
**Files:** `lib/src/sources/manager.rs:333`

**Problem:** After stopping a source during deletion, the code sleeps for 100ms before checking if it's stopped:

```rust
tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
```

**Impact:** This is a race condition workaround. If the source takes longer than 100ms to stop, the subsequent status check may find it still in `Stopping`, and the code proceeds with a warning. Conversely, if it stops immediately, 100ms is wasted.

**Recommendation:** Use the same polling pattern as `update_source()` (which polls with a 10-second deadline and 50ms intervals). Or better, use a `tokio::sync::Notify` / `watch` channel to be notified when the status actually changes.

---

### 2.15 MEDIUM — Graph Update Loop Has No Graceful Shutdown

**Severity:** Medium  
**Files:** `lib/src/lib_core.rs:328-353`

**Problem:** The graph update loop runs until all `update_tx` senders are dropped. There is no explicit shutdown mechanism — it relies on all components being dropped, which drops their `ComponentUpdateSender` clones, which closes the channel.

```rust
tokio::spawn(async move {
    while let Some(update) = update_rx.recv().await {
        // ... process ...
    }
    tracing::debug!("Graph update loop exited — all senders dropped");
});
```

**Impact:** If any component holds a `ComponentUpdateSender` clone indefinitely (e.g., leaked in a spawned task), the loop never exits and the `JoinHandle` is never awaited (it's not even stored). This is a potential resource leak.

**Recommendation:** Store the `JoinHandle` and provide an explicit shutdown mechanism. Consider using a `CancellationToken` to signal shutdown independently of channel closure.

---

### 2.16 MEDIUM — `BootstrapProvider` and `IdentityProvider` Kinds Are Ghost Types

**Severity:** Medium  
**Files:** `lib/src/component_graph.rs:186-199, 244-254`

**Problem:** `ComponentKind::BootstrapProvider` and `ComponentKind::IdentityProvider` are defined as graph node types, and `RelationshipKind::Bootstraps`/`BootstrappedBy`/`Authenticates`/`AuthenticatedBy` are defined as edge types. However, **no code ever creates nodes or edges of these types**. Bootstrap providers are embedded inside sources, not tracked as independent graph nodes.

**Impact:** These types inflate the API surface and give the false impression that bootstrap providers and identity providers are first-class graph citizens. They may confuse plugin developers who see them in the enum and expect to register them.

**Recommendation:** Either:
- (a) Remove them from the enums and add them back when actually implemented, or
- (b) Mark them with `#[doc(hidden)]` and add `// Reserved for future use` comments, or
- (c) Implement the full lifecycle tracking for bootstrap/identity providers.

---

### 2.17 LOW — Mixing `log` and `tracing` Macros

**Severity:** Low (code smell)  
**Files:** `lib/src/lib_core.rs:336-348`, `lib/src/lifecycle.rs`, `lib/src/sources/manager.rs`

**Problem:** The codebase mixes `log::info!` / `log::warn!` with `tracing::debug!`. The graph update loop uses `log::info!` for events and `tracing::debug!` for the exit message.

**Impact:** No functional impact (tracing includes a `log` compatibility layer), but it's inconsistent and may confuse contributors.

**Recommendation:** Standardize on `tracing` macros throughout.

---

### 2.18 LOW — `ComponentNode.metadata` is Stringly-Typed

**Severity:** Low  
**Files:** `lib/src/component_graph.rs:283-294`

**Problem:** `metadata` is `HashMap<String, String>` with ad-hoc keys like `"kind"`, `"query"`, `"autoStart"`. There's no schema or type safety.

**Impact:** Typos in metadata keys will silently produce missing data. There's no way to know what keys are expected for each `ComponentKind`.

**Recommendation:** Consider a typed metadata enum or struct per `ComponentKind`, or at minimum define constants for the well-known keys.

---

### 2.19 LOW — `ComponentStatus` Derives `PartialEq` but Not `Eq`

**Severity:** Low (non-idiomatic)  
**Files:** `lib/src/channels/events.rs:159`

**Problem:** `ComponentStatus` derives `PartialEq` but not `Eq`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ComponentStatus { ... }
```

All variants are unit variants (no float fields), so `Eq` is trivially correct and would enable use in `HashSet`, `BTreeSet`, and as `HashMap` keys.

**Recommendation:** Add `Eq` and `Hash` derives.

---

### 2.20 LOW — `as_arc()` Creates a New `Arc` on Every Call

**Severity:** Low  
**Files:** `lib/src/lib_core.rs:213-215`

**Problem:** `DrasiLib::as_arc()` clones `self` and wraps in `Arc::new()` every time. Since `DrasiLib` is already entirely composed of `Arc`-wrapped fields, cloning is cheap (ref count bumps), but creating a new `Arc` wrapping is wasteful compared to storing the `Arc<DrasiLib>` once.

**Recommendation:** If `Arc<DrasiLib>` is needed, create it once during construction and store it. Or document that `as_arc()` creates a new Arc each time (currently the doc says "this helper makes the intent clearer").

---

## 3. Concurrency Analysis

### 3.1 Lock Ordering

The system acquires locks in the following order:
1. `component_graph` (RwLock — write for mutations, read for queries)
2. `sources/queries/reactions` HashMap (RwLock — write for add/remove, read for lookup)
3. `state_store` (RwLock — read only)

The managers always release the graph lock before acquiring the HashMap lock, which prevents deadlocks. The graph update loop acquires only the graph lock.

**Assessment:** Lock ordering is consistent. No deadlock risk identified.

### 3.2 TOCTOU (Time-of-Check-Time-of-Use)

The `update_source()` method has a potential TOCTOU issue:
1. Reads old source from HashMap → releases lock
2. Stops old source (async, takes time)
3. Initializes new source
4. Re-acquires HashMap write lock → checks if entry still exists → replaces

The guard at step 4 (`if !sources.contains_key(&id)`) catches concurrent deletion. However, concurrent `update_source()` on the same ID could still interleave (both read the old source, both stop it, both try to replace).

**Assessment:** Low risk in practice (component updates are administrative, not high-frequency), but should be documented as a known limitation.

### 3.3 Channel Backpressure

- **update_tx (mpsc, capacity 1000):** Status updates use `try_send()` which drops on full. For 1000 components each sending ~3 status changes (Added→Starting→Running), the channel could handle 333 concurrent component startups. Adequate for expected use cases.
- **event_tx (broadcast, capacity 1000):** Broadcast channels lag behind slow receivers. If a subscriber doesn't poll fast enough, it will receive `RecvError::Lagged` and miss events. The graph ignores broadcast send failures.

**Assessment:** Adequate for current scale. The `try_send()` drop risk (§2.11) is the main concern.

---

## 4. Summary of Recommendations

### Must-Fix Before Merge (Critical/High)

| # | Issue | Effort |
|---|-------|--------|
| 2.1 | Route status queries through ComponentGraph, not HashMap | Medium |
| 2.2 | Call `can_remove()` in all delete/remove paths | Low |
| 2.3 | Resolve `Added` status dead zone (transition to `Stopped` immediately) | Low |
| 2.4 | Add state transition validation in `update_status_with_message()` | Medium |
| 2.5 | Warn or error when relationship edges are skipped due to missing targets | Low |
| 2.6 | Send `Starting` event in `start_all()` | Low |
| 2.7 | Add `remove_relationship()` method | Medium |
| 2.8 | Prevent duplicate edges in `add_relationship()` | Low |

### Should-Fix (Medium)

| # | Issue | Effort |
|---|-------|--------|
| 2.9 | Wire `topological_order()` into LifecycleManager or mark `#[cfg(test)]` | Medium |
| 2.10 | Replace `RwLock` with lock-free primitives in `ComponentStatusHandle` | Medium |
| 2.11 | Log warnings on `try_send()` failures | Low |
| 2.12 | Give InspectionAPI a reference to the ComponentGraph | Low |
| 2.13 | Align `remove_source()` status check with `delete_source()` | Low |
| 2.14 | Replace hard-coded sleep with polling or notification | Low |
| 2.15 | Store graph update loop JoinHandle and add explicit shutdown | Low |
| 2.16 | Address ghost types (BootstrapProvider/IdentityProvider) | Low |

### Nice-to-Have (Low)

| # | Issue | Effort |
|---|-------|--------|
| 2.17 | Standardize on `tracing` macros | Low |
| 2.18 | Type-safe metadata | Medium |
| 2.19 | Add `Eq` and `Hash` derives to `ComponentStatus` | Trivial |
| 2.20 | Store `Arc<DrasiLib>` instead of creating on each call | Low |

---

## 5. Architecture Diagram

```
                    ┌──────────────────────────────┐
                    │         DrasiLib              │
                    │                              │
                    │  component_graph ─────────── Arc<RwLock<ComponentGraph>>
                    │  source_manager ──────────── Arc<SourceManager>
                    │  query_manager ───────────── Arc<QueryManager>
                    │  reaction_manager ────────── Arc<ReactionManager>
                    │  component_event_broadcast_tx │
                    └──────────┬───────────────────┘
                               │
                ┌──────────────┼──────────────┐
                │              │              │
    ┌───────────▼──┐   ┌──────▼──────┐  ┌────▼─────────┐
    │SourceManager │   │QueryManager │  │ReactionMgr   │
    │              │   │             │  │              │
    │ HashMap<Arc> │   │ HashMap<Arc>│  │ HashMap<Arc> │
    │ graph (ref)  │   │ graph (ref) │  │ graph (ref)  │
    │ update_tx    │   │ update_tx   │  │ update_tx    │
    └──────┬───────┘   └──────┬──────┘  └──────┬───────┘
           │                  │                │
           ▼                  ▼                ▼
    ┌─────────────────────────────────────────────────┐
    │              ComponentGraph                     │
    │                                                 │
    │  StableGraph<ComponentNode, RelationshipKind>   │
    │  index: HashMap<String, NodeIndex>              │
    │                                                 │
    │  event_tx: broadcast::Sender<ComponentEvent>    │──▶ Subscribers
    │  update_tx: mpsc::Sender<ComponentUpdate>       │
    └───────────────────────┬─────────────────────────┘
                            │
                    Graph Update Loop
                    (sole mpsc consumer)
                            │
                    ┌───────▼────────┐
                    │ apply_update() │
                    │ emit_event()   │
                    │ record_event() │
                    └────────────────┘
```

**Data Flow:**
- **Cold path (structural):** Manager acquires graph write lock → add/remove component → release lock
- **Hot path (status):** Component calls `status_handle.set_status()` → mpsc `try_send()` → graph update loop → graph write lock → emit broadcast → release lock → record event

---

## 6. Conclusion

The ComponentGraph's core design is architecturally sound. The separation of structural mutations (cold path with direct graph lock) from status updates (hot path with fire-and-forget mpsc) is a good concurrency pattern. The use of `petgraph::StableGraph` is appropriate for a runtime-mutable graph.

However, the graph is currently a **shadow of the truth**, not the source of truth. All queries about component state go through the managers' HashMaps, all deletions bypass dependency validation, and several graph capabilities (`topological_order`, `list_by_kind`, `can_remove`) are tested but never used in production code paths.

To make the ComponentGraph the true single source of truth, the recommended priority is:
1. **Route all status and listing queries through the graph** (§2.1, §2.12)
2. **Enforce dependency constraints on deletion** (§2.2)
3. **Add state transition validation** (§2.3, §2.4)
4. **Fix silent edge failures** (§2.5, §2.8)
5. **Wire `topological_order()` into the lifecycle** (§2.9)

These changes would transform the ComponentGraph from a passive observer into the authoritative control plane for all component lifecycle operations.
