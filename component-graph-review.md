# ComponentGraph Architecture Review (Post-Implementation)

**Date:** 2026-03-18  
**Scope:** Full review of `lib/src/component_graph.rs` and its system-wide integration after implementing the 9 must-fix items from the initial review.  
**Prior Review:** `component-graph-review-v1.md` (pre-implementation)

---

## 1. Status of Prior Review Items

All 8 must-fix items from the initial review have been implemented:

| # | Issue | Status | Notes |
|---|-------|--------|-------|
| 2.1 | Dual source of truth for status | ✅ Fixed | `get_*_status()` and `list_*()` read from graph |
| 2.2 | No dependency validation on deletion | ✅ Fixed | `can_remove()` called in all `delete_*()` paths |
| 2.3 | `Added`/`Removed` status dead zone | ✅ Fixed | Removed from enum; components start as `Stopped` |
| 2.4 | No state transition enforcement | ✅ Fixed | `is_valid_transition()` guards all updates |
| 2.5 | Silent edge failures on add | ✅ Fixed | `tracing::warn!` on missing edge targets |
| 2.6 | `start_all()` bypasses Starting event | ✅ Fixed | Sends `Starting` through `update_tx` |
| 2.7 | No `remove_relationship()` | ✅ Fixed | Method added with idempotent behavior |
| 2.8 | No duplicate edge prevention | ✅ Fixed | `add_relationship()` is now idempotent |
| New | Transactional graph mutations | ✅ Added | `GraphTransaction` with commit/rollback |

Additionally fixed:
- `ComponentStatus` now derives `Eq` and `Hash`
- Reconfiguration flows update graph directly via `apply_update()` with event recording
- `Running → Reconfiguring` added as valid state transition
- `remove_*()` in `lib_core_ops` now checks `Starting` status alongside `Running`

---

## 2. Remaining Findings

### 2.1 CRITICAL — Inverted Control Flow: Managers Drive the Graph Instead of Vice Versa

**Severity:** Critical (architectural)  
**Files:** `lib/src/sources/manager.rs`, `lib/src/queries/manager.rs`, `lib/src/reactions/manager.rs`, `lib/src/lib_core_ops/*.rs`, `lib/src/lib_core.rs`

**Problem:** The current architecture has managers directly manipulating the ComponentGraph — calling `graph.add_component()`, `graph.add_relationship()`, `graph.remove_component()`, and `graph.apply_update()`. This means managers have intimate knowledge of the graph's internals (node construction, edge types, relationship kinds, locking). The ComponentGraph is treated as a passive data store rather than the authoritative control plane.

**Current flow (manager-driven):**
```
User → DrasiLib::add_source(source) → SourceManager::add_source()
    ├─ Manager acquires graph write lock
    ├─ Manager constructs ComponentNode with metadata
    ├─ Manager calls graph.add_component(node)
    ├─ Manager calls graph.add_relationship() for each edge
    ├─ Manager releases graph lock
    ├─ Manager creates runtime context
    ├─ Manager calls source.initialize()
    └─ Manager stores in HashMap
```

