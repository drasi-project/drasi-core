// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use petgraph::stable_graph::{EdgeIndex, NodeIndex, StableGraph};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use tokio::sync::{broadcast, mpsc, Notify};

use crate::channels::{ComponentEvent, ComponentEventBroadcastReceiver, ComponentStatus};
use crate::managers::ComponentEventHistory;

use super::transaction::GraphTransaction;
use super::{
    ComponentKind, ComponentNode, ComponentUpdate, ComponentUpdateReceiver, ComponentUpdateSender,
    GraphEdge, GraphSnapshot, RelationshipKind,
};

// ============================================================================
// Component Graph
// ============================================================================

/// Central component dependency graph — the single source of truth.
///
/// Backed by `petgraph::stable_graph::StableGraph` which keeps node/edge indices
/// stable across removals (critical for a graph that changes at runtime).
///
/// # Error Handling
///
/// Mutation methods on this struct return `anyhow::Result`, following the Layer 2
/// (internal module) error convention. The public API boundary in `DrasiLib` and
/// `lib_core_ops` wraps these into `DrasiError` variants before returning to callers.
/// External consumers should use `DrasiLib` methods rather than calling graph
/// mutations directly.
///
/// # Event Emission
///
/// The graph emits [`ComponentEvent`]s via a built-in broadcast channel whenever
/// components are added, removed, or change status. Call [`subscribe()`](Self::subscribe)
/// to receive events.
///
/// # Thread Safety
///
/// `ComponentGraph` is NOT `Send`/`Sync` by itself due to the underlying `StableGraph`.
/// It must be wrapped in `Arc<RwLock<ComponentGraph>>` for multi-threaded access.
/// All public APIs in `DrasiLib` handle this wrapping automatically.
///
/// # Instance Root
///
/// The graph always has the DrasiLib instance as its root node.
/// All other components are connected to it via Owns/OwnedBy edges.
pub struct ComponentGraph {
    /// The underlying petgraph directed graph
    pub(super) graph: StableGraph<ComponentNode, RelationshipKind>,
    /// Fast lookup: component ID → node index (O(1) access)
    pub(super) index: HashMap<String, NodeIndex>,
    /// The instance node index (always present)
    instance_idx: NodeIndex,
    /// Broadcast sender for component lifecycle events (fan-out to subscribers).
    /// Events are emitted by `add_component()`, `remove_component()`, `update_status()`,
    /// and the graph update loop.
    pub(super) event_tx: broadcast::Sender<ComponentEvent>,
    /// mpsc sender for component status updates (fan-in from components).
    /// Cloned and given to each component's Base struct. The graph update loop
    /// owns the corresponding receiver.
    update_tx: mpsc::Sender<ComponentUpdate>,
    /// Notifies waiters when any component's status changes.
    /// Used by `wait_for_status()` to replace polling loops with event-driven waits.
    /// Follows the same pattern as `PriorityQueue::notify`.
    status_notify: Arc<Notify>,
    /// Type-erased runtime instances, keyed by component ID.
    ///
    /// Managers store their `Arc<dyn Source>`, `Arc<dyn Query>`, `Arc<dyn Reaction>`, etc.
    /// here during provisioning. This eliminates the dual-registry pattern where each
    /// manager maintained its own HashMap — the graph is now the single store for both
    /// metadata (in the petgraph node) and runtime instances (here).
    ///
    /// Access via typed helpers: [`set_runtime()`], [`get_runtime()`], [`take_runtime()`].
    runtimes: HashMap<String, Box<dyn Any + Send + Sync>>,
    /// Centralized event history for all components.
    ///
    /// Stores lifecycle events (Starting, Running, Error, Stopped, etc.) for every
    /// component in the graph. Events are recorded by [`apply_update()`] and
    /// [`remove_component()`]. Managers delegate event queries here rather than
    /// maintaining their own per-manager histories.
    event_history: ComponentEventHistory,
}

/// Default broadcast channel capacity for component events.
const EVENT_CHANNEL_CAPACITY: usize = 1000;

/// Default mpsc channel capacity for component updates.
const UPDATE_CHANNEL_CAPACITY: usize = 1000;

impl ComponentGraph {
    /// Create a new component graph with the given instance as root node.
    ///
    /// Returns the graph and a [`ComponentUpdateReceiver`] that must be consumed by
    /// a graph update loop task (see [`Self::apply_update`]).
    pub fn new(instance_id: &str) -> (Self, ComponentUpdateReceiver) {
        let mut graph = StableGraph::new();
        let instance_node = ComponentNode {
            id: instance_id.to_string(),
            kind: ComponentKind::Instance,
            status: ComponentStatus::Running,
            metadata: HashMap::new(),
        };
        let instance_idx = graph.add_node(instance_node);

        let mut index = HashMap::new();
        index.insert(instance_id.to_string(), instance_idx);
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (update_tx, update_rx) = mpsc::channel(UPDATE_CHANNEL_CAPACITY);

        (
            Self {
                graph,
                index,
                instance_idx,
                event_tx,
                update_tx,
                status_notify: Arc::new(Notify::new()),
                runtimes: HashMap::new(),
                event_history: ComponentEventHistory::new(),
            },
            update_rx,
        )
    }

