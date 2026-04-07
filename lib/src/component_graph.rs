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

//! Component Dependency Graph — the single source of truth for DrasiLib configuration.
//!
//! The `ComponentGraph` maintains a directed graph of all components (Instance, Sources,
//! Queries, Reactions, BootstrapProviders, IdentityProviders) and their bidirectional
//! relationships. The DrasiLib instance itself is the root node.
//!
//! All three managers (SourceManager, QueryManager, ReactionManager) share the same
//! `Arc<RwLock<ComponentGraph>>`. Managers register components in the graph **first**
//! (registry-first pattern), then create and store runtime instances. The graph is the
//! authoritative source for component relationships, dependency tracking, and lifecycle
//! events.
//!
//! # Event Emission
//!
//! The graph emits [`ComponentEvent`]s via a built-in `broadcast::Sender` whenever
//! components are added, removed, or change status. Subscribers (ComponentGraphSource,
//! EventHistory, external consumers) receive events directly from the graph.
//!
//! # Status Update Channel
//!
//! Components report status changes via a shared `mpsc::Sender<ComponentUpdate>`.
//! A dedicated graph update loop (spawned externally) receives from this channel and
//! applies updates to the graph, emitting broadcast events. This decouples components
//! from the graph lock — status reporting is fire-and-forget.
//!
//! Structural mutations (`add_component`, `remove_component`, `add_relationship`) and
//! command-initiated transitions (`Starting`, `Stopping`) are applied directly by
//! managers, which hold the graph write lock on the cold path.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use petgraph::stable_graph::{EdgeIndex, NodeIndex, StableGraph};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, Notify, RwLock};

use crate::channels::{
    ComponentEvent, ComponentEventBroadcastReceiver, ComponentStatus, ComponentType,
};
use crate::managers::ComponentEventHistory;

// ============================================================================
// Component Update Messages (mpsc fan-in from components to graph)
// ============================================================================

/// A status or metric update sent from a component to the graph update loop.
///
/// Components hold a cloned `mpsc::Sender<ComponentUpdate>` and call `send()` or
/// `try_send()` to report status changes without acquiring any locks. The graph
/// update loop is the sole consumer.
#[derive(Debug, Clone)]
pub enum ComponentUpdate {
    /// A component status change (e.g., Running, Error, Stopped).
    Status {
        /// The component ID reporting the update
        component_id: String,
        /// The new status
        status: ComponentStatus,
        /// Optional human-readable message
        message: Option<String>,
    },
    // Future variants:
    // Metric { component_id: String, name: String, value: f64 },
    // LifecycleTransition { component_id: String, from: ComponentStatus, to: ComponentStatus },
}

/// Sender half of the component update channel.
///
/// Cloned and given to each component's `SourceBase`/`ReactionBase`/`QueryBase`.
/// Fire-and-forget: `try_send()` never blocks.
pub type ComponentUpdateSender = mpsc::Sender<ComponentUpdate>;

/// Receiver half of the component update channel.
///
/// Owned by the graph update loop task, which is the sole consumer.
pub type ComponentUpdateReceiver = mpsc::Receiver<ComponentUpdate>;

/// A clonable handle for reading, writing, and reporting component status.
///
/// `ComponentStatusHandle` owns the component's local status (`Arc<RwLock<ComponentStatus>>`)
/// **and** the mpsc sender to the graph update loop. This means a single cloned handle is
/// all a spawned task needs to both read the current status and update it (with automatic
/// graph notification).
///
/// # Obtaining a handle
///
/// Source and reaction plugins obtain this from their base class:
/// ```ignore
/// let handle = self.base.status_handle();
/// ```
///
/// # Usage in spawned tasks
///
/// ```ignore
/// let handle = self.base.status_handle();
/// tokio::spawn(async move {
///     let current = handle.get_status().await;
///     if let Err(e) = do_work().await {
///         handle.set_status(ComponentStatus::Error, Some(format!("Failed: {e}"))).await;
///     }
/// });
/// ```
#[derive(Clone)]
pub struct ComponentStatusHandle {
    component_id: String,
    status: Arc<RwLock<ComponentStatus>>,
    update_tx: Arc<tokio::sync::OnceCell<ComponentUpdateSender>>,
}

impl ComponentStatusHandle {
    /// Create a handle without a graph channel.
    ///
    /// The handle is fully functional for local status reads/writes immediately.
    /// Call [`wire`] later to connect to the graph update loop.
    pub fn new(component_id: impl Into<String>) -> Self {
        Self {
            component_id: component_id.into(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            update_tx: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Create a handle already connected to the graph update loop.
    ///
    /// Use this when the update channel is available at construction time
    /// (e.g., in `QueryManager` where queries are created with full context).
    pub fn new_wired(component_id: impl Into<String>, update_tx: ComponentUpdateSender) -> Self {
        let cell = tokio::sync::OnceCell::new();
        let _ = cell.set(update_tx);
        Self {
            component_id: component_id.into(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            update_tx: Arc::new(cell),
        }
    }

    /// Connect this handle to the graph update loop.
    ///
    /// After wiring, every [`set_status`](Self::set_status) call will also
    /// send a fire-and-forget notification to the graph.
    pub async fn wire(&self, update_tx: ComponentUpdateSender) {
        let _ = self.update_tx.set(update_tx);
    }

    /// Set the component's status — updates local state AND notifies the graph.
    ///
    /// This is the single canonical way to change a component's status. It writes
    /// to the local `Arc<RwLock<ComponentStatus>>` and sends the update to the
    /// graph update loop (if wired). The send awaits backpressure if the channel
    /// is full, ensuring status transitions are never silently dropped.
    pub async fn set_status(&self, status: ComponentStatus, message: Option<String>) {
        // Update local state first, then release the lock before sending
        // to avoid holding the RwLock during a potential channel backpressure wait.
        {
            *self.status.write().await = status;
        }

        if let Some(tx) = self.update_tx.get() {
            if let Err(e) = tx
                .send(ComponentUpdate::Status {
                    component_id: self.component_id.clone(),
                    status,
                    message,
                })
                .await
            {
                log::warn!(
                    "Status update for '{}' dropped (channel closed): {e}",
                    self.component_id
                );
            }
        }
    }

    /// Read the current status.
    pub async fn get_status(&self) -> ComponentStatus {
        *self.status.read().await
    }
}

// ============================================================================
// Graph Type Definitions
// ============================================================================

/// Type of component in the graph
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ComponentKind {
    /// The DrasiLib instance itself (root node)
    Instance,
    /// A data source
    Source,
    /// A continuous query
    Query,
    /// A reaction/output
    Reaction,
    /// A bootstrap provider
    BootstrapProvider,
    /// An identity provider
    IdentityProvider,
}

impl std::fmt::Display for ComponentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComponentKind::Instance => write!(f, "instance"),
            ComponentKind::Source => write!(f, "source"),
            ComponentKind::Query => write!(f, "query"),
            ComponentKind::Reaction => write!(f, "reaction"),
            ComponentKind::BootstrapProvider => write!(f, "bootstrap_provider"),
            ComponentKind::IdentityProvider => write!(f, "identity_provider"),
        }
    }
}

impl ComponentKind {
    /// Convert to [`ComponentType`] for event emission.
    ///
    /// Returns `None` only for the Instance kind (the root node), which has no
    /// corresponding event type. All other component kinds map to a `ComponentType`.
    pub fn to_component_type(&self) -> Option<ComponentType> {
        match self {
            ComponentKind::Source => Some(ComponentType::Source),
            ComponentKind::Query => Some(ComponentType::Query),
            ComponentKind::Reaction => Some(ComponentType::Reaction),
            ComponentKind::BootstrapProvider => Some(ComponentType::BootstrapProvider),
            ComponentKind::IdentityProvider => Some(ComponentType::IdentityProvider),
            ComponentKind::Instance => None,
        }
    }
}

/// Type of relationship between components (bidirectional pairs)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum RelationshipKind {
    // Ownership
    /// Instance → Component
    Owns,
    /// Component → Instance
    OwnedBy,

    // Data flow
    /// Source → Query, or Query → Reaction
    Feeds,
    /// Query → Source, or Reaction → Query
    SubscribesTo,

    // Bootstrap
    /// BootstrapProvider → Source
    Bootstraps,
    /// Source → BootstrapProvider
    BootstrappedBy,

