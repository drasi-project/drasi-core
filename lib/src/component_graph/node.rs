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

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};

use crate::channels::{ComponentStatus, ComponentType};

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
/// Uses `send().await` which applies backpressure if the channel is full,
/// ensuring status transitions are never silently dropped.
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
