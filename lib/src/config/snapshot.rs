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

//! Configuration snapshot types for capturing point-in-time instance state.
//!
//! A [`ConfigurationSnapshot`] captures the full topology, status, and configuration
//! properties of all components in a drasi-lib instance. Hosts can serialize this
//! snapshot to store it, and later use it to reconstruct an equivalent instance.
//!
//! **Important:** Sources and reactions are trait objects — their properties are
//! captured but they cannot be automatically deserialized back into instances.
//! The host must supply the appropriate plugin factories to reconstruct them.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::channels::ComponentStatus;
use crate::component_graph::GraphEdge;
use crate::config::schema::QueryConfig;

/// A point-in-time snapshot of the full drasi-lib instance configuration.
///
/// Contains the topology (components and dependency edges), lifecycle status,
/// and configuration properties of every source, query, and reaction.
///
/// # Serialization
///
/// This type implements `Serialize` and `Deserialize`, so it can be stored
/// as JSON, YAML, or any serde-compatible format.
///
/// # Example
///
/// ```no_run
/// # use drasi_lib::DrasiLib;
/// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
/// let snapshot = core.snapshot_configuration().await?;
/// let json = serde_json::to_string_pretty(&snapshot)?;
/// std::fs::write("config-snapshot.json", &json)?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigurationSnapshot {
    /// Unique identifier of the drasi-lib instance
    pub instance_id: String,
    /// ISO 8601 timestamp when the snapshot was captured
    pub timestamp: String,
    /// All source components with their configuration properties
    pub sources: Vec<SourceSnapshot>,
    /// All query components with their full query configurations
    pub queries: Vec<QuerySnapshot>,
    /// All reaction components with their configuration properties
    pub reactions: Vec<ReactionSnapshot>,
    /// Dependency edges between components (source→query, query→reaction)
    pub edges: Vec<GraphEdge>,
}

/// Snapshot of a source component's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSnapshot {
    /// Source component identifier
    pub id: String,
    /// Plugin type identifier (e.g., "postgres", "http", "grpc")
    pub source_type: String,
    /// Lifecycle status at the time of snapshot
    pub status: ComponentStatus,
    /// Whether the source was configured to auto-start
    pub auto_start: bool,
    /// Configuration properties reported by the source plugin
    pub properties: HashMap<String, serde_json::Value>,
    /// Bootstrap provider configuration, if one is attached to this source
    pub bootstrap_provider: Option<BootstrapSnapshot>,
}

/// Snapshot of a bootstrap provider's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapSnapshot {
    /// Bootstrap provider kind (e.g., "postgres", "scriptfile", "noop")
    pub kind: String,
    /// Configuration properties for the bootstrap provider
    pub properties: HashMap<String, serde_json::Value>,
}

/// Snapshot of a query component's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuerySnapshot {
    /// Query component identifier
    pub id: String,
    /// Full query configuration (query string, source subscriptions, joins, etc.)
    pub config: QueryConfig,
    /// Lifecycle status at the time of snapshot
    pub status: ComponentStatus,
}

/// Snapshot of a reaction component's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionSnapshot {
    /// Reaction component identifier
    pub id: String,
    /// Plugin type identifier (e.g., "log", "http", "grpc")
    pub reaction_type: String,
    /// Lifecycle status at the time of snapshot
    pub status: ComponentStatus,
    /// Whether the reaction was configured to auto-start
    pub auto_start: bool,
    /// Query IDs this reaction subscribes to
    pub queries: Vec<String>,
    /// Configuration properties reported by the reaction plugin
    pub properties: HashMap<String, serde_json::Value>,
}