    // Authentication
    /// IdentityProvider → Source/Reaction
    Authenticates,
    /// Source/Reaction → IdentityProvider
    AuthenticatedBy,
}

impl RelationshipKind {
    /// Get the reverse of this relationship
    pub fn reverse(&self) -> Self {
        match self {
            RelationshipKind::Owns => RelationshipKind::OwnedBy,
            RelationshipKind::OwnedBy => RelationshipKind::Owns,
            RelationshipKind::Feeds => RelationshipKind::SubscribesTo,
            RelationshipKind::SubscribesTo => RelationshipKind::Feeds,
            RelationshipKind::Bootstraps => RelationshipKind::BootstrappedBy,
            RelationshipKind::BootstrappedBy => RelationshipKind::Bootstraps,
            RelationshipKind::Authenticates => RelationshipKind::AuthenticatedBy,
            RelationshipKind::AuthenticatedBy => RelationshipKind::Authenticates,
        }
    }
}

// ============================================================================
// Graph Node Data
// ============================================================================

/// A node in the component graph.
///
/// Contains metadata (ID, kind, status, properties) for each component.
/// Runtime component instances (`Arc<dyn Source>`, etc.) are stored in the
/// managers' HashMaps alongside the graph, keyed by the same component ID.
/// The graph and HashMaps are always updated together in the same code paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentNode {
    /// Unique identifier for this component
    pub id: String,
    /// Type of component
    pub kind: ComponentKind,
    /// Current lifecycle status
    pub status: ComponentStatus,
    /// Optional metadata (e.g., source_type, query_language, reaction_type)
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

// ============================================================================
// Serializable Graph Snapshot
// ============================================================================

/// Serializable snapshot of the entire component graph.
///
/// Used for API responses, UI visualization, and debugging.
/// Contains only metadata — no runtime component instances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSnapshot {
    /// The instance ID (root node)
    pub instance_id: String,
    /// All nodes in the graph
    pub nodes: Vec<ComponentNode>,
    /// All edges in the graph
    pub edges: Vec<GraphEdge>,
}

