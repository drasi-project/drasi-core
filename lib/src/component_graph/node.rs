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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::channels::{ComponentStatus, ComponentType};

// Preserve existing plugin import paths without owning their status protocol.
pub use crate::channels::{
    ComponentStatusHandle, ComponentUpdate, ComponentUpdateReceiver, ComponentUpdateSender,
};

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
/// This is an inspection DTO only. Runtime instances and their statuses belong
/// exclusively to the ComputationGraph registry publication it was derived from.
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