    /// Subscribe to component lifecycle events.
    ///
    /// Returns a broadcast receiver that gets a copy of every [`ComponentEvent`]
    /// emitted by graph mutations (`add_component`, `remove_component`, `update_status`).
    pub fn subscribe(&self) -> ComponentEventBroadcastReceiver {
        self.event_tx.subscribe()
    }

    /// Get a reference to the broadcast sender for component events.
    ///
    /// This allows callers to clone the sender before the graph is wrapped in
    /// `Arc<RwLock<>>`, enabling subscription without needing to acquire the lock.
    /// The returned sender is the same one used by the graph for event emission.
    pub fn event_sender(&self) -> &broadcast::Sender<ComponentEvent> {
        &self.event_tx
    }

    /// Get a clone of the mpsc update sender.
    ///
    /// This is the sender that components use to report status changes without
    /// acquiring any graph lock. Clone this and pass to `SourceBase`/`ReactionBase`.
    pub fn update_sender(&self) -> ComponentUpdateSender {
        self.update_tx.clone()
    }

    /// Get a clone of the status change notifier.
    ///
    /// This `Notify` is signalled whenever any component's status changes in the graph.
    /// Use it with [`wait_for_status`] or build custom wait loops that avoid polling
    /// with sleep.
    ///
    /// # Pattern
    ///
    /// ```ignore
    /// let notify = graph.status_notifier();
    /// // Register interest BEFORE releasing the graph lock
    /// let notified = notify.notified();
    /// drop(graph); // release lock
    /// notified.await; // woken when any status changes
    /// ```
    pub fn status_notifier(&self) -> Arc<Notify> {
        self.status_notify.clone()
    }

    /// Apply a [`ComponentUpdate`] received from the mpsc channel.
    ///
    /// Called by the graph update loop task. Updates the graph, emits a
    /// broadcast event, and records the event in the centralized event history.
    /// Returns the event for external logging/processing.
    pub fn apply_update(&mut self, update: ComponentUpdate) -> Option<ComponentEvent> {
        match update {
            ComponentUpdate::Status {
                component_id,
                status,
                message,
            } => match self.update_status_with_message(&component_id, status, message) {
                Ok(event) => event,
                Err(e) => {
                    tracing::debug!(
                        "Graph update loop: status update skipped for '{}': {e}",
                        component_id
                    );
                    None
                }
            },
        }
    }

    /// Emit a [`ComponentEvent`] to all broadcast subscribers and return it.
    ///
    /// Returns `Some(event)` if the component kind maps to a [`ComponentType`],
    /// `None` for kinds like `Instance` that have no external type.
    /// Silently ignores broadcast send failures (no subscribers connected).
    fn emit_event(
        &self,
        component_id: &str,
        kind: &ComponentKind,
        status: ComponentStatus,
        message: Option<String>,
    ) -> Option<ComponentEvent> {
        if let Some(component_type) = kind.to_component_type() {
            let event = ComponentEvent {
                component_id: component_id.to_string(),
                component_type,
                status,
                timestamp: chrono::Utc::now(),
                message,
            };
            let _ = self.event_tx.send(event.clone());
            Some(event)
        } else {
            None
        }
    }

    /// Get the instance ID (root node)
    pub fn instance_id(&self) -> &str {
        &self.graph[self.instance_idx].id
    }

    // ========================================================================
    // Runtime Instance Store
    // ========================================================================

    /// Store a runtime instance for a component.
    ///
    /// The component must already exist in the graph (registered via `add_component`
    /// or one of the `register_*` methods). The runtime instance is stored in a
    /// type-erased map keyed by component ID.
    ///
    /// Managers call this during provisioning after creating the runtime instance:
    /// ```ignore
    /// let source: Arc<dyn Source> = Arc::new(my_source);
    /// source.initialize(context).await;
    /// graph.set_runtime("source-1", Box::new(source))?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the component ID is not present in the graph.
    pub fn set_runtime(
        &mut self,
        id: &str,
        runtime: Box<dyn Any + Send + Sync>,
    ) -> anyhow::Result<()> {
        if !self.index.contains_key(id) {
            return Err(anyhow::anyhow!(
                "set_runtime called for component '{id}' which is not in the graph"
            ));
        }

        // Warn if the runtime type doesn't match the component kind.
        // This catches type mismatches during development. Uses a warning rather
        // than a hard assert to avoid breaking unit tests that use substitute types.
        #[cfg(debug_assertions)]
        if let Some(node) = self.get_component(id) {
            let kind = &node.kind;
            let type_ok = match kind {
                ComponentKind::Source => runtime
                    .downcast_ref::<std::sync::Arc<dyn crate::sources::Source>>()
                    .is_some(),
                ComponentKind::Query => runtime
                    .downcast_ref::<std::sync::Arc<dyn crate::queries::manager::Query>>()
                    .is_some(),
                ComponentKind::Reaction => runtime
                    .downcast_ref::<std::sync::Arc<dyn crate::reactions::Reaction>>()
                    .is_some(),
                _ => true,
            };
            if !type_ok {
                tracing::warn!(
                    "set_runtime: possible type mismatch for component '{id}' (kind={kind})"
                );
            }
        }

        self.runtimes.insert(id.to_string(), runtime);
        Ok(())
    }