/// A serializable edge in the graph snapshot
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Source component ID
    pub from: String,
    /// Target component ID
    pub to: String,
    /// Type of relationship
    pub relationship: RelationshipKind,
}

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
    graph: StableGraph<ComponentNode, RelationshipKind>,
    /// Fast lookup: component ID → node index (O(1) access)
    index: HashMap<String, NodeIndex>,
    /// The instance node index (always present)
    instance_idx: NodeIndex,
    /// Broadcast sender for component lifecycle events (fan-out to subscribers).
    /// Events are emitted by `add_component()`, `remove_component()`, `update_status()`,
    /// and the graph update loop.
    event_tx: broadcast::Sender<ComponentEvent>,
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
    fn add_component_internal(
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
    /// Emits a [`ComponentEvent`] with status [`ComponentStatus::Stopped`] to all
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
            ComponentStatus::Stopped,
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
    fn update_status(
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
    fn add_relationship_internal(
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
            status: ComponentStatus::Stopped,
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
            status: ComponentStatus::Stopped,
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
            status: ComponentStatus::Stopped,
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
            status: ComponentStatus::Stopped,
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
            status: ComponentStatus::Stopped,
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
        GraphTransaction {
            graph: self,
            added_nodes: Vec::new(),
            added_edges: Vec::new(),
            pending_events: Vec::new(),
            committed: false,
        }
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
// Async Status Waiter
// ============================================================================

/// Wait for a component to reach one of the target statuses, with a timeout.
///
/// This replaces polling loops that use `sleep()` + status check. It uses the
/// graph's [`Notify`] to wake up only when a status actually changes, avoiding
/// busy-waiting.
///
/// # Pattern
///
/// Uses the same register-before-check pattern as `PriorityQueue::enqueue_wait()`:
/// 1. Register `notified()` interest
/// 2. Acquire read lock, check condition
/// 3. If not met, release lock and await notification
/// 4. Repeat until condition met or timeout
///
/// # Arguments
///
/// * `graph` — The shared graph handle
/// * `component_id` — ID of the component to watch
/// * `target_statuses` — One or more acceptable statuses to wait for
/// * `timeout` — Maximum time to wait before returning an error
///
/// # Errors
///
/// Returns an error if the timeout expires before the component reaches any
/// target status, or if the component is not found in the graph.
pub async fn wait_for_status(
    graph: &Arc<RwLock<ComponentGraph>>,
    component_id: &str,
    target_statuses: &[ComponentStatus],
    timeout: std::time::Duration,
) -> anyhow::Result<ComponentStatus> {
    let deadline = tokio::time::Instant::now() + timeout;

    // Get the Notify handle once (doesn't require holding the lock)
    let notify = {
        let g = graph.read().await;
        g.status_notifier()
    };

    loop {
        // Register interest BEFORE checking condition (avoid race)
        let notified = notify.notified();

        // Check current status under read lock
        {
            let g = graph.read().await;
            if let Some(node) = g.get_component(component_id) {
                if target_statuses.contains(&node.status) {
                    return Ok(node.status);
                }
            } else {
                return Err(anyhow::anyhow!(
                    "Component '{component_id}' not found in graph"
                ));
            }
        }
        // Lock released

        // Wait for a status change or timeout
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow::anyhow!(
                "Timed out waiting for component '{component_id}' to reach {target_statuses:?}",
            ));
        }

        tokio::select! {
            _ = notified => {
                // A status changed somewhere — loop back and re-check
            }
            _ = tokio::time::sleep(remaining) => {
                return Err(anyhow::anyhow!(
                    "Timed out waiting for component '{component_id}' to reach {target_statuses:?}",
                ));
            }
        }
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
fn is_valid_relationship(
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
/// Stopped ──→ Starting ──→ Running ──→ Stopping ──→ Stopped
///    │            │            │            │
///    │            ↓            ↓            ↓
///    │          Error        Error        Error
///    │            │
///    │            ↓
///    │         Stopped (aborted start)
///    │
///    ↓
/// Reconfiguring ──→ Stopped | Starting | Error
///
/// Error ──→ Starting (retry) | Stopped (reset)
/// ```
fn is_valid_transition(from: &ComponentStatus, to: &ComponentStatus) -> bool {
    use ComponentStatus::*;
    matches!(
        (from, to),
        // Normal lifecycle
        (Stopped, Starting)
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

// ============================================================================
// Graph Transaction
// ============================================================================

/// A transactional wrapper for batching graph mutations.
///
/// Collects added nodes and edges, deferring event emission until [`commit()`](Self::commit).
/// If dropped without commit, all additions are rolled back (nodes and edges removed).
///
/// The `&'g mut ComponentGraph` borrow ensures Rust's borrow checker prevents
/// any concurrent access to the graph during the transaction — zero-cost safety.
///
/// # Cross-system usage
///
/// For operations that span the graph and external systems (runtime initialization,
/// HashMap insertion), use this pattern:
///
/// ```ignore
/// // Phase 1: Graph transaction
/// {
///     let mut graph = self.graph.write().await;
///     let mut txn = graph.begin();
///     txn.add_component(node)?;
///     txn.commit(); // graph is consistent
/// }
///
/// // Phase 2: Runtime init (no graph lock)
/// let instance = match RuntimeType::new(...) {
///     Ok(i) => i,
///     Err(e) => {
///         // Compensating rollback
///         let mut graph = self.graph.write().await;
///         let _ = graph.remove_component(&id);
///         return Err(e);
///     }
/// };
/// ```
pub struct GraphTransaction<'g> {
    graph: &'g mut ComponentGraph,
    added_nodes: Vec<(NodeIndex, String)>,
    added_edges: Vec<EdgeIndex>,
    pending_events: Vec<ComponentEvent>,
    committed: bool,
}

impl<'g> GraphTransaction<'g> {
    /// Add a component node to the graph within this transaction.
    ///
    /// The node is added immediately (so subsequent `add_relationship` calls
    /// can reference it), but the event is deferred until `commit()`.
    /// On rollback (drop without commit), the node and its ownership edges
    /// are removed.
    pub fn add_component(&mut self, node: ComponentNode) -> anyhow::Result<NodeIndex> {
        let id = node.id.clone();
        let (node_idx, event, owns_edge, owned_by_edge) =
            self.graph.add_component_internal(node)?;
        self.added_nodes.push((node_idx, id));

        // Record the ownership edges for rollback
        self.added_edges.push(owns_edge);
        self.added_edges.push(owned_by_edge);

        if let Some(event) = event {
            self.pending_events.push(event);
        }

        Ok(node_idx)
    }

    /// Add a bidirectional relationship within this transaction.
    ///
    /// The edges are added immediately; on rollback they are removed.
    /// Idempotent: if the relationship already exists, this is a no-op.
    pub fn add_relationship(
        &mut self,
        from_id: &str,
        to_id: &str,
        forward: RelationshipKind,
    ) -> anyhow::Result<()> {
        let (fwd, rev) = self
            .graph
            .add_relationship_internal(from_id, to_id, forward)?;
        if let Some(e) = fwd {
            self.added_edges.push(e);
        }
        if let Some(e) = rev {
            self.added_edges.push(e);
        }
        Ok(())
    }

    /// Commit the transaction: emit all deferred events.
    ///
    /// After commit, the mutations are permanent and cannot be rolled back
    /// through this transaction. For cross-system rollback, use compensating
    /// actions (e.g., `graph.remove_component()`).
    pub fn commit(mut self) {
        self.committed = true;
        for event in &self.pending_events {
            let _ = self.graph.event_tx.send(event.clone());
        }
    }
}

impl<'g> Drop for GraphTransaction<'g> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        // Rollback: remove all added edges first (reverse order to maintain index stability)
        for &edge_idx in self.added_edges.iter().rev() {
            self.graph.graph.remove_edge(edge_idx);
        }

        // Remove all added nodes (reverse order)
        for (node_idx, ref id) in self.added_nodes.iter().rev() {
            self.graph.index.remove(id.as_str());
            self.graph.graph.remove_node(*node_idx);
        }
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_graph() -> ComponentGraph {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        graph
    }

    fn source_node(id: &str) -> ComponentNode {
        ComponentNode {
            id: id.to_string(),
            kind: ComponentKind::Source,
            status: ComponentStatus::Stopped,
            metadata: HashMap::new(),
        }
    }

    fn query_node(id: &str) -> ComponentNode {
        ComponentNode {
            id: id.to_string(),
            kind: ComponentKind::Query,
            status: ComponentStatus::Stopped,
            metadata: HashMap::new(),
        }
    }

    fn reaction_node(id: &str) -> ComponentNode {
        ComponentNode {
            id: id.to_string(),
            kind: ComponentKind::Reaction,
            status: ComponentStatus::Stopped,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn test_new_graph_has_instance_root() {
        let graph = create_test_graph();
        assert_eq!(graph.instance_id(), "test-instance");
        assert_eq!(graph.node_count(), 1);
        assert!(graph.contains("test-instance"));
    }

    #[test]
    fn test_add_component() {
        let mut graph = create_test_graph();
        let result = graph.add_component(source_node("source-1"));
        assert!(result.is_ok());
        assert_eq!(graph.node_count(), 2);
        assert!(graph.contains("source-1"));

        // Should have 2 edges: Instance--Owns-->Source, Source--OwnedBy-->Instance
        assert_eq!(graph.edge_count(), 2);
    }

    #[test]
    fn test_add_duplicate_component_fails() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        let result = graph.add_component(source_node("source-1"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_remove_component() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        assert_eq!(graph.node_count(), 2);

        let removed = graph.remove_component("source-1").unwrap();
        assert_eq!(removed.id, "source-1");
        assert_eq!(graph.node_count(), 1);
        assert!(!graph.contains("source-1"));
        // Edges should be removed too
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn test_cannot_remove_instance_root() {
        let mut graph = create_test_graph();
        let result = graph.remove_component("test-instance");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("instance root"));
    }

    #[test]
    fn test_remove_nonexistent_fails() {
        let mut graph = create_test_graph();
        let result = graph.remove_component("nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_bidirectional_relationship() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();

        // Add Source --Feeds--> Query (and Query --SubscribesTo--> Source)
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();

        // 4 Owns/OwnedBy edges + 2 Feeds/SubscribesTo edges = 6
        assert_eq!(graph.edge_count(), 6);

        // Source feeds query
        let dependents = graph.get_dependents("source-1");
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].id, "query-1");

        // Query subscribes to source
        let dependencies = graph.get_dependencies("query-1");
        assert_eq!(dependencies.len(), 1);
        assert_eq!(dependencies[0].id, "source-1");
    }

    #[test]
    fn test_can_remove_with_dependents() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();

        // Cannot remove source with dependent query
        let result = graph.can_remove("source-1");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), vec!["query-1"]);

        // Can remove query (no dependents)
        assert!(graph.can_remove("query-1").is_ok());
    }

    #[test]
    fn test_full_pipeline_source_query_reaction() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        graph.add_component(reaction_node("reaction-1")).unwrap();

        // Source --Feeds--> Query --Feeds--> Reaction
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        graph
            .add_relationship("query-1", "reaction-1", RelationshipKind::Feeds)
            .unwrap();

        // Source has query as dependent
        let source_deps = graph.get_dependents("source-1");
        assert_eq!(source_deps.len(), 1);
        assert_eq!(source_deps[0].id, "query-1");

        // Query has reaction as dependent
        let query_deps = graph.get_dependents("query-1");
        assert_eq!(query_deps.len(), 1);
        assert_eq!(query_deps[0].id, "reaction-1");

        // Reaction has no dependents
        assert!(graph.get_dependents("reaction-1").is_empty());

        // Reaction depends on query
        let reaction_deps = graph.get_dependencies("reaction-1");
        assert_eq!(reaction_deps.len(), 1);
        assert_eq!(reaction_deps[0].id, "query-1");
    }

    #[test]
    fn test_update_status() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();

        assert_eq!(
            graph.get_component("source-1").unwrap().status,
            ComponentStatus::Stopped
        );

        // Follow valid transitions: Stopped → Starting → Running
        graph
            .update_status("source-1", ComponentStatus::Starting)
            .unwrap();

        assert_eq!(
            graph.get_component("source-1").unwrap().status,
            ComponentStatus::Starting
        );

        graph
            .update_status("source-1", ComponentStatus::Running)
            .unwrap();

        assert_eq!(
            graph.get_component("source-1").unwrap().status,
            ComponentStatus::Running
        );
    }

    #[test]
    fn test_list_by_kind() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(source_node("source-2")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();

        let sources = graph.list_by_kind(&ComponentKind::Source);
        assert_eq!(sources.len(), 2);

        let queries = graph.list_by_kind(&ComponentKind::Query);
        assert_eq!(queries.len(), 1);

        let reactions = graph.list_by_kind(&ComponentKind::Reaction);
        assert!(reactions.is_empty());
    }

    #[test]
    fn test_snapshot() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();

        let snapshot = graph.snapshot();
        assert_eq!(snapshot.instance_id, "test-instance");
        assert_eq!(snapshot.nodes.len(), 3); // instance + source + query
        assert_eq!(snapshot.edges.len(), 6); // 4 Owns/OwnedBy + 2 Feeds/SubscribesTo
    }

    #[test]
    fn test_snapshot_serializes_to_json() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();

        let snapshot = graph.snapshot();
        let json = serde_json::to_string_pretty(&snapshot).unwrap();
        assert!(json.contains("test-instance"));
        assert!(json.contains("source-1"));
        assert!(json.contains("query-1"));
        assert!(json.contains("Feeds"));
        assert!(json.contains("SubscribesTo"));
    }

    #[test]
    fn test_topological_order() {
        let mut graph = create_test_graph();
        graph.add_component(reaction_node("reaction-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        graph.add_component(source_node("source-1")).unwrap();

        // Source --Feeds--> Query --Feeds--> Reaction
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        graph
            .add_relationship("query-1", "reaction-1", RelationshipKind::Feeds)
            .unwrap();

        let order = graph.topological_order().unwrap();
        let ids: Vec<&str> = order.iter().map(|n| n.id.as_str()).collect();

        // Source must come before Query, Query must come before Reaction
        let source_pos = ids.iter().position(|&id| id == "source-1").unwrap();
        let query_pos = ids.iter().position(|&id| id == "query-1").unwrap();
        let reaction_pos = ids.iter().position(|&id| id == "reaction-1").unwrap();
        assert!(source_pos < query_pos);
        assert!(query_pos < reaction_pos);
    }

    #[test]
    fn test_get_neighbors_by_relationship() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();

        // Source's Feeds neighbors = [query-1]
        let feeds = graph.get_neighbors("source-1", &RelationshipKind::Feeds);
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].id, "query-1");

        // Source's Owns neighbors = [] (only Instance owns things)
        let owns = graph.get_neighbors("source-1", &RelationshipKind::Owns);
        assert!(owns.is_empty());

        // Instance's Owns neighbors = [source-1, query-1]
        let instance_owns = graph.get_neighbors("test-instance", &RelationshipKind::Owns);
        assert_eq!(instance_owns.len(), 2);
    }

    #[test]
    fn test_multiple_sources_feed_same_query() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(source_node("source-2")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();

        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        graph
            .add_relationship("source-2", "query-1", RelationshipKind::Feeds)
            .unwrap();

        // Query depends on both sources
        let deps = graph.get_dependencies("query-1");
        assert_eq!(deps.len(), 2);
        let dep_ids: Vec<&str> = deps.iter().map(|n| n.id.as_str()).collect();
        assert!(dep_ids.contains(&"source-1"));
        assert!(dep_ids.contains(&"source-2"));
    }

    #[test]
    fn test_remove_component_cleans_up_edges() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();

        // 6 edges total
        assert_eq!(graph.edge_count(), 6);

        // Remove query — its edges should be cleaned up
        graph.remove_component("query-1").unwrap();
        // Only 2 edges left: Instance--Owns/OwnedBy-->Source
        assert_eq!(graph.edge_count(), 2);

        // Source no longer has any dependents
        assert!(graph.get_dependents("source-1").is_empty());
    }

    // ====================================================================
    // Duplicate edge prevention tests
    // ====================================================================

    #[test]
    fn test_add_duplicate_relationship_is_idempotent() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();

        // First add: creates 2 new edges (Feeds + SubscribesTo)
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        assert_eq!(graph.edge_count(), 6); // 4 Owns/OwnedBy + 2 Feeds/SubscribesTo

        // Second add: no-op
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        assert_eq!(graph.edge_count(), 6); // unchanged
    }

    // ====================================================================
    // Remove relationship tests
    // ====================================================================

    #[test]
    fn test_remove_relationship() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        assert_eq!(graph.edge_count(), 6);

        // Remove the Feeds/SubscribesTo relationship
        graph
            .remove_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        // Only 4 Owns/OwnedBy edges remain
        assert_eq!(graph.edge_count(), 4);

        // No more dependents
        assert!(graph.get_dependents("source-1").is_empty());
        assert!(graph.get_dependencies("query-1").is_empty());
    }

    #[test]
    fn test_remove_nonexistent_relationship_is_noop() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        // No relationship added — removal should succeed as no-op
        graph
            .remove_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        assert_eq!(graph.edge_count(), 4);
    }

    // ====================================================================
    // Transaction tests
    // ====================================================================

    #[test]
    fn test_transaction_commit_emits_events() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        let mut event_rx = graph.subscribe();

        {
            let mut txn = graph.begin();
            txn.add_component(source_node("source-1")).unwrap();
            txn.add_component(query_node("query-1")).unwrap();
            txn.add_relationship("source-1", "query-1", RelationshipKind::Feeds)
                .unwrap();
            // Events not yet emitted
            assert!(event_rx.try_recv().is_err());
            txn.commit();
        }

        // After commit, events are emitted
        let e1 = event_rx.try_recv().unwrap();
        assert_eq!(e1.component_id, "source-1");
        let e2 = event_rx.try_recv().unwrap();
        assert_eq!(e2.component_id, "query-1");

        // Graph has the components
        assert!(graph.contains("source-1"));
        assert!(graph.contains("query-1"));
        assert_eq!(graph.get_dependents("source-1").len(), 1);
    }

    #[test]
    fn test_transaction_rollback_on_drop() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");

        {
            let mut txn = graph.begin();
            txn.add_component(source_node("source-1")).unwrap();
            txn.add_component(query_node("query-1")).unwrap();
            txn.add_relationship("source-1", "query-1", RelationshipKind::Feeds)
                .unwrap();
            // Drop without commit — rollback
        }

        // Components should not exist
        assert!(!graph.contains("source-1"));
        assert!(!graph.contains("query-1"));
        assert_eq!(graph.node_count(), 1); // only instance root
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn test_transaction_partial_failure_rollback() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");

        // Pre-add a source outside the transaction
        graph.add_component(source_node("source-1")).unwrap();
        assert_eq!(graph.node_count(), 2);

        {
            let mut txn = graph.begin();
            txn.add_component(query_node("query-1")).unwrap();
            // Try to add duplicate — this fails
            let result = txn.add_component(source_node("source-1"));
            assert!(result.is_err());
            // Drop without commit — query-1 should be rolled back
        }

        // source-1 still exists (added before transaction), query-1 does not
        assert!(graph.contains("source-1"));
        assert!(!graph.contains("query-1"));
        assert_eq!(graph.node_count(), 2);
    }

    #[test]
    fn test_valid_state_transitions() {
        assert!(is_valid_transition(
            &ComponentStatus::Stopped,
            &ComponentStatus::Starting
        ));
        assert!(is_valid_transition(
            &ComponentStatus::Starting,
            &ComponentStatus::Running
        ));
        assert!(is_valid_transition(
            &ComponentStatus::Running,
            &ComponentStatus::Stopping
        ));
        assert!(is_valid_transition(
            &ComponentStatus::Stopping,
            &ComponentStatus::Stopped
        ));
        assert!(is_valid_transition(
            &ComponentStatus::Starting,
            &ComponentStatus::Error
        ));
        assert!(is_valid_transition(
            &ComponentStatus::Running,
            &ComponentStatus::Error
        ));
        assert!(is_valid_transition(
            &ComponentStatus::Error,
            &ComponentStatus::Starting
        ));
        assert!(is_valid_transition(
            &ComponentStatus::Error,
            &ComponentStatus::Stopped
        ));
    }

    #[test]
    fn test_invalid_state_transitions() {
        assert!(!is_valid_transition(
            &ComponentStatus::Stopped,
            &ComponentStatus::Running
        ));
        assert!(!is_valid_transition(
            &ComponentStatus::Stopped,
            &ComponentStatus::Stopping
        ));
        assert!(!is_valid_transition(
            &ComponentStatus::Running,
            &ComponentStatus::Starting
        ));
        assert!(!is_valid_transition(
            &ComponentStatus::Starting,
            &ComponentStatus::Stopping
        ));
    }

    #[test]
    fn test_update_status_rejects_invalid_transition() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();

        // source-1 starts as Stopped
        assert_eq!(
            graph.get_component("source-1").unwrap().status,
            ComponentStatus::Stopped
        );

        // Invalid: Stopped → Running (must go through Starting)
        let result = graph.update_status("source-1", ComponentStatus::Running);
        assert!(result.is_ok());
        // The update should return None (skipped) and status should remain Stopped
        assert!(result.unwrap().is_none());
        assert_eq!(
            graph.get_component("source-1").unwrap().status,
            ComponentStatus::Stopped
        );

        // Valid: Stopped → Starting
        let result = graph.update_status("source-1", ComponentStatus::Starting);
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());
        assert_eq!(
            graph.get_component("source-1").unwrap().status,
            ComponentStatus::Starting
        );
    }

    // ====================================================================
    // Registration method tests
    // ====================================================================

    #[test]
    fn test_register_source() {
        let mut graph = create_test_graph();
        graph.register_source("source-1", HashMap::new()).unwrap();

        assert!(graph.contains("source-1"));
        assert_eq!(
            graph.get_component("source-1").unwrap().kind,
            ComponentKind::Source
        );
        assert_eq!(
            graph.get_component("source-1").unwrap().status,
            ComponentStatus::Stopped
        );
        // 2 ownership edges
        assert_eq!(graph.edge_count(), 2);
    }

    #[test]
    fn test_register_source_duplicate_fails() {
        let mut graph = create_test_graph();
        graph.register_source("source-1", HashMap::new()).unwrap();
        let result = graph.register_source("source-1", HashMap::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_register_query_with_sources() {
        let mut graph = create_test_graph();
        graph.register_source("source-1", HashMap::new()).unwrap();
        graph.register_source("source-2", HashMap::new()).unwrap();

        graph
            .register_query(
                "query-1",
                HashMap::new(),
                &["source-1".to_string(), "source-2".to_string()],
            )
            .unwrap();

        assert!(graph.contains("query-1"));
        let deps = graph.get_dependencies("query-1");
        assert_eq!(deps.len(), 2);
        assert_eq!(graph.get_dependents("source-1").len(), 1);
        assert_eq!(graph.get_dependents("source-2").len(), 1);
    }

    #[test]
    fn test_register_query_missing_source_fails() {
        let mut graph = create_test_graph();
        let result = graph.register_query("query-1", HashMap::new(), &["source-1".to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
        assert!(!graph.contains("query-1"));
    }

    #[test]
    fn test_register_reaction_with_queries() {
        let mut graph = create_test_graph();
        graph.register_source("source-1", HashMap::new()).unwrap();
        graph
            .register_query("query-1", HashMap::new(), &["source-1".to_string()])
            .unwrap();

        graph
            .register_reaction("reaction-1", HashMap::new(), &["query-1".to_string()])
            .unwrap();

        assert!(graph.contains("reaction-1"));
        assert_eq!(graph.get_dependents("query-1").len(), 1);
        assert_eq!(graph.get_dependencies("reaction-1").len(), 1);
    }

    #[test]
    fn test_register_reaction_missing_query_fails() {
        let mut graph = create_test_graph();
        let result = graph.register_reaction(
            "reaction-1",
            HashMap::new(),
            &["nonexistent-query".to_string()],
        );
        assert!(result.is_err());
        assert!(!graph.contains("reaction-1"));
    }

    #[test]
    fn test_deregister_succeeds_no_dependents() {
        let mut graph = create_test_graph();
        graph.register_source("source-1", HashMap::new()).unwrap();
        let removed = graph.deregister("source-1").unwrap();
        assert_eq!(removed.id, "source-1");
        assert!(!graph.contains("source-1"));
    }

    #[test]
    fn test_deregister_fails_with_dependents() {
        let mut graph = create_test_graph();
        graph.register_source("source-1", HashMap::new()).unwrap();
        graph
            .register_query("query-1", HashMap::new(), &["source-1".to_string()])
            .unwrap();

        let result = graph.deregister("source-1");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("depended on by"));
        assert!(graph.contains("source-1"));
    }

    #[test]
    fn test_full_pipeline_registration() {
        let mut graph = create_test_graph();
        graph.register_source("src", HashMap::new()).unwrap();
        graph
            .register_query("qry", HashMap::new(), &["src".to_string()])
            .unwrap();
        graph
            .register_reaction("rxn", HashMap::new(), &["qry".to_string()])
            .unwrap();

        assert_eq!(graph.get_dependents("src").len(), 1);
        assert_eq!(graph.get_dependents("qry").len(), 1);
        assert!(graph.get_dependents("rxn").is_empty());

        assert!(graph.deregister("src").is_err());
        graph.deregister("rxn").unwrap();
        graph.deregister("qry").unwrap();
        graph.deregister("src").unwrap();

        assert_eq!(graph.node_count(), 1);
    }

    // ========================================================================
    // validate_and_transition tests
    // ========================================================================

    #[test]
    fn test_validate_and_transition_stopped_to_starting() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("s1")).unwrap();

        let result = graph.validate_and_transition(
            "s1",
            ComponentStatus::Starting,
            Some("Starting source".into()),
        );
        assert!(result.is_ok());
        assert!(result.unwrap().is_some()); // event emitted
        assert_eq!(
            graph.get_component("s1").unwrap().status,
            ComponentStatus::Starting
        );
    }

    #[test]
    fn test_validate_and_transition_running_to_stopping() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("s1")).unwrap();
        // Move to Running first
        graph
            .validate_and_transition("s1", ComponentStatus::Starting, None)
            .unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Running, None)
            .unwrap();

        let result = graph.validate_and_transition(
            "s1",
            ComponentStatus::Stopping,
            Some("Stopping source".into()),
        );
        assert!(result.is_ok());
        assert_eq!(
            graph.get_component("s1").unwrap().status,
            ComponentStatus::Stopping
        );
    }

    #[test]
    fn test_validate_and_transition_idempotent_same_state() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("s1")).unwrap();

        let result = graph.validate_and_transition("s1", ComponentStatus::Stopped, None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none()); // no event for same-state
    }

    #[test]
    fn test_validate_and_transition_invalid_start_while_running() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("s1")).unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Starting, None)
            .unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Running, None)
            .unwrap();

        let result = graph.validate_and_transition("s1", ComponentStatus::Starting, None);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("already running"),
            "Expected 'already running' in: {err_msg}"
        );
        // Status unchanged
        assert_eq!(
            graph.get_component("s1").unwrap().status,
            ComponentStatus::Running
        );
    }

    #[test]
    fn test_validate_and_transition_invalid_stop_while_stopped() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("s1")).unwrap();

        let result = graph.validate_and_transition("s1", ComponentStatus::Stopping, None);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("already stopped"),
            "Expected 'already stopped' in: {err_msg}"
        );
    }

    #[test]
    fn test_validate_and_transition_error_recovery() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("s1")).unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Starting, None)
            .unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Error, None)
            .unwrap();

        // Error → Starting (retry) should work
        let result = graph.validate_and_transition("s1", ComponentStatus::Starting, None);
        assert!(result.is_ok());
        assert_eq!(
            graph.get_component("s1").unwrap().status,
            ComponentStatus::Starting
        );
    }

    #[test]
    fn test_validate_and_transition_reconfiguring() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("s1")).unwrap();

        // Stopped → Reconfiguring should work
        let result = graph.validate_and_transition("s1", ComponentStatus::Reconfiguring, None);
        assert!(result.is_ok());
        assert_eq!(
            graph.get_component("s1").unwrap().status,
            ComponentStatus::Reconfiguring
        );

        // Reconfiguring → Stopped should work
        let result = graph.validate_and_transition("s1", ComponentStatus::Stopped, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_and_transition_invalid_reconfig_while_stopping() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("s1")).unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Starting, None)
            .unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Running, None)
            .unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Stopping, None)
            .unwrap();

        let result = graph.validate_and_transition("s1", ComponentStatus::Reconfiguring, None);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("stopping"),
            "Expected 'stopping' in: {err_msg}"
        );
    }

    #[test]
    fn test_validate_and_transition_nonexistent_component() {
        let mut graph = create_test_graph();

        let result = graph.validate_and_transition("nonexistent", ComponentStatus::Starting, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_validate_and_transition_cannot_stop_error_state() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("s1")).unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Starting, None)
            .unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Error, None)
            .unwrap();

        let result = graph.validate_and_transition("s1", ComponentStatus::Stopping, None);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("error state"),
            "Expected 'error state' in: {err_msg}"
        );
    }

    // ========================================================================
    // register_bootstrap_provider tests
    // ========================================================================

    #[test]
    fn test_register_bootstrap_provider_standalone() {
        let mut graph = create_test_graph();
        graph
            .register_bootstrap_provider("bp-1", HashMap::new(), &[])
            .unwrap();

        assert!(graph.contains("bp-1"));
        assert_eq!(
            graph.get_component("bp-1").unwrap().kind,
            ComponentKind::BootstrapProvider
        );
        assert_eq!(
            graph.get_component("bp-1").unwrap().status,
            ComponentStatus::Stopped
        );
        // 2 ownership edges (Owns + OwnedBy)
        assert_eq!(graph.edge_count(), 2);
    }

    #[test]
    fn test_register_bootstrap_provider_with_source() {
        let mut graph = create_test_graph();
        graph.register_source("source-1", HashMap::new()).unwrap();
        graph
            .register_bootstrap_provider("bp-1", HashMap::new(), &["source-1".to_string()])
            .unwrap();

        assert!(graph.contains("bp-1"));
        // 2 ownership edges for source + 2 ownership edges for bp + 2 Bootstraps/BootstrappedBy edges
        assert_eq!(graph.edge_count(), 6);
    }

    #[test]
    fn test_register_bootstrap_provider_duplicate_fails() {
        let mut graph = create_test_graph();
        graph
            .register_bootstrap_provider("bp-1", HashMap::new(), &[])
            .unwrap();
        let result = graph.register_bootstrap_provider("bp-1", HashMap::new(), &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_register_bootstrap_provider_missing_source_fails() {
        let mut graph = create_test_graph();
        let result = graph.register_bootstrap_provider(
            "bp-1",
            HashMap::new(),
            &["nonexistent-source".to_string()],
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
        assert!(!graph.contains("bp-1"));
    }

    // ========================================================================
    // register_identity_provider tests
    // ========================================================================

    #[test]
    fn test_register_identity_provider_standalone() {
        let mut graph = create_test_graph();
        graph
            .register_identity_provider("ip-1", HashMap::new(), &[])
            .unwrap();

        assert!(graph.contains("ip-1"));
        assert_eq!(
            graph.get_component("ip-1").unwrap().kind,
            ComponentKind::IdentityProvider
        );
        assert_eq!(
            graph.get_component("ip-1").unwrap().status,
            ComponentStatus::Stopped
        );
        // 2 ownership edges (Owns + OwnedBy)
        assert_eq!(graph.edge_count(), 2);
    }

    #[test]
    fn test_register_identity_provider_with_components() {
        let mut graph = create_test_graph();
        graph.register_source("source-1", HashMap::new()).unwrap();
        graph
            .register_identity_provider("ip-1", HashMap::new(), &["source-1".to_string()])
            .unwrap();

        assert!(graph.contains("ip-1"));
        // 2 ownership edges for source + 2 ownership edges for ip + 2 Authenticates/AuthenticatedBy edges
        assert_eq!(graph.edge_count(), 6);
    }

    #[test]
    fn test_register_identity_provider_duplicate_fails() {
        let mut graph = create_test_graph();
        graph
            .register_identity_provider("ip-1", HashMap::new(), &[])
            .unwrap();
        let result = graph.register_identity_provider("ip-1", HashMap::new(), &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_register_identity_provider_missing_component_fails() {
        let mut graph = create_test_graph();
        let result = graph.register_identity_provider(
            "ip-1",
            HashMap::new(),
            &["nonexistent-source".to_string()],
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
        assert!(!graph.contains("ip-1"));
    }

    #[test]
    fn test_full_pipeline_with_providers() {
        let mut graph = create_test_graph();

        // Register providers first
        graph
            .register_bootstrap_provider("bp-1", HashMap::new(), &[])
            .unwrap();
        graph
            .register_identity_provider("ip-1", HashMap::new(), &[])
            .unwrap();

        // Register source with bootstrap and identity links
        graph.register_source("src", HashMap::new()).unwrap();
        graph
            .add_relationship("bp-1", "src", RelationshipKind::Bootstraps)
            .unwrap();
        graph
            .add_relationship("ip-1", "src", RelationshipKind::Authenticates)
            .unwrap();

        // Register query and reaction
        graph
            .register_query("qry", HashMap::new(), &["src".to_string()])
            .unwrap();
        graph
            .register_reaction("rxn", HashMap::new(), &["qry".to_string()])
            .unwrap();

        // Verify topology
        assert_eq!(graph.node_count(), 6); // instance + bp + ip + src + qry + rxn

        // List by kind
        assert_eq!(
            graph.list_by_kind(&ComponentKind::BootstrapProvider).len(),
            1
        );
        assert_eq!(
            graph.list_by_kind(&ComponentKind::IdentityProvider).len(),
            1
        );

        // Teardown in reverse order
        graph.deregister("rxn").unwrap();
        graph.deregister("qry").unwrap();
        graph.deregister("src").unwrap();
        graph.deregister("ip-1").unwrap();
        graph.deregister("bp-1").unwrap();

        assert_eq!(graph.node_count(), 1); // only instance remains
    }

    // ========================================================================
    // Runtime store tests
    // ========================================================================

    #[test]
    fn test_set_and_get_runtime() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();

        // Store an Arc<String> as a stand-in for Arc<dyn Source>
        let runtime: Arc<String> = Arc::new("test-runtime".to_string());
        graph
            .set_runtime("source-1", Box::new(runtime.clone()))
            .unwrap();

        // Retrieve it
        let retrieved = graph.get_runtime::<Arc<String>>("source-1");
        assert!(retrieved.is_some());
        assert_eq!(**retrieved.unwrap(), "test-runtime");
    }

    #[test]
    fn test_get_runtime_wrong_type_returns_none() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();

        let runtime: Arc<String> = Arc::new("test".to_string());
        graph.set_runtime("source-1", Box::new(runtime)).unwrap();

        // Wrong type returns None
        let retrieved = graph.get_runtime::<Arc<i32>>("source-1");
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_get_runtime_nonexistent_returns_none() {
        let graph = create_test_graph();
        let retrieved = graph.get_runtime::<Arc<String>>("nonexistent");
        assert!(retrieved.is_none());
    }

    #[test]
    fn test_take_runtime() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();

        let runtime: Arc<String> = Arc::new("take-me".to_string());
        graph.set_runtime("source-1", Box::new(runtime)).unwrap();

        // Take removes it
        let taken = graph.take_runtime::<Arc<String>>("source-1");
        assert!(taken.is_some());
        assert_eq!(*taken.unwrap(), "take-me");

        // Now it's gone
        assert!(!graph.has_runtime("source-1"));
        assert!(graph.get_runtime::<Arc<String>>("source-1").is_none());
    }

    #[test]
    fn test_has_runtime() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();

        assert!(!graph.has_runtime("source-1"));

        graph.set_runtime("source-1", Box::new(42i32)).unwrap();
        assert!(graph.has_runtime("source-1"));
    }

    #[test]
    fn test_remove_component_removes_runtime() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();

        graph
            .set_runtime("source-1", Box::new(Arc::new("runtime".to_string())))
            .unwrap();
        assert!(graph.has_runtime("source-1"));

        graph.remove_component("source-1").unwrap();
        assert!(!graph.has_runtime("source-1"));
    }

    #[test]
    fn test_deregister_removes_runtime() {
        let mut graph = create_test_graph();
        graph.register_source("source-1", HashMap::new()).unwrap();

        graph
            .set_runtime("source-1", Box::new(Arc::new("runtime".to_string())))
            .unwrap();
        assert!(graph.has_runtime("source-1"));

        graph.deregister("source-1").unwrap();
        assert!(!graph.has_runtime("source-1"));
    }

    #[test]
    fn test_set_runtime_replaces_existing() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();

        graph
            .set_runtime("source-1", Box::new(Arc::new("first".to_string())))
            .unwrap();
        graph
            .set_runtime("source-1", Box::new(Arc::new("second".to_string())))
            .unwrap();

        let retrieved = graph.get_runtime::<Arc<String>>("source-1");
        assert_eq!(**retrieved.unwrap(), "second");
    }

    // ========================================================================
    // ComponentStatusHandle tests
    // ========================================================================

    #[tokio::test]
    async fn test_status_handle_new_defaults_to_stopped() {
        let handle = ComponentStatusHandle::new("comp-1");
        assert_eq!(handle.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_status_handle_set_and_get() {
        let handle = ComponentStatusHandle::new("comp-1");
        handle.set_status(ComponentStatus::Running, None).await;
        assert_eq!(handle.get_status().await, ComponentStatus::Running);
    }

    #[tokio::test]
    async fn test_status_handle_new_wired_sends_update() {
        let (tx, mut rx) = mpsc::channel::<ComponentUpdate>(16);
        let handle = ComponentStatusHandle::new_wired("comp-1", tx);

        // Local status starts at Stopped
        assert_eq!(handle.get_status().await, ComponentStatus::Stopped);

        // set_status should update locally AND send via the channel
        handle
            .set_status(ComponentStatus::Running, Some("started".into()))
            .await;
        assert_eq!(handle.get_status().await, ComponentStatus::Running);

        let update = rx.try_recv().unwrap();
        match update {
            ComponentUpdate::Status {
                component_id,
                status,
                message,
            } => {
                assert_eq!(component_id, "comp-1");
                assert_eq!(status, ComponentStatus::Running);
                assert_eq!(message, Some("started".into()));
            }
        }
    }

    #[tokio::test]
    async fn test_status_handle_unwired_does_not_send() {
        let handle = ComponentStatusHandle::new("comp-1");
        // No channel wired — set_status should still work locally
        handle.set_status(ComponentStatus::Error, None).await;
        assert_eq!(handle.get_status().await, ComponentStatus::Error);
    }

    #[tokio::test]
    async fn test_status_handle_wire_after_creation() {
        let (tx, mut rx) = mpsc::channel::<ComponentUpdate>(16);
        let handle = ComponentStatusHandle::new("comp-1");

        // Wire later
        handle.wire(tx).await;

        handle.set_status(ComponentStatus::Starting, None).await;

        let update = rx.try_recv().unwrap();
        match update {
            ComponentUpdate::Status {
                component_id,
                status,
                ..
            } => {
                assert_eq!(component_id, "comp-1");
                assert_eq!(status, ComponentStatus::Starting);
            }
        }
    }

    #[tokio::test]
    async fn test_status_handle_wire_only_first_call_takes_effect() {
        let (tx1, mut rx1) = mpsc::channel::<ComponentUpdate>(16);
        let (tx2, mut rx2) = mpsc::channel::<ComponentUpdate>(16);
        let handle = ComponentStatusHandle::new("comp-1");

        handle.wire(tx1).await;
        // Second wire is ignored (OnceCell)
        handle.wire(tx2).await;

        handle.set_status(ComponentStatus::Running, None).await;

        // Should arrive on first channel only
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_status_handle_clone_shares_state() {
        let handle1 = ComponentStatusHandle::new("comp-1");
        let handle2 = handle1.clone();

        handle1.set_status(ComponentStatus::Running, None).await;
        assert_eq!(handle2.get_status().await, ComponentStatus::Running);
    }

    // ========================================================================
    // subscribe() tests
    // ========================================================================

    #[test]
    fn test_subscribe_receives_add_event() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        let mut event_rx = graph.subscribe();

        graph.add_component(source_node("source-1")).unwrap();

        let event = event_rx.try_recv().unwrap();
        assert_eq!(event.component_id, "source-1");
        assert_eq!(event.component_type, ComponentType::Source);
        assert_eq!(event.status, ComponentStatus::Stopped);
    }

    #[test]
    fn test_subscribe_receives_status_change_event() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        let mut event_rx = graph.subscribe();

        graph.add_component(source_node("source-1")).unwrap();
        let _add_event = event_rx.try_recv().unwrap(); // consume add event

        // Transition Stopped → Starting
        graph
            .validate_and_transition("source-1", ComponentStatus::Starting, None)
            .unwrap();

        let event = event_rx.try_recv().unwrap();
        assert_eq!(event.component_id, "source-1");
        assert_eq!(event.status, ComponentStatus::Starting);
    }

    #[test]
    fn test_subscribe_multiple_receivers() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        let mut rx1 = graph.subscribe();
        let mut rx2 = graph.subscribe();

        graph.add_component(source_node("source-1")).unwrap();

        assert_eq!(rx1.try_recv().unwrap().component_id, "source-1");
        assert_eq!(rx2.try_recv().unwrap().component_id, "source-1");
    }

    // ========================================================================
    // apply_update() tests
    // ========================================================================

    #[test]
    fn test_apply_update_changes_status() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();

        // Stopped → Starting is valid
        let event = graph.apply_update(ComponentUpdate::Status {
            component_id: "source-1".into(),
            status: ComponentStatus::Starting,
            message: Some("booting".into()),
        });

        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.component_id, "source-1");
        assert_eq!(event.status, ComponentStatus::Starting);
        assert_eq!(event.message, Some("booting".into()));

        // Verify status was updated in the graph
        let node = graph.get_component("source-1").unwrap();
        assert_eq!(node.status, ComponentStatus::Starting);
    }

    #[test]
    fn test_apply_update_emits_broadcast_event() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();
        let mut event_rx = graph.subscribe();
        let _add = event_rx.try_recv(); // consume add event

        graph.apply_update(ComponentUpdate::Status {
            component_id: "source-1".into(),
            status: ComponentStatus::Starting,
            message: None,
        });

        let event = event_rx.try_recv().unwrap();
        assert_eq!(event.component_id, "source-1");
        assert_eq!(event.status, ComponentStatus::Starting);
    }

    #[test]
    fn test_apply_update_nonexistent_component_returns_none() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");

        let event = graph.apply_update(ComponentUpdate::Status {
            component_id: "nonexistent".into(),
            status: ComponentStatus::Running,
            message: None,
        });

        assert!(event.is_none());
    }

    #[test]
    fn test_apply_update_invalid_transition_returns_none() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();

        // Stopped → Running is not a valid transition (must go through Starting)
        let event = graph.apply_update(ComponentUpdate::Status {
            component_id: "source-1".into(),
            status: ComponentStatus::Running,
            message: None,
        });

        assert!(event.is_none());
        // Status should remain Stopped
        assert_eq!(
            graph.get_component("source-1").unwrap().status,
            ComponentStatus::Stopped
        );
    }

    #[test]
    fn test_apply_update_same_status_is_noop() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();

        let event = graph.apply_update(ComponentUpdate::Status {
            component_id: "source-1".into(),
            status: ComponentStatus::Stopped,
            message: None,
        });

        assert!(event.is_none());
    }

    // ========================================================================
    // get_dependents() / get_dependencies() tests
    // ========================================================================

    #[test]
    fn test_get_dependents_returns_empty_for_leaf() {
        let mut graph = create_test_graph();
        graph.add_component(reaction_node("reaction-1")).unwrap();

        assert!(graph.get_dependents("reaction-1").is_empty());
    }

    #[test]
    fn test_get_dependents_returns_empty_for_nonexistent() {
        let graph = create_test_graph();
        assert!(graph.get_dependents("nonexistent").is_empty());
    }

    #[test]
    fn test_get_dependents_multiple() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        graph.add_component(query_node("query-2")).unwrap();

        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        graph
            .add_relationship("source-1", "query-2", RelationshipKind::Feeds)
            .unwrap();

        let dependents = graph.get_dependents("source-1");
        assert_eq!(dependents.len(), 2);
        let ids: Vec<&str> = dependents.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"query-1"));
        assert!(ids.contains(&"query-2"));
    }

    #[test]
    fn test_get_dependencies_returns_empty_for_root_component() {
        let mut graph = create_test_graph();
        graph.add_component(source_node("source-1")).unwrap();

        // Source has no SubscribesTo edges
        assert!(graph.get_dependencies("source-1").is_empty());
    }

    #[test]
    fn test_get_dependencies_returns_empty_for_nonexistent() {
        let graph = create_test_graph();
        assert!(graph.get_dependencies("nonexistent").is_empty());
    }

    #[test]
    fn test_get_dependencies_follows_subscribes_to() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();

        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();

        // query-1 subscribes to source-1
        let deps = graph.get_dependencies("query-1");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "source-1");
    }

    #[test]
    fn test_get_dependencies_multiple() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(source_node("source-2")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();

        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        graph
            .add_relationship("source-2", "query-1", RelationshipKind::Feeds)
            .unwrap();

        let deps = graph.get_dependencies("query-1");
        assert_eq!(deps.len(), 2);
        let ids: Vec<&str> = deps.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"source-1"));
        assert!(ids.contains(&"source-2"));
    }

    #[test]
    fn test_get_dependencies_chain() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();
        graph.add_component(query_node("query-1")).unwrap();
        graph.add_component(reaction_node("reaction-1")).unwrap();

        graph
            .add_relationship("source-1", "query-1", RelationshipKind::Feeds)
            .unwrap();
        graph
            .add_relationship("query-1", "reaction-1", RelationshipKind::Feeds)
            .unwrap();

        // reaction-1 depends on query-1 (not transitively on source-1)
        let deps = graph.get_dependencies("reaction-1");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "query-1");
    }

    // ========================================================================
    // wait_for_status() tests
    // ========================================================================

    #[tokio::test]
    async fn test_wait_for_status_already_reached() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        let graph = Arc::new(RwLock::new(graph));

        {
            let mut g = graph.write().await;
            g.add_component(source_node("source-1")).unwrap();
        }

        // source-1 starts as Stopped, so waiting for Stopped should return immediately
        let result = wait_for_status(
            &graph,
            "source-1",
            &[ComponentStatus::Stopped],
            std::time::Duration::from_millis(100),
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_wait_for_status_component_not_found() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        let graph = Arc::new(RwLock::new(graph));

        let result = wait_for_status(
            &graph,
            "nonexistent",
            &[ComponentStatus::Running],
            std::time::Duration::from_millis(100),
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_wait_for_status_timeout() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        let graph = Arc::new(RwLock::new(graph));

        {
            let mut g = graph.write().await;
            g.add_component(source_node("source-1")).unwrap();
        }

        // source-1 is Stopped, wait for Running with a short timeout
        let result = wait_for_status(
            &graph,
            "source-1",
            &[ComponentStatus::Running],
            std::time::Duration::from_millis(50),
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Timed out"));
    }

    #[tokio::test]
    async fn test_wait_for_status_reaches_target_via_update() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        let graph = Arc::new(RwLock::new(graph));

        {
            let mut g = graph.write().await;
            g.add_component(source_node("source-1")).unwrap();
        }

        let graph_clone = Arc::clone(&graph);
        // Spawn a task that transitions the component after a short delay
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let mut g = graph_clone.write().await;
            g.apply_update(ComponentUpdate::Status {
                component_id: "source-1".into(),
                status: ComponentStatus::Starting,
                message: None,
            });
        });

        let result = wait_for_status(
            &graph,
            "source-1",
            &[ComponentStatus::Starting],
            std::time::Duration::from_secs(2),
        )
        .await;

        handle.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ComponentStatus::Starting);
    }

    #[tokio::test]
    async fn test_wait_for_status_multiple_targets() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        let graph = Arc::new(RwLock::new(graph));

        {
            let mut g = graph.write().await;
            g.add_component(source_node("source-1")).unwrap();
        }

        // Stopped matches the second target
        let result = wait_for_status(
            &graph,
            "source-1",
            &[ComponentStatus::Running, ComponentStatus::Stopped],
            std::time::Duration::from_millis(100),
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ComponentStatus::Stopped);
    }

    // ========================================================================
    // GraphTransaction additional tests
    // ========================================================================

    #[test]
    fn test_transaction_rollback_cleans_edges_and_nodes() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        // Pre-add source outside transaction
        graph.add_component(source_node("source-1")).unwrap();
        let initial_edge_count = graph.edge_count(); // 2 (Owns + OwnedBy)

        {
            let mut txn = graph.begin();
            txn.add_component(query_node("query-1")).unwrap();
            txn.add_relationship("source-1", "query-1", RelationshipKind::Feeds)
                .unwrap();
            // Drop without commit
        }

        assert!(!graph.contains("query-1"));
        assert_eq!(graph.edge_count(), initial_edge_count);
        assert!(graph.get_dependents("source-1").is_empty());
    }

    #[test]
    fn test_transaction_commit_events_have_correct_status() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        let mut event_rx = graph.subscribe();

        {
            let mut txn = graph.begin();
            txn.add_component(source_node("source-1")).unwrap();
            txn.commit();
        }

        let event = event_rx.try_recv().unwrap();
        assert_eq!(event.status, ComponentStatus::Stopped);
        assert_eq!(event.component_type, ComponentType::Source);
    }

    #[test]
    fn test_transaction_no_events_before_commit() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        let mut event_rx = graph.subscribe();

        {
            let mut txn = graph.begin();
            txn.add_component(source_node("source-1")).unwrap();
            txn.add_component(query_node("query-1")).unwrap();
            // Not committed yet — no events emitted
            assert!(event_rx.try_recv().is_err());
            txn.commit();
        }

        // Now events should be available
        assert!(event_rx.try_recv().is_ok());
        assert!(event_rx.try_recv().is_ok());
    }

    #[test]
    fn test_transaction_add_relationship_between_existing_and_new() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();

        {
            let mut txn = graph.begin();
            txn.add_component(query_node("query-1")).unwrap();
            txn.add_relationship("source-1", "query-1", RelationshipKind::Feeds)
                .unwrap();
            txn.commit();
        }

        assert!(graph.contains("query-1"));
        assert_eq!(graph.get_dependents("source-1").len(), 1);
    }

    // ========================================================================
    // record_event() / get_events() / get_all_events() / get_last_error()
    // ========================================================================

    #[test]
    fn test_record_and_get_events() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");

        let event = ComponentEvent {
            component_id: "source-1".into(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: None,
        };
        graph.record_event(event.clone());

        let events = graph.get_events("source-1");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].component_id, "source-1");
        assert_eq!(events[0].status, ComponentStatus::Starting);
    }

    #[test]
    fn test_get_events_empty_for_unknown_component() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        assert!(graph.get_events("nonexistent").is_empty());
    }

    #[test]
    fn test_get_all_events_across_components() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");

        let event1 = ComponentEvent {
            component_id: "source-1".into(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: None,
        };
        let event2 = ComponentEvent {
            component_id: "query-1".into(),
            component_type: ComponentType::Query,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: None,
        };
        graph.record_event(event1);
        graph.record_event(event2);

        let all = graph.get_all_events();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_get_last_error_returns_none_when_no_errors() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        assert!(graph.get_last_error("source-1").is_none());
    }

    #[test]
    fn test_get_last_error_returns_error_message() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");

        let event = ComponentEvent {
            component_id: "source-1".into(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Error,
            timestamp: chrono::Utc::now(),
            message: Some("connection refused".into()),
        };
        graph.record_event(event);

        let error = graph.get_last_error("source-1");
        assert!(error.is_some());
        assert_eq!(error.unwrap(), "connection refused");
    }

    #[test]
    fn test_apply_update_records_event_in_history() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();

        graph.apply_update(ComponentUpdate::Status {
            component_id: "source-1".into(),
            status: ComponentStatus::Starting,
            message: Some("boot".into()),
        });

        let events = graph.get_events("source-1");
        // add_component also records an event, plus the apply_update
        assert!(!events.is_empty());
        let last = events.last().unwrap();
        assert_eq!(last.status, ComponentStatus::Starting);
        assert_eq!(last.message, Some("boot".into()));
    }

    // ========================================================================
    // subscribe_events() tests
    // ========================================================================

    #[test]
    fn test_subscribe_events_returns_history_and_receiver() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");

        // Record some history first
        let event = ComponentEvent {
            component_id: "source-1".into(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: None,
        };
        graph.record_event(event);

        let (history, _rx) = graph.subscribe_events("source-1");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, ComponentStatus::Starting);
    }

    #[test]
    fn test_subscribe_events_receives_new_events() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.add_component(source_node("source-1")).unwrap();

        let (_history, mut event_rx) = graph.subscribe_events("source-1");

        // Record a new event after subscribing
        let event = ComponentEvent {
            component_id: "source-1".into(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: None,
        };
        graph.record_event(event);

        let received = event_rx.try_recv().unwrap();
        assert_eq!(received.status, ComponentStatus::Running);
    }

    #[test]
    fn test_subscribe_events_empty_history_for_new_component() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");

        let (history, _rx) = graph.subscribe_events("brand-new");
        assert!(history.is_empty());
    }

    // ========================================================================
    // event_sender() / update_sender() / status_notifier() accessor tests
    // ========================================================================

    #[test]
    fn test_event_sender_broadcasts_to_subscribers() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        let sender = graph.event_sender().clone();
        let mut rx = graph.subscribe();

        let event = ComponentEvent {
            component_id: "test".into(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: None,
        };
        sender.send(event).unwrap();

        let received = rx.try_recv().unwrap();
        assert_eq!(received.component_id, "test");
    }

    #[tokio::test]
    async fn test_update_sender_delivers_to_receiver() {
        let (graph, mut update_rx) = ComponentGraph::new("test-instance");
        let update_tx = graph.update_sender();

        update_tx
            .send(ComponentUpdate::Status {
                component_id: "comp-1".into(),
                status: ComponentStatus::Running,
                message: None,
            })
            .await
            .unwrap();

        let update = update_rx.recv().await.unwrap();
        match update {
            ComponentUpdate::Status {
                component_id,
                status,
                ..
            } => {
                assert_eq!(component_id, "comp-1");
                assert_eq!(status, ComponentStatus::Running);
            }
        }
    }

    #[test]
    fn test_status_notifier_returns_arc() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        let n1 = graph.status_notifier();
        let n2 = graph.status_notifier();
        // Both should point to the same Notify
        assert!(Arc::ptr_eq(&n1, &n2));
    }
}
