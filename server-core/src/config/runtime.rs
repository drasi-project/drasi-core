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
    /// Query-specific configuration properties
    pub properties: HashMap<String, serde_json::Value>,
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
        Self {
            id: config.id,
            source_type: config.source_type,
            status: ComponentStatus::Stopped,
            error_message: None,
            properties: config.properties,
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
            properties: config.properties,
            joins: config.joins,
        }
    }
}

impl From<ReactionConfig> for ReactionRuntime {
    fn from(config: ReactionConfig) -> Self {
        Self {
            id: config.id,
            reaction_type: config.reaction_type,
            status: ComponentStatus::Stopped,
            error_message: None,
            queries: config.queries,
            properties: config.properties,
        }
    }
}

/// Runtime configuration for the Drasi server
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub server: super::schema::DrasiServerCoreSettings,
    pub sources: Vec<SourceConfig>,
    pub queries: Vec<QueryConfig>,
    pub reactions: Vec<ReactionConfig>,
}

impl From<super::schema::DrasiServerCoreConfig> for RuntimeConfig {
    fn from(config: super::schema::DrasiServerCoreConfig) -> Self {
        Self {
            server: config.server,
            sources: config.sources,
            queries: config.queries,
            reactions: config.reactions,
        }
    }
}