    /// Retrieve a reference to a component's runtime instance, downcasting to `T`.
    ///
    /// Returns `None` if the component has no runtime instance or if the stored
    /// type doesn't match `T`.
    ///
    /// # Type Parameter
    ///
    /// `T` is typically `Arc<dyn Source>`, `Arc<dyn Query>`, or `Arc<dyn Reaction>`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let source: &Arc<dyn Source> = graph.get_runtime::<Arc<dyn Source>>("source-1")
    ///     .ok_or_else(|| anyhow!("Source not found"))?;
    /// let source = source.clone(); // Clone the Arc for use outside the lock
    /// ```
    pub fn get_runtime<T: 'static>(&self, id: &str) -> Option<&T> {
        self.runtimes.get(id).and_then(|r| r.downcast_ref::<T>())
    }

    /// Remove and return a component's runtime instance, downcasting to `T`.
    ///
    /// Returns `None` if the component has no runtime instance or if the stored
    /// type doesn't match `T`. On type mismatch, the runtime is **put back** into
    /// the store (not lost) and an error is logged.
    ///
    /// Used during teardown when the manager needs ownership of the instance
    /// (e.g., to call `deprovision()`).
    pub fn take_runtime<T: 'static>(&mut self, id: &str) -> Option<T> {
        let runtime = self.runtimes.remove(id)?;
        match runtime.downcast::<T>() {
            Ok(boxed) => Some(*boxed),
            Err(runtime) => {
                tracing::error!(
                    "take_runtime: type mismatch for component '{id}', putting runtime back"
                );
                self.runtimes.insert(id.to_string(), runtime);
                None
            }
        }
    }

    /// Check if a component has a runtime instance stored.
    pub fn has_runtime(&self, id: &str) -> bool {
        self.runtimes.contains_key(id)
    }

    // ========================================================================
    // Node Operations
    // ========================================================================

    /// Add a component node to the graph.
    ///
    /// Automatically creates bidirectional Owns/OwnedBy edges between the
    /// instance root and the new component. Emits a [`ComponentEvent`] with
    /// the node's current status to all subscribers.
    ///
    /// # Errors
    ///
    /// Returns an error if a component with the same ID already exists.
    pub fn add_component(&mut self, node: ComponentNode) -> anyhow::Result<NodeIndex> {
        let (node_idx, event, _, _) = self.add_component_internal(node)?;
        if let Some(event) = event {
            let _ = self.event_tx.send(event);
        }
        Ok(node_idx)
    }

    /// Internal: adds a component and returns the event without emitting it.
    /// Used by both `add_component()` (emits immediately) and `GraphTransaction`
    /// (defers emission to commit).
    pub(super) fn add_component_internal(
        &mut self,
        node: ComponentNode,
    ) -> anyhow::Result<(NodeIndex, Option<ComponentEvent>, EdgeIndex, EdgeIndex)> {
        if self.index.contains_key(&node.id) {
            return Err(anyhow::anyhow!(
                "{} '{}' already exists in the graph",
                node.kind,
                node.id
            ));
        }

        let id = node.id.clone();
        let kind = node.kind.clone();
        let status = node.status;
        let node_idx = self.graph.add_node(node);
        self.index.insert(id.clone(), node_idx);

        // Create bidirectional ownership edges (Instance ↔ Component)
        let owns_edge = self
            .graph
            .add_edge(self.instance_idx, node_idx, RelationshipKind::Owns);
        let owned_by_edge =
            self.graph
                .add_edge(node_idx, self.instance_idx, RelationshipKind::OwnedBy);

        let event = kind
            .to_component_type()
            .map(|component_type| ComponentEvent {
                component_id: id,
                component_type,
                status,
                timestamp: chrono::Utc::now(),
                message: Some(format!("{kind} added")),
            });

        Ok((node_idx, event, owns_edge, owned_by_edge))
    }

    /// Remove a component node and all its edges from the graph.
    ///
    /// Emits a [`ComponentEvent`] with status [`ComponentStatus::Removed`] to all
    /// subscribers. Returns the removed node data, or an error if the component
    /// doesn't exist. The instance root node cannot be removed.
    pub fn remove_component(&mut self, id: &str) -> anyhow::Result<ComponentNode> {
        let node_idx = self
            .index
            .get(id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Component '{id}' not found in graph"))?;

        if node_idx == self.instance_idx {
            return Err(anyhow::anyhow!("Cannot remove the instance root node"));
        }

        // Capture kind before removing so we can emit an event
        let kind = self.graph[node_idx].kind.clone();

        self.index.remove(id);
        // Remove runtime instance if present (atomic with node removal)
        self.runtimes.remove(id);
        // Remove event history for this component
        self.event_history.remove_component(id);
        // StableGraph::remove_node automatically removes all edges connected to this node
        let removed = self
            .graph
            .remove_node(node_idx)
            .ok_or_else(|| anyhow::anyhow!("Component '{id}' already removed"))?;

        self.emit_event(
            id,
            &kind,
            ComponentStatus::Removed,
            Some(format!("{kind} removed")),
        );

        Ok(removed)
    }

    /// Get a component node by ID.
    pub fn get_component(&self, id: &str) -> Option<&ComponentNode> {
        self.index
            .get(id)
            .and_then(|idx| self.graph.node_weight(*idx))
    }

    /// Get a mutable reference to a component node by ID.
    pub fn get_component_mut(&mut self, id: &str) -> Option<&mut ComponentNode> {
        self.index
            .get(id)
            .copied()
            .and_then(|idx| self.graph.node_weight_mut(idx))
    }

    /// Check if a component exists in the graph.
    pub fn contains(&self, id: &str) -> bool {
        self.index.contains_key(id)
    }

    /// List all components of a specific kind with their status.
    pub fn list_by_kind(&self, kind: &ComponentKind) -> Vec<(String, ComponentStatus)> {
        self.graph
            .node_weights()
            .filter(|node| &node.kind == kind)
            .map(|node| (node.id.clone(), node.status))
            .collect()
    }

    /// Update a component's status.
    ///
    /// Emits a [`ComponentEvent`] with the new status to all subscribers.
    /// Used internally by [`apply_update`] and tests.
    pub(super) fn update_status(
        &mut self,
        id: &str,
        status: ComponentStatus,
    ) -> anyhow::Result<Option<ComponentEvent>> {
        self.update_status_with_message(id, status, None)
    }

    /// Update a component's status with an optional message.
    ///
    /// Emits a [`ComponentEvent`] with the new status and message to all broadcast
    /// subscribers AND records it in the centralized event history. This ensures
    /// events are visible to both global subscribers (via broadcast) and per-component
    /// subscribers (via event history channels).
    ///
    /// Called by [`apply_update`] in the graph update loop and by
    /// [`validate_and_transition`] for command-initiated transitions.
    fn update_status_with_message(
        &mut self,
        id: &str,
        status: ComponentStatus,
        message: Option<String>,
    ) -> anyhow::Result<Option<ComponentEvent>> {
        let node = self
            .get_component_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Component '{id}' not found in graph"))?;
        let kind = node.kind.clone();

        // Same-state updates are idempotent no-ops (no event, no warning)
        if node.status == status {
            return Ok(None);
        }

        if !is_valid_transition(&node.status, &status) {
            tracing::warn!(
                "Invalid state transition for component '{}': {:?} → {:?}, ignoring update",
                id,
                node.status,
                status
            );
            return Ok(None);
        }

        node.status = status;

        // Wake up any waiters blocking on status changes (e.g., wait_for_status)
        self.status_notify.notify_waiters();

        let event = self.emit_event(id, &kind, status, message);
        if let Some(ref event) = event {
            self.event_history.record_event(event.clone());
        }
        Ok(event)
    }

    // ========================================================================
    // Edge Operations
    // ========================================================================

    /// Add a bidirectional relationship between two components (idempotent).
    ///
    /// Creates both the forward edge (from → to with `forward` relationship) and
    /// the reverse edge (to → from with the reverse of `forward`).
    /// If the relationship already exists, this is a no-op and returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// Returns an error if either component doesn't exist.
    pub fn add_relationship(
        &mut self,
        from_id: &str,
        to_id: &str,
        forward: RelationshipKind,
    ) -> anyhow::Result<()> {
        let (_, _) = self.add_relationship_internal(from_id, to_id, forward)?;
        Ok(())
    }

    /// Internal: adds a relationship and returns the edge indices for rollback.
    /// Returns `(None, None)` if the relationship already exists (idempotent).
    pub(super) fn add_relationship_internal(
        &mut self,
        from_id: &str,
        to_id: &str,
        forward: RelationshipKind,
    ) -> anyhow::Result<(Option<EdgeIndex>, Option<EdgeIndex>)> {
        let from_idx = self
            .index
            .get(from_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Component '{from_id}' not found in graph"))?;
        let to_idx = self
            .index
            .get(to_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Component '{to_id}' not found in graph"))?;

        // Validate the relationship is semantically valid for the node kinds
        let from_kind = &self.graph[from_idx].kind;
        let to_kind = &self.graph[to_idx].kind;
        if !is_valid_relationship(from_kind, to_kind, &forward) {
            return Err(anyhow::anyhow!(
                "Invalid relationship: {forward:?} from {from_kind} '{from_id}' to {to_kind} '{to_id}'"
            ));
        }

        // Idempotency: check if the forward edge already exists
        let already_exists = self
            .graph
            .edges_directed(from_idx, Direction::Outgoing)
            .any(|e| e.target() == to_idx && e.weight() == &forward);
        if already_exists {
            return Ok((None, None));
        }

        let reverse = forward.reverse();
        let fwd_edge = self.graph.add_edge(from_idx, to_idx, forward);
        let rev_edge = self.graph.add_edge(to_idx, from_idx, reverse);

        Ok((Some(fwd_edge), Some(rev_edge)))
    }

    /// Remove a bidirectional relationship between two components.
    ///
    /// Removes both the forward edge (from → to with `forward` relationship) and
    /// the reverse edge (to → from with the reverse of `forward`).
    /// If the relationship doesn't exist, this is a no-op and returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// Returns an error if either component doesn't exist in the graph.
    pub fn remove_relationship(
        &mut self,
        from_id: &str,
        to_id: &str,
        forward: RelationshipKind,
    ) -> anyhow::Result<()> {
        let from_idx = self
            .index
            .get(from_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Component '{from_id}' not found in graph"))?;
        let to_idx = self
            .index
            .get(to_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Component '{to_id}' not found in graph"))?;

        let reverse = forward.reverse();

        // Find and remove forward edge
        let forward_edge = self
            .graph
            .edges_directed(from_idx, Direction::Outgoing)
            .find(|e| e.target() == to_idx && e.weight() == &forward)
            .map(|e| e.id());
        if let Some(edge_id) = forward_edge {
            self.graph.remove_edge(edge_id);
        }

        // Find and remove reverse edge
        let reverse_edge = self
            .graph
            .edges_directed(to_idx, Direction::Outgoing)
            .find(|e| e.target() == from_idx && e.weight() == &reverse)
            .map(|e| e.id());
        if let Some(edge_id) = reverse_edge {
            self.graph.remove_edge(edge_id);
        }

        Ok(())
    }

    // ========================================================================
    // Relationship Queries
    // ========================================================================

    /// Get all components that this component has outgoing edges to,
    /// filtered by relationship kind.
    pub fn get_neighbors(&self, id: &str, relationship: &RelationshipKind) -> Vec<&ComponentNode> {
        let Some(&node_idx) = self.index.get(id) else {
            return Vec::new();
        };

        self.graph
            .edges_directed(node_idx, Direction::Outgoing)
            .filter(|edge| edge.weight() == relationship)
            .filter_map(|edge| self.graph.node_weight(edge.target()))
            .collect()
    }

    /// Get all components that depend on the given component.
    ///
    /// "Dependents" are components that would be affected if this component
    /// were removed or stopped. This follows Feeds edges (outgoing).
    pub fn get_dependents(&self, id: &str) -> Vec<&ComponentNode> {
        let Some(&node_idx) = self.index.get(id) else {
            return Vec::new();
        };

        self.graph
            .edges_directed(node_idx, Direction::Outgoing)
            .filter(|edge| matches!(edge.weight(), RelationshipKind::Feeds))
            .filter_map(|edge| self.graph.node_weight(edge.target()))
            .collect()
    }

    /// Get all components that this component depends on.
    ///
    /// "Dependencies" are components that this component needs to function.
    /// This follows SubscribesTo edges (outgoing).
    pub fn get_dependencies(&self, id: &str) -> Vec<&ComponentNode> {
        let Some(&node_idx) = self.index.get(id) else {
            return Vec::new();
        };

        self.graph
            .edges_directed(node_idx, Direction::Outgoing)
            .filter(|edge| matches!(edge.weight(), RelationshipKind::SubscribesTo))
            .filter_map(|edge| self.graph.node_weight(edge.target()))
            .collect()
    }

    /// Check if a component can be safely removed (no dependents that would break).
    ///
    /// Returns Ok(()) if safe, or Err with the list of dependent component IDs.
    pub fn can_remove(&self, id: &str) -> Result<(), Vec<String>> {
        let dependents = self.get_dependents(id);
        if dependents.is_empty() {
            Ok(())
        } else {
            Err(dependents.iter().map(|n| n.id.clone()).collect())
        }
    }

    // ========================================================================
    // Lifecycle
    // ========================================================================

    /// Atomically validate and apply a commanded status transition.
    ///
    /// This is the **single canonical way** for managers to change a component's
    /// status for command-initiated transitions (`Starting`, `Stopping`,
    /// `Reconfiguring`). It combines validation and mutation under a single
    /// `&mut self` borrow, eliminating the TOCTOU gap between checking status
    /// and updating it.
    ///
    /// Components still report runtime-initiated transitions (`Running`,
    /// `Stopped`, `Error`) via the mpsc channel → [`apply_update`].
    ///
    /// # Returns
    ///
    /// - `Ok(Some(event))` — transition applied, event emitted to broadcast subscribers
    /// - `Ok(None)` — same-state no-op (component already in `target_status`)
    /// - `Err(...)` — component not found or transition not valid from current state
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut graph = self.graph.write().await;
    /// graph.validate_and_transition("source-1", ComponentStatus::Starting, Some("Starting source"))?;
    /// drop(graph); // release lock before calling source.start()
    /// source.start().await?;
    /// ```
    pub fn validate_and_transition(
        &mut self,
        id: &str,
        target_status: ComponentStatus,
        message: Option<String>,
    ) -> anyhow::Result<Option<ComponentEvent>> {
        let node = self
            .get_component(id)
            .ok_or_else(|| anyhow::anyhow!("Component '{id}' not found in graph"))?;
        let current = node.status;

        // Same-state is an idempotent no-op
        if current == target_status {
            return Ok(None);
        }

        // Produce a descriptive error message for invalid transitions
        if !is_valid_transition(&current, &target_status) {
            let reason = describe_invalid_transition(id, &current, &target_status);
            return Err(anyhow::anyhow!(reason));
        }

        // Transition is valid — apply it
        self.update_status_with_message(id, target_status, message)
    }

    /// Get a topological ordering of components for lifecycle operations.
    ///
    /// Returns components in dependency order: sources first, then queries, then reactions.
    /// Only follows Feeds edges for ordering (other edge types don't affect lifecycle order).
    ///
    /// The instance root node is excluded from the result.
    pub fn topological_order(&self) -> anyhow::Result<Vec<&ComponentNode>> {
        // Build a filtered subgraph with only Feeds edges for ordering
        // (bidirectional edges like SubscribesTo/OwnedBy create cycles that
        // would prevent toposort on the full graph)
        let mut order_graph: StableGraph<(), ()> = StableGraph::new();
        let mut idx_map: HashMap<NodeIndex, NodeIndex> = HashMap::new();

        // Add all nodes
        for node_idx in self.graph.node_indices() {
            let new_idx = order_graph.add_node(());
            idx_map.insert(node_idx, new_idx);
        }

        // Add only Feeds edges
        for edge_idx in self.graph.edge_indices() {
            if let Some(weight) = self.graph.edge_weight(edge_idx) {
                if matches!(weight, RelationshipKind::Feeds) {
                    if let Some((from, to)) = self.graph.edge_endpoints(edge_idx) {
                        if let (Some(&new_from), Some(&new_to)) =
                            (idx_map.get(&from), idx_map.get(&to))
                        {
                            order_graph.add_edge(new_from, new_to, ());
                        }
                    }
                }
            }
        }

        // Reverse map: new index → original index
        let reverse_map: HashMap<NodeIndex, NodeIndex> =
            idx_map.iter().map(|(&orig, &new)| (new, orig)).collect();

        match petgraph::algo::toposort(&order_graph, None) {
            Ok(sorted) => Ok(sorted
                .into_iter()
                .filter_map(|new_idx| reverse_map.get(&new_idx))
                .filter(|idx| **idx != self.instance_idx)
                .filter_map(|idx| self.graph.node_weight(*idx))
                .collect()),
            Err(_cycle) => Err(anyhow::anyhow!(
                "Cycle detected in component graph — cannot determine lifecycle order"
            )),
        }
    }

    // ========================================================================
    // Serialization
    // ========================================================================

    /// Create a serializable snapshot of the entire graph.
    ///
    /// The snapshot includes the instance root node and all components
    /// with their relationships. Used for API responses and UI visualization.
    pub fn snapshot(&self) -> GraphSnapshot {
        let nodes: Vec<ComponentNode> = self.graph.node_weights().cloned().collect();

        let edges: Vec<GraphEdge> = self
            .graph
            .edge_indices()
            .filter_map(|edge_idx| {
                let (from_idx, to_idx) = self.graph.edge_endpoints(edge_idx)?;
                let from = self.graph.node_weight(from_idx)?;
                let to = self.graph.node_weight(to_idx)?;
                let relationship = self.graph.edge_weight(edge_idx)?;
                Some(GraphEdge {
                    from: from.id.clone(),
                    to: to.id.clone(),
                    relationship: relationship.clone(),
                })
            })
            .collect();

        GraphSnapshot {
            instance_id: self.instance_id().to_string(),
            nodes,
            edges,
        }
    }

    /// Get the total number of components (including the instance root).
    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Get the total number of edges.
    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    // ========================================================================
    // High-Level Registration (Source of Truth)
    // ========================================================================

    /// Register a source component in the graph.
    ///
    /// Creates the node and bidirectional ownership edges transactionally.
    /// Events are emitted only on successful commit.
    ///
    /// # Errors
    ///
    /// Returns an error if a component with the same ID already exists.
    pub fn register_source(
        &mut self,
        id: &str,
        metadata: HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let node = ComponentNode {
            id: id.to_string(),
            kind: ComponentKind::Source,
            status: ComponentStatus::Added,
            metadata,
        };
        let mut txn = self.begin();
        txn.add_component(node)?;
        txn.commit();
        Ok(())
    }

    /// Register a query component with its source dependencies.
    ///
    /// Creates the node, ownership edges, and `Feeds` edges from each source.
    /// All operations are transactional — if any dependency is missing or any
    /// step fails, the entire registration is rolled back.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A component with the same ID already exists
    /// - Any referenced source does not exist in the graph
    pub fn register_query(
        &mut self,
        id: &str,
        metadata: HashMap<String, String>,
        source_ids: &[String],
    ) -> anyhow::Result<()> {
        // Validate all dependencies exist before starting the transaction
        for source_id in source_ids {
            if !self.contains(source_id) {
                return Err(anyhow::anyhow!(
                    "Cannot register query '{id}': referenced source '{source_id}' does not exist in the graph"
                ));
            }
        }

        let node = ComponentNode {
            id: id.to_string(),
            kind: ComponentKind::Query,
            status: ComponentStatus::Added,
            metadata,
        };
        let mut txn = self.begin();
        txn.add_component(node)?;
        for source_id in source_ids {
            txn.add_relationship(source_id, id, RelationshipKind::Feeds)?;
        }
        txn.commit();
        Ok(())
    }

    /// Register a reaction component with its query dependencies.
    ///
    /// Creates the node, ownership edges, and `Feeds` edges from each query.
    /// All operations are transactional — if any dependency is missing or any
    /// step fails, the entire registration is rolled back.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A component with the same ID already exists
    /// - Any referenced query does not exist in the graph
    pub fn register_reaction(
        &mut self,
        id: &str,
        metadata: HashMap<String, String>,
        query_ids: &[String],
    ) -> anyhow::Result<()> {
        // Validate all dependencies exist before starting the transaction
        for query_id in query_ids {
            if !self.contains(query_id) {
                return Err(anyhow::anyhow!(
                    "Cannot register reaction '{id}': referenced query '{query_id}' does not exist in the graph"
                ));
            }
        }

        let node = ComponentNode {
            id: id.to_string(),
            kind: ComponentKind::Reaction,
            status: ComponentStatus::Added,
            metadata,
        };
        let mut txn = self.begin();
        txn.add_component(node)?;
        for query_id in query_ids {
            txn.add_relationship(query_id, id, RelationshipKind::Feeds)?;
        }
        txn.commit();
        Ok(())
    }

    /// Deregister a component and all its edges from the graph.
    ///
    /// Validates that the component exists and has no dependents before removal.
    /// The instance root node cannot be deregistered.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The component does not exist
    /// - The component has dependents (use `can_remove()` to check first)
    /// - The component is the instance root node
    pub fn deregister(&mut self, id: &str) -> anyhow::Result<ComponentNode> {
        // Validate no dependents
        if let Err(dependent_ids) = self.can_remove(id) {
            return Err(anyhow::anyhow!(
                "Cannot deregister '{}': depended on by: {}",
                id,
                dependent_ids.join(", ")
            ));
        }
        self.remove_component(id)
    }

    /// Register a bootstrap provider in the graph for topology visibility.
    ///
    /// Creates the node and bidirectional ownership edges transactionally.
    /// Optionally links the provider to its target source via `Bootstraps` edges.
    ///
    /// # Usage
    ///
    /// Bootstrap providers are managed internally by source plugins (set via
    /// `Source::set_bootstrap_provider()`). Call this method to make a bootstrap
    /// provider visible in the component graph for topology visualization and
    /// dependency tracking.
    ///
    /// ```ignore
    /// // After adding a source with a bootstrap provider:
    /// let mut graph = core.component_graph().write().await;
    /// graph.register_bootstrap_provider("my-bootstrap", metadata, &["my-source".into()])?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A component with the same ID already exists
    /// - Any referenced source does not exist in the graph
    pub fn register_bootstrap_provider(
        &mut self,
        id: &str,
        metadata: HashMap<String, String>,
        source_ids: &[String],
    ) -> anyhow::Result<()> {
        for source_id in source_ids {
            if !self.contains(source_id) {
                return Err(anyhow::anyhow!(
                    "Cannot register bootstrap provider '{id}': referenced source '{source_id}' does not exist in the graph"
                ));
            }
        }

        let node = ComponentNode {
            id: id.to_string(),
            kind: ComponentKind::BootstrapProvider,
            status: ComponentStatus::Added,
            metadata,
        };
        let mut txn = self.begin();
        txn.add_component(node)?;
        for source_id in source_ids {
            txn.add_relationship(id, source_id, RelationshipKind::Bootstraps)?;
        }
        txn.commit();
        Ok(())
    }

    /// Register an identity provider in the graph for topology visibility.
    ///
    /// Creates the node and bidirectional ownership edges transactionally.
    /// Optionally links the provider to components it authenticates via
    /// `Authenticates` edges.
    ///
    /// # Current Status
    ///
    /// This method is **reserved for future use**. Identity provider support
    /// is not yet implemented in the component lifecycle. The registration
    /// infrastructure is in place for when authentication integration is added.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A component with the same ID already exists
    /// - Any referenced component does not exist in the graph
    pub fn register_identity_provider(
        &mut self,
        id: &str,
        metadata: HashMap<String, String>,
        component_ids: &[String],
    ) -> anyhow::Result<()> {
        for component_id in component_ids {
            if !self.contains(component_id) {
                return Err(anyhow::anyhow!(
                    "Cannot register identity provider '{id}': referenced component '{component_id}' does not exist in the graph"
                ));
            }
        }

        let node = ComponentNode {
            id: id.to_string(),
            kind: ComponentKind::IdentityProvider,
            status: ComponentStatus::Added,
            metadata,
        };
        let mut txn = self.begin();
        txn.add_component(node)?;
        for component_id in component_ids {
            txn.add_relationship(id, component_id, RelationshipKind::Authenticates)?;
        }
        txn.commit();
        Ok(())
    }

    // ========================================================================
    // Transactions
    // ========================================================================

    /// Begin a transactional mutation of the graph.
    ///
    /// Returns a [`GraphTransaction`] that collects mutations (nodes, edges)
    /// and defers event emission until [`commit()`](GraphTransaction::commit).
    /// If the transaction is dropped without being committed, all added nodes
    /// and edges are rolled back automatically.
    ///
    /// The `&mut self` borrow ensures compile-time exclusivity — no other code
    /// can access the graph while a transaction is in progress.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut graph = self.graph.write().await;
    /// let mut txn = graph.begin();
    /// txn.add_component(source_node)?;
    /// txn.add_relationship("source-1", "query-1", RelationshipKind::Feeds)?;
    /// txn.commit(); // events emitted here; if this line is not reached, rollback on drop
    /// ```
    pub fn begin(&mut self) -> GraphTransaction<'_> {
        GraphTransaction::new(self)
    }

    // ========================================================================
    // Event History (centralized)
    // ========================================================================

    /// Record a component event in the centralized history.
    ///
    /// Called internally by [`apply_update()`]. Managers should NOT call this
    /// directly — status updates flow through the mpsc channel and are recorded
    /// automatically.
    pub fn record_event(&mut self, event: ComponentEvent) {
        self.event_history.record_event(event);
    }

    /// Get all lifecycle events for a specific component.
    ///
    /// Returns events in chronological order (oldest first).
    /// Up to 100 most recent events are retained per component.
    pub fn get_events(&self, component_id: &str) -> Vec<ComponentEvent> {
        self.event_history.get_events(component_id)
    }

    /// Get all lifecycle events across all components.
    ///
    /// Returns events sorted by timestamp (oldest first).
    pub fn get_all_events(&self) -> Vec<ComponentEvent> {
        self.event_history.get_all_events()
    }

    /// Get the most recent error message for a component.
    pub fn get_last_error(&self, component_id: &str) -> Option<String> {
        self.event_history.get_last_error(component_id)
    }

    /// Subscribe to live lifecycle events for a component.
    ///
    /// Returns the current history and a broadcast receiver for new events.
    /// Creates the component's event channel if it doesn't exist.
    pub fn subscribe_events(
        &mut self,
        component_id: &str,
    ) -> (Vec<ComponentEvent>, broadcast::Receiver<ComponentEvent>) {
        self.event_history.subscribe(component_id)
    }
}

// ============================================================================
// Relationship Validation
// ============================================================================

/// Check if a relationship kind is semantically valid between two component kinds.
///
/// This enforces the graph topology rules:
/// - **Feeds**: Source → Query, or Query → Reaction
/// - **Owns/OwnedBy**: Instance ↔ any component (created automatically)
/// - **Bootstraps**: BootstrapProvider → Source
/// - **Authenticates**: IdentityProvider → any component
pub(super) fn is_valid_relationship(
    from_kind: &ComponentKind,
    to_kind: &ComponentKind,
    relationship: &RelationshipKind,
) -> bool {
    use ComponentKind::*;
    use RelationshipKind::*;
    matches!(
        (from_kind, to_kind, relationship),
        // Data flow
        (Source, Query, Feeds)
            | (Query, Reaction, Feeds)
            // Ownership (auto-created by add_component_internal)
            | (Instance, _, Owns)
            // Bootstrap
            | (BootstrapProvider, Source, Bootstraps)
            // Authentication
            | (IdentityProvider, _, Authenticates)
    )
}

// ============================================================================
// State Transition Validation
// ============================================================================

/// Check if a status transition is valid according to the component lifecycle state machine.
///
/// ```text
/// Added ──→ Starting ──→ Running ──→ Stopping ──→ Stopped
///   │           │            │            │
///   │           ↓            ↓            ↓
///   │         Error        Error        Error
///   │           │
///   │           ↓
///   │        Stopped (aborted start)
///   │
///   ↓
/// Reconfiguring ──→ Stopped | Starting | Error
///
/// Error ──→ Starting (retry) | Stopped (reset)
///
/// Note: Added and Removed are set by the graph on add/remove_component()
/// and are NOT valid targets for validate_and_transition().
/// ```
pub(super) fn is_valid_transition(from: &ComponentStatus, to: &ComponentStatus) -> bool {
    use ComponentStatus::*;
    matches!(
        (from, to),
        // Normal lifecycle
        (Added, Starting)
            | (Added, Stopped) // immediate deactivation without starting
            | (Stopped, Starting)
            | (Starting, Running)
            | (Starting, Error)
            | (Starting, Stopped) // aborted start
            | (Running, Stopping)
            | (Running, Stopped) // direct stop (async channel may skip Stopping)
            | (Running, Error)
            | (Stopping, Stopped)
            | (Stopping, Error)
            // Error recovery
            | (Error, Starting) // retry
            | (Error, Stopped) // reset
            // Reconfiguration (from any stable state)
            | (Added, Reconfiguring)
            | (Stopped, Reconfiguring)
            | (Running, Reconfiguring)
            | (Error, Reconfiguring)
            | (Reconfiguring, Stopped)
            | (Reconfiguring, Starting)
            | (Reconfiguring, Error)
    )
}

/// Produce a human-readable error message for an invalid transition.
///
/// These messages provide actionable feedback (e.g., "Component is already running"
/// instead of just "invalid transition").
fn describe_invalid_transition(id: &str, from: &ComponentStatus, to: &ComponentStatus) -> String {
    use ComponentStatus::*;
    match (from, to) {
        // Trying to start something that's already starting/running
        (Starting, Starting) => format!("Component '{id}' is already starting"),
        (Running, Starting) => format!("Component '{id}' is already running"),
        (Stopping, Starting) => {
            format!("Cannot start component '{id}' while it is stopping")
        }
        (Reconfiguring, Starting) => {
            // Reconfiguring → Starting is actually valid, so this shouldn't be reached,
            // but kept for safety
            format!("Cannot start component '{id}' while it is reconfiguring")
        }
        // Trying to stop something that's already stopped/stopping
        (Stopped, Stopping) => {
            format!("Cannot stop component '{id}': it is already stopped")
        }
        (Stopping, Stopping) => format!("Component '{id}' is already stopping"),
        (Error, Stopping) => {
            format!("Cannot stop component '{id}': it is in error state")
        }
        // Trying to reconfigure during a transition
        (Starting, Reconfiguring) => {
            format!("Cannot reconfigure component '{id}' while it is starting")
        }
        (Stopping, Reconfiguring) => {
            format!("Cannot reconfigure component '{id}' while it is stopping")
        }
        (Reconfiguring, Reconfiguring) => {
            format!("Component '{id}' is already reconfiguring")
        }
        // Generic fallback
        _ => format!("Invalid state transition for component '{id}': {from:?} → {to:?}"),
    }
}

impl std::fmt::Debug for ComponentGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentGraph")
            .field("instance_id", &self.instance_id())
            .field("node_count", &self.node_count())
            .field("edge_count", &self.edge_count())
            .finish()
    }
}
