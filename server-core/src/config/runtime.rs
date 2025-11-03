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

use super::schema::{QueryConfig, ReactionConfig, SourceConfig};
use crate::channels::ComponentStatus;

/// Helper function to convert typed config to properties HashMap
fn serialize_to_properties<T: serde::Serialize>(config: &T) -> HashMap<String, serde_json::Value> {
    match serde_json::to_value(config) {
        Ok(serde_json::Value::Object(map)) => {
            map.into_iter()
                .filter(|(k, _)| k != "source_type" && k != "reaction_type")
                .collect()
        }
        _ => HashMap::new(),
    }
}

/// Runtime representation of a source with status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRuntime {
    /// Unique identifier for the source
    pub id: String,
    /// Type of source (e.g., "mock", "kafka", "database")
    pub source_type: String,
    /// Current status of the source
    pub status: ComponentStatus,
    /// Error message if status is Error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Source-specific configuration properties
    pub properties: HashMap<String, serde_json::Value>,
}

/// Runtime representation of a query with status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRuntime {
    /// Unique identifier for the query
    pub id: String,
    /// Cypher query string
    pub query: String,
    /// Current status of the query
    pub status: ComponentStatus,
    /// Error message if status is Error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// IDs of sources this query subscribes to
    pub sources: Vec<String>,
    /// Optional synthetic joins for the query
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joins: Option<Vec<super::schema::QueryJoinConfig>>,
}

/// Runtime representation of a reaction with status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionRuntime {
    /// Unique identifier for the reaction
    pub id: String,
    /// Type of reaction (e.g., "log", "webhook", "notification")
    pub reaction_type: String,
    /// Current status of the reaction
    pub status: ComponentStatus,
    /// Error message if status is Error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// IDs of queries this reaction subscribes to
    pub queries: Vec<String>,
    /// Reaction-specific configuration properties
    pub properties: HashMap<String, serde_json::Value>,
}

impl From<SourceConfig> for SourceRuntime {
    fn from(config: SourceConfig) -> Self {
        let id = config.id.clone();
        let source_type = config.source_type().to_string();
        let properties = serialize_to_properties(&config.config);
        Self {
            id,
            source_type,
            status: ComponentStatus::Stopped,
            error_message: None,
            properties,
        }
    }
}

impl From<QueryConfig> for QueryRuntime {
    fn from(config: QueryConfig) -> Self {
        Self {
            id: config.id,
            query: config.query,
            status: ComponentStatus::Stopped,
            error_message: None,
            sources: config.sources,
            joins: config.joins,
        }
    }
}

impl From<ReactionConfig> for ReactionRuntime {
    fn from(config: ReactionConfig) -> Self {
        let id = config.id.clone();
        let reaction_type = config.reaction_type.clone();
        let queries = config.queries.clone();
        let properties = serialize_to_properties(&config.config);
        Self {
            id,
            reaction_type,
            status: ComponentStatus::Stopped,
            error_message: None,
            queries,
            properties,
        }
    }
}

/// Runtime configuration for the Drasi server
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub server_core: super::schema::DrasiServerCoreSettings,
    pub sources: Vec<SourceConfig>,
    pub queries: Vec<QueryConfig>,
    pub reactions: Vec<ReactionConfig>,
}

impl From<super::schema::DrasiServerCoreConfig> for RuntimeConfig {
    fn from(config: super::schema::DrasiServerCoreConfig) -> Self {
        // Get the global defaults (or hardcoded fallbacks)
        let global_priority_queue = config.server_core.priority_queue_capacity.unwrap_or(10000);
        let global_dispatch_capacity = config
            .server_core
            .dispatch_buffer_capacity
            .unwrap_or(1000);

        // Apply global defaults to sources that don't specify their own dispatch capacity
        let sources = config
            .sources
            .into_iter()
            .map(|mut s| {
                if s.dispatch_buffer_capacity.is_none() {
                    s.dispatch_buffer_capacity = Some(global_dispatch_capacity);
                }
                s
            })
            .collect();

        // Apply global defaults to queries
        let queries = config
            .queries
            .into_iter()
            .map(|mut q| {
                if q.priority_queue_capacity.is_none() {
                    q.priority_queue_capacity = Some(global_priority_queue);
                }
                if q.dispatch_buffer_capacity.is_none() {
                    q.dispatch_buffer_capacity = Some(global_dispatch_capacity);
                }
                q
            })
            .collect();

        // Apply global default to reactions that don't specify their own capacity
        let reactions = config
            .reactions
            .into_iter()
            .map(|mut r| {
                if r.priority_queue_capacity.is_none() {
                    r.priority_queue_capacity = Some(global_priority_queue);
                }
                r
            })
            .collect();

        Self {
            server_core: config.server_core,
            sources,
            queries,
            reactions,
        }
    }
}