**Problems with this approach:**
1. **Managers know too much** — They construct `ComponentNode`, `ComponentKind`, `RelationshipKind`, manage metadata keys, and hold graph locks directly.
2. **Graph cannot enforce its own invariants** — Dependency validation, duplicate checking, and state transitions are scattered across managers instead of centralized in the graph.
3. **No single command entry point** — The graph cannot reject, queue, or coordinate operations because it doesn't see them until they're already partially applied.
4. **Orphaned nodes on failure** — If runtime init fails after graph registration, the manager must handle rollback (and currently doesn't).
5. **Status updates bypass the graph** — Managers send `Starting`/`Stopping` through `update_tx` but send `Reconfiguring` directly via `apply_update()`, creating inconsistent paths.

**Recommended architecture (graph-driven):**

The ComponentGraph should be the **first recipient** of all component creation, deletion, and control commands. It updates the graph as the desired state, validates dependencies, and then dispatches to the appropriate manager for runtime operations. Components send status updates back through the graph.

```
User → DrasiLib::add_source(source)
    └─ ComponentGraph::register_source(id, metadata, source_instance)
        ├─ Graph validates: no duplicate ID, all dependencies exist
        ├─ Graph creates node + ownership edges (transactional)
        ├─ Graph dispatches to SourceManager::provision(source_instance)
        │   ├─ Manager creates runtime context
        │   ├─ Manager calls source.initialize()
        │   └─ Manager stores in HashMap
        ├─ On manager failure: Graph rolls back node + edges
        └─ On success: Graph emits ComponentEvent

User → DrasiLib::remove_source(id)
    └─ ComponentGraph::deregister_source(id)
        ├─ Graph validates: can_remove() — no dependents
        ├─ Graph dispatches to SourceManager::deprovision(id)
        │   ├─ Manager stops if running
        │   ├─ Manager calls deprovision()
        │   └─ Manager removes from HashMap
        ├─ Graph removes node + edges
        └─ Graph emits ComponentEvent

Component status updates (hot path):
    Source → status_handle.set_status(Running) → update_tx → Graph update loop
        └─ Graph validates transition, updates node, emits event

User → DrasiLib::start_source(id)
    └─ ComponentGraph::start_component(id)
        ├─ Graph validates: component exists, status allows Start
        ├─ Graph sets status to Starting, emits event
        ├─ Graph dispatches to SourceManager::start(id)
        └─ Component reports Running/Error via status_handle
```

**Key principles of the recommended architecture:**
1. **Graph is the entry point** — All commands flow through the graph first
2. **Graph enforces invariants** — Dependency checks, state validation, duplicate prevention
3. **Managers are runtime-only** — They handle `initialize()`, `start()`, `stop()`, `deprovision()`, but never touch graph structure
4. **Managers don't import graph types** — No `ComponentNode`, `ComponentKind`, `RelationshipKind` in manager code
5. **Single status update path** — All status changes flow through `update_tx` → graph update loop
6. **Transactional** — Graph registration is atomic; rollback on manager failure is graph-initiated

**Migration path:**
This is a significant refactor. A phased approach:
1. Add `register_source/query/reaction()` methods to ComponentGraph that encapsulate node+edge creation
2. Move graph calls from managers into DrasiLib (orchestration layer)
3. Have DrasiLib call graph first, then manager
4. Remove graph references from managers (managers only hold `update_tx` for status reporting)
5. Eventually, ComponentGraph dispatches to managers (full inversion)

**Current flow:**
```
DrasiLib::add_source(source)          [lib_core_ops — thin wrapper]
  └→ SourceManager::add_source()     [manager — does everything]
       ├─ graph.add_component(node)   // manager touches graph directly
       ├─ source.initialize()         // manager does runtime init
       └─ hashmap.insert()            // manager stores instance
```

**Why this is problematic:**
1. **Managers can create orphaned graph nodes** — If `DrasiQuery::new()` fails after `graph.add_component()`, the graph has a node with no runtime instance. There is no rollback.
2. **Dependency validation is in the wrong place** — Checking whether referenced sources/queries exist is done (or not done) by the manager, not the graph. The graph should enforce its own invariants.
3. **`GraphTransaction` exists but is unused** — The transaction primitive was built for exactly this use case, but no manager uses it.
4. **Managers mix structural and operational concerns** — Constructing graph nodes, creating edges, and managing relationships are structural operations that belong in the graph layer or the `DrasiLib` orchestration layer.

**Recommended architecture:**
```
DrasiLib::add_source(source)          [orchestration layer]
  ├─ graph.register_source(id, metadata, dependencies)  // graph validates & creates node+edges
  ├─ source.initialize(context)       // runtime init (no graph involvement)
  ├─ manager.store_instance(source)   // manager stores runtime instance only
  └─ on failure: graph.remove_component(id)  // compensating rollback
```

Or, if keeping the manager as the orchestrator:
```
SourceManager::add_source()
  ├─ graph.begin()                    // start transaction
  ├─ txn.add_component(node)          // node added, events deferred
  ├─ txn.add_relationship(...)        // edges added, validated
  ├─ txn.commit()                     // events emitted
  ├─ source.initialize()             // runtime init
  ├─ hashmap.insert()                // store instance
  └─ on init failure: graph.remove_component()  // compensating rollback
```

The key point: graph registration should either be (a) a single graph method that validates dependencies, or (b) wrapped in a `GraphTransaction` with compensating rollback on failure. Currently it is neither.

---

### 2.2 HIGH — Adding Components with Non-Existent Dependencies Should Fail

**Severity:** High  
**Files:** `lib/src/queries/manager.rs:1276-1286`, `lib/src/reactions/manager.rs:139-149`

**Problem:** When a query is added that references a source not yet in the graph, or a reaction references a query not yet in the graph, the system logs a warning and silently skips the edge creation. This should be a hard error — it should not be possible to create a component that has a dependency on another component that does not exist already.

```rust
// queries/manager.rs:1276-1286 — silently skips missing sources
for sub in &config.sources {
    if graph.contains(&sub.source_id) {
        let _ = graph.add_relationship(&sub.source_id, &config.id, RelationshipKind::Feeds);
    } else {
        tracing::warn!(...);  // <-- should be an error, not a warning
    }
}
```

**Impact:** The graph's dependency tracking is incomplete. `can_remove()` will not see the dependency, allowing sources/queries to be deleted while other components still reference them.

**Recommendation:** Change the warning to a hard error. If any referenced source/query doesn't exist, the entire `add_query`/`add_reaction` should fail and roll back (using `GraphTransaction`):

```rust
for sub in &config.sources {
    if !graph.contains(&sub.source_id) {
        return Err(anyhow::anyhow!(
            "Cannot add query '{}': referenced source '{}' does not exist",
            config.id, sub.source_id
        ));
    }
    graph.add_relationship(&sub.source_id, &config.id, RelationshipKind::Feeds)?;
}
```

---

### 2.3 HIGH — Silent `let _` Discards `add_relationship` Errors

**Severity:** High  
**Files:** `lib/src/queries/manager.rs:1278`, `lib/src/reactions/manager.rs:141-142`

**Problem:** `add_relationship()` can return errors (e.g., if a component was concurrently removed between the `contains()` check and the `add_relationship()` call). These errors are silently discarded with `let _`:

```rust
let _ = graph.add_relationship(&sub.source_id, &config.id, RelationshipKind::Feeds);
```

**Recommendation:** Log the error instead of discarding:

```rust
if let Err(e) = graph.add_relationship(&sub.source_id, &config.id, RelationshipKind::Feeds) {
    tracing::warn!("Failed to create Feeds edge from '{}' to '{}': {e}", sub.source_id, config.id);
}
```

---

### 2.4 MEDIUM — Dual Status Update Paths (Inconsistent Architecture)

**Severity:** Medium  
**Files:** `lib/src/sources/manager.rs:213-221,413-421`, `lib/src/reactions/manager.rs:195-203,404-413`

**Problem:** Status updates flow through two different paths:

1. **Async channel** (`update_tx.send()`): Used for `Starting`, `Stopping` in `start_source()`, `stop_source()`
2. **Direct graph mutation** (`graph.apply_update()`): Used for `Reconfiguring`, `Stopped` (post-reconfiguration) in `update_source()`

Path 2 requires the manager to also manually record events in `event_history`, while path 1 has this handled automatically by the graph update loop. This creates a maintenance burden and risks forgetting to record events when new direct mutations are added.

**Recommendation:** For consistency, consider routing all manager-initiated transitions through `update_tx` and using `tokio::sync::watch` or a `Notify` to synchronize when the graph has processed the update. Alternatively, document the two-path pattern clearly and add a helper method `apply_update_and_record()` that encapsulates both steps.

---

### 2.5 MEDIUM — TOCTOU in `remove_source/query/reaction` (lib_core_ops)

**Severity:** Medium  
**Files:** `lib/src/lib_core_ops/source_ops.rs:87-98`, `query_ops.rs:72-83`, `reaction_ops.rs:87-98`

**Problem:** The `remove_*()` methods read status, then conditionally stop, then delete — all as separate async operations with no atomicity:

```rust
let status = self.source_manager.get_source_status(id.to_string()).await?;
// <-- status could change here
if matches!(status, ComponentStatus::Running | ComponentStatus::Starting) {
    self.source_manager.stop_source(id.to_string()).await?;
    // <-- another caller could delete it here
}
self.source_manager.delete_source(id.to_string(), cleanup).await?;
```

**Impact:** In concurrent scenarios, the component could be stopped or deleted by another task between the status check and the stop/delete call. However, the inner `delete_source()` already handles stopping running components, so the outer stop is redundant. The real risk is a clearer error message path — not data corruption.

**Recommendation:** Remove the outer status-check-and-stop from `lib_core_ops` and rely entirely on `delete_source()` which already handles this internally with proper state validation. The `lib_core_ops` layer should be a thin wrapper:

```rust
pub async fn remove_source(&self, id: &str, cleanup: bool) -> Result<()> {
    self.state_guard.require_initialized().await?;
    self.source_manager.delete_source(id.to_string(), cleanup).await
        .map_err(|e| DrasiError::provisioning(format!("Failed to delete source: {e}")))?;
    Ok(())
}
```

---

### 2.6 MEDIUM — `BootstrapProvider` and `IdentityProvider` Remain Ghost Types

**Severity:** Medium  
**Files:** `lib/src/component_graph.rs:186-198`

**Problem:** `ComponentKind::BootstrapProvider` and `ComponentKind::IdentityProvider` are still defined in the enum but never instantiated as graph nodes. `RelationshipKind::Bootstraps`/`BootstrappedBy`/`Authenticates`/`AuthenticatedBy` are defined but never used. The `to_component_type()` method returns `None` for these kinds, so they cannot emit events.

**Recommendation:** Either implement proper tracking (register bootstrap providers when sources are added with them) or remove the variants and add them back when implemented. Mark with `#[doc(hidden)]` at minimum.

---

### 2.7 MEDIUM — Fragile Message-Based Event Detection in ComponentGraphSource

**Severity:** Medium  
**Files:** `lib/src/sources/component_graph_source.rs`

**Problem:** After removing `Added`/`Removed` from `ComponentStatus`, the `ComponentGraphSource` detects add/remove events by checking if `event.message` ends with `"added"` or `"removed"`. This is fragile — any message wording change will silently break the detection.

```rust
let changes = match event.message.as_deref() {
    Some(msg) if msg.ends_with("added") => build_added_changes(instance_id, event),
    Some(msg) if msg.ends_with("removed") => build_removed_changes(instance_id, event),
    _ => build_status_update_changes(event),
};
```

**Recommendation:** Add a structured field to `ComponentEvent` (e.g., `event_kind: Option<EventKind>` where `EventKind` is `StatusChange | ComponentAdded | ComponentRemoved`) or use a separate broadcast channel for structural changes. This would make the detection type-safe rather than string-based.

---

### 2.8 MEDIUM — Inconsistent Metadata Keys Across Managers

**Severity:** Medium (code quality)  
**Files:** `lib/src/sources/manager.rs:164`, `lib/src/queries/manager.rs:1266`, `lib/src/reactions/manager.rs:129`

**Problem:** Metadata field names are ad-hoc strings with no constants:

| Manager | Key | Value |
|---------|-----|-------|
| Source | `"kind"` | source type name |
| Source | `"autoStart"` | bool as string |
| Query | `"query"` | query text |
| Reaction | `"kind"` | reaction type name |

**Recommendation:** Define constants in `component_graph.rs`:

```rust
pub mod metadata_keys {
    pub const COMPONENT_TYPE: &str = "componentType";
    pub const AUTO_START: &str = "autoStart";
    pub const QUERY_TEXT: &str = "queryText";
}
```

---

### 2.9 LOW — Lock Contention in Reconfiguration Event Recording

**Severity:** Low  
**Files:** `lib/src/sources/manager.rs:413-421`, `lib/src/reactions/manager.rs:404-413`

**Problem:** The reconfiguration path holds the graph write lock while also acquiring the `event_history` write lock:

```rust
{
    let mut graph = self.graph.write().await;
    if let Some(event) = graph.apply_update(...) {
        self.event_history.write().await.record_event(event);  // <-- nested lock
    }
}
```

**Impact:** Under high concurrency, this double-lock could cause contention. The graph write lock blocks all graph reads/writes while event recording completes.

**Recommendation:** Drop the graph lock before recording the event:

```rust
let event = {
    let mut graph = self.graph.write().await;
    graph.apply_update(...)
};
if let Some(event) = event {
    self.event_history.write().await.record_event(event);
}
```

---

### 2.10 LOW — `topological_order()` and `list_by_kind()` Usage

**Severity:** Low  
**Files:** `lib/src/component_graph.rs`, `lib/src/lifecycle.rs`

**Problem:** `topological_order()` is still not used by `LifecycleManager` — startup/shutdown order is still hard-coded as Sources → Queries → Reactions. `list_by_kind()` is now used by managers for `list_*()` methods (good), but `topological_order()` remains test-only.

**Recommendation:** Consider wiring `topological_order()` into `LifecycleManager` for future support of multi-level dependency chains, or mark it `#[cfg(test)]`.

---

### 2.11 LOW — Hard-Coded Sleep in `delete_source()` / `delete_query()` / `delete_reaction()`

**Severity:** Low  
**Files:** `lib/src/sources/manager.rs:336`, `lib/src/queries/manager.rs:~1500`, `lib/src/reactions/manager.rs:~308`

**Problem:** After stopping a component during deletion, the code sleeps for 100ms before checking if it's actually stopped:

```rust
tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
```

**Recommendation:** Use the same polling pattern as `update_source()` (which polls with a 10-second deadline and 50ms intervals), or use `tokio::sync::Notify` for event-driven notification.

---

## 3. Test Coverage Gaps

### 3.1 CRITICAL — No Integration Tests for `can_remove()` Enforcement

The `can_remove()` check was added to all `delete_*()` methods, but there are **zero integration tests** verifying that removing a source with dependent queries is actually blocked. Only a unit test in `component_graph.rs` tests the graph method itself.

**Missing tests:**
- `remove_source()` fails when queries depend on it
- `remove_query()` fails when reactions depend on it
- `remove_source()` succeeds when no dependents exist
- Error message contains the dependent component IDs

### 3.2 HIGH — No Integration Tests for `GraphTransaction` Rollback

The `GraphTransaction` has 3 unit tests (commit, rollback, partial failure), but no integration test demonstrates rollback during a real component lifecycle operation.

### 3.3 MEDIUM — Timing-Based Tests Are Fragile

Approximately 20+ tests across `sources/tests.rs`, `queries/tests.rs`, and `reactions/tests.rs` use `tokio::time::sleep(Duration::from_millis(50-100))` to wait for async graph updates. These may become flaky on slow CI systems.

### 3.4 MEDIUM — Missing State Transition Edge Cases

- No test for `Error → Starting` recovery path
- No test for `Stopping → Error` (error during shutdown)
- No test for status update on a non-existent component
- No test for concurrent reconfiguration of the same component

---

## 4. Architecture Diagram (Post-Implementation)

```
                    ┌──────────────────────────────────┐
                    │           DrasiLib                │
                    │                                  │
                    │  component_graph ──── Arc<RwLock<ComponentGraph>>
                    │  source_manager ───── Arc<SourceManager>
                    │  query_manager ────── Arc<QueryManager>
                    │  reaction_manager ─── Arc<ReactionManager>
                    │  component_event_broadcast_tx     │
                    └──────────┬───────────────────────┘
                               │
                ┌──────────────┼──────────────┐
                │              │              │
    ┌───────────▼──┐   ┌──────▼──────┐  ┌────▼─────────┐
    │SourceManager │   │QueryManager │  │ReactionMgr   │
    │              │   │             │  │              │
    │ HashMap<Arc> │   │ HashMap<Arc>│  │ HashMap<Arc> │
    │ graph (ref)  │   │ graph (ref) │  │ graph (ref)  │
    │ update_tx    │   │ update_tx   │  │ update_tx    │
    │              │   │             │  │              │
    │ ┌──────────┐ │   │             │  │              │
    │ │ Status:  │ │   │             │  │              │
    │ │ READ from│ │   │ READ from   │  │ READ from    │
    │ │ graph ✅ │ │   │ graph ✅    │  │ graph ✅     │
    │ └──────────┘ │   │             │  │              │
    │ ┌──────────┐ │   │             │  │              │
    │ │can_remove│ │   │ can_remove  │  │ can_remove   │
    │ │on delete✅│ │   │ on delete✅ │  │ on delete✅  │
    │ └──────────┘ │   │             │  │              │
    │ ┌──────────┐ │   │             │  │              │
    │ │Txn: NOT  │ │   │ Txn: NOT   │  │ Txn: NOT     │
    │ │used yet⚠│ │   │ used yet⚠  │  │ used yet⚠   │
    │ └──────────┘ │   │             │  │              │
    └──────┬───────┘   └──────┬──────┘  └──────┬───────┘
           │                  │                │
           ▼                  ▼                ▼
    ┌─────────────────────────────────────────────────┐
    │              ComponentGraph                     │
    │                                                 │
    │  ✅ StableGraph<ComponentNode, RelationshipKind> │
    │  ✅ is_valid_transition() state machine          │
    │  ✅ Idempotent add_relationship()                │
    │  ✅ remove_relationship()                        │
    │  ✅ can_remove() dependency check                │
    │  ✅ GraphTransaction (begin/commit/rollback)     │
    │  ⚠  GraphTransaction NOT USED by managers       │
    │                                                 │
    │  event_tx: broadcast::Sender<ComponentEvent>    │
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

**Legend:** ✅ = implemented and working, ⚠ = implemented but not wired in

---

## 5. Summary of Recommendations

### Must-Fix Before Merge

| # | Issue | Effort | Impact |
|---|-------|--------|--------|
| 2.1 | Invert control flow: graph receives commands first, dispatches to managers | High | Eliminates orphaned nodes, centralizes invariant enforcement, decouples managers from graph internals |
| 2.3 | Replace `let _` on `add_relationship` errors with proper logging | Low | Prevents silent failures |
| 2.9 | Drop graph lock before recording events in reconfiguration path | Low | Prevents lock contention |
| 3.1 | Add integration tests for `can_remove()` enforcement | Low | Validates critical safety feature |

### Should-Fix

| # | Issue | Effort | Impact |
|---|-------|--------|--------|
| 2.2 | Reject components with non-existent dependencies (error, not warning) | Medium | Prevents incomplete dependency tracking |
| 2.4 | Unify status update paths (all through `update_tx` or document dual-path) | Medium | Reduces maintenance burden |
| 2.5 | Simplify `remove_*` in lib_core_ops (delegate to manager's `delete_*`) | Low | Eliminates TOCTOU |
| 2.7 | Replace string-based event detection with structured `EventKind` | Medium | Type-safe add/remove detection |
| 3.3 | Replace timing-based test waits with event-driven synchronization | Medium | Reduces CI flakiness |

### Nice-to-Have

| # | Issue | Effort | Impact |
|---|-------|--------|--------|
| 2.6 | Resolve ghost types (BootstrapProvider/IdentityProvider) | Low | Cleaner API |
| 2.8 | Define metadata key constants | Low | Better code quality |
| 2.10 | Wire `topological_order()` into `LifecycleManager` | Medium | Multi-level dependency support |
| 2.11 | Replace hard-coded sleeps in delete flows | Low | More reliable shutdown |
| 3.4 | Add error recovery and concurrent operation tests | Medium | Better test coverage |

---

## 6. Conclusion

The implementation of the 9 must-fix items has significantly strengthened the ComponentGraph's role as the source of truth. Status queries, listing, dependency validation, state transitions, and event consistency are all routed through the graph. The `GraphTransaction` primitive provides a solid foundation for atomic graph mutations.

The primary remaining issue is architectural: the control flow is inverted (§2.1). Managers currently drive the graph, but the graph should drive the managers. The ComponentGraph should be the entry point for all component commands — it validates, updates its state as the desired/actual state, and dispatches runtime operations to managers. Managers should be runtime-only (start, stop, initialize) with no knowledge of graph internals. This is a significant refactor but can be done incrementally, starting with `register_source/query/reaction()` methods on the ComponentGraph that encapsulate node and edge creation with dependency validation.

Additionally, adding components with non-existent dependencies (§2.2) should be a hard error enforced by the graph, and the `GraphTransaction` primitive should be used for atomic graph operations with proper rollback.
