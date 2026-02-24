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

use std::collections::HashMap;
use std::sync::Arc;

use petgraph::stable_graph::{NodeIndex, StableGraph};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::channels::{
    ComponentEvent, ComponentEventBroadcastReceiver, ComponentStatus, ComponentType,
};

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
    update_tx: Arc<RwLock<Option<ComponentUpdateSender>>>,
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
            update_tx: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a handle already connected to the graph update loop.
    ///
    /// Use this when the update channel is available at construction time
    /// (e.g., in `QueryManager` where queries are created with full context).
    pub fn new_wired(component_id: impl Into<String>, update_tx: ComponentUpdateSender) -> Self {
        Self {
            component_id: component_id.into(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            update_tx: Arc::new(RwLock::new(Some(update_tx))),
        }
    }

    /// Connect this handle to the graph update loop.
    ///
    /// After wiring, every [`set_status`](Self::set_status) call will also
    /// send a fire-and-forget notification to the graph.
    pub async fn wire(&self, update_tx: ComponentUpdateSender) {
        *self.update_tx.write().await = Some(update_tx);
    }

    /// Set the component's status — updates local state AND notifies the graph.
    ///
    /// This is the single canonical way to change a component's status. It writes
    /// to the local `Arc<RwLock<ComponentStatus>>` and sends a fire-and-forget
    /// update to the graph update loop (if wired).
    pub async fn set_status(&self, status: ComponentStatus, message: Option<String>) {
        *self.status.write().await = status.clone();
        if let Some(ref tx) = *self.update_tx.read().await {
            let _ = tx.try_send(ComponentUpdate::Status {
                component_id: self.component_id.clone(),
                status,
                message,
            });
        }
    }

    /// Read the current status.
    pub async fn get_status(&self) -> ComponentStatus {
        self.status.read().await.clone()
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
    /// Returns `None` for kinds that don't have a corresponding event type
    /// (Instance, BootstrapProvider, IdentityProvider).
    pub fn to_component_type(&self) -> Option<ComponentType> {
        match self {
            ComponentKind::Source => Some(ComponentType::Source),
            ComponentKind::Query => Some(ComponentType::Query),
            ComponentKind::Reaction => Some(ComponentType::Reaction),
            _ => None,
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
/// # Event Emission
///
/// The graph emits [`ComponentEvent`]s via a built-in broadcast channel whenever
/// components are added, removed, or change status. Call [`subscribe()`](Self::subscribe)
/// to receive events.
///
/// # Thread Safety
///
/// Wrap in `Arc<RwLock<ComponentGraph>>` for multi-threaded access.
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

    /// Apply a [`ComponentUpdate`] received from the mpsc channel.
    ///
    /// Called by the graph update loop task. Updates the graph, emits a
    /// broadcast event, and returns the event so the loop can record it
    /// in the appropriate manager's [`ComponentEventHistory`].
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
        if self.index.contains_key(&node.id) {
            return Err(anyhow::anyhow!(
                "{} '{}' already exists in the graph",
                node.kind,
                node.id
            ));
        }

        let id = node.id.clone();
        let kind = node.kind.clone();
        let status = node.status.clone();
        let node_idx = self.graph.add_node(node);
        self.index.insert(id.clone(), node_idx);

        // Create bidirectional ownership edges (Instance ↔ Component)
        self.graph
            .add_edge(self.instance_idx, node_idx, RelationshipKind::Owns);
        self.graph
            .add_edge(node_idx, self.instance_idx, RelationshipKind::OwnedBy);

        self.emit_event(&id, &kind, status, Some(format!("{kind} added")));

        Ok(node_idx)
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
            .map(|node| (node.id.clone(), node.status.clone()))
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
    /// Emits a [`ComponentEvent`] with the new status and message to all subscribers.
    /// Called by [`apply_update`] in the graph update loop — the single funnel for
    /// all status mutations.
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
        node.status = status.clone();

        Ok(self.emit_event(id, &kind, status, message))
    }

    // ========================================================================
    // Edge Operations
    // ========================================================================

    /// Add a bidirectional relationship between two components.
    ///
    /// Creates both the forward edge (from → to with `forward` relationship) and
    /// the reverse edge (to → from with the reverse of `forward`).
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
        self.graph.add_edge(from_idx, to_idx, forward);
        self.graph.add_edge(to_idx, from_idx, reverse);

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
}
