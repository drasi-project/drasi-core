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

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::bootstrap::BootstrapProviderConfig;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum QueryLanguage {
    Cypher,
    GQL,
}

impl Default for QueryLanguage {
    fn default() -> Self {
        QueryLanguage::Cypher
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DrasiServerCoreConfig {
    #[serde(default)]
    pub server_core: DrasiServerCoreSettings,
    pub sources: Vec<SourceConfig>,
    pub queries: Vec<QueryConfig>,
    pub reactions: Vec<ReactionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrasiServerCoreSettings {
    #[serde(default = "default_id")]
    pub id: String,
    /// Default priority queue capacity for queries and reactions (default: 10000)
    #[serde(default = "default_priority_queue_capacity")]
    pub priority_queue_capacity: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    /// Unique identifier for the source
    pub id: String,
    /// Type of source (e.g., "mock", "kafka", "database")
    pub source_type: String,
    /// Whether to automatically start this source (default: true)
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    /// Source-specific configuration properties
    #[serde(default, deserialize_with = "deserialize_null_as_empty_map")]
    pub properties: HashMap<String, serde_json::Value>,
    /// Optional bootstrap provider configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap_provider: Option<BootstrapProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryConfig {
    /// Unique identifier for the query
    pub id: String,
    /// Query string (Cypher or GQL depending on query_language)
    pub query: String,
    /// Query language to use (default: Cypher)
    #[serde(default, rename = "queryLanguage")]
    pub query_language: QueryLanguage,
    /// IDs of sources this query subscribes to
    pub sources: Vec<String>,
    /// Whether to automatically start this query (default: true)
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    /// Query-specific configuration properties
    #[serde(default, deserialize_with = "deserialize_null_as_empty_map")]
    pub properties: HashMap<String, serde_json::Value>,
    /// Optional synthetic joins for the query
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joins: Option<Vec<QueryJoinConfig>>,
    /// Whether to enable bootstrap (default: true)
    #[serde(default = "default_enable_bootstrap", rename = "enableBootstrap")]
    pub enable_bootstrap: bool,
    /// Maximum number of events to buffer during bootstrap (default: 10000)
    #[serde(
        default = "default_bootstrap_buffer_size",
        rename = "bootstrapBufferSize"
    )]
    pub bootstrap_buffer_size: usize,
    /// Priority queue capacity for this query (default: 10000)
    #[serde(default = "default_priority_queue_capacity")]
    pub priority_queue_capacity: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryJoinConfig {
    /// Unique identifier for the join (should match relationship type in query)
    pub id: String,
    /// Keys defining the join relationship
    pub keys: Vec<QueryJoinKeyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryJoinKeyConfig {
    /// Node label to match
    pub label: String,
    /// Property to use for joining
    pub property: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionConfig {
    /// Unique identifier for the reaction
    pub id: String,
    /// Type of reaction (e.g., "log", "webhook", "notification")
    pub reaction_type: String,
    /// IDs of queries this reaction subscribes to
    pub queries: Vec<String>,
    /// Whether to automatically start this reaction (default: true)
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    /// Reaction-specific configuration properties
    #[serde(default, deserialize_with = "deserialize_null_as_empty_map")]
    pub properties: HashMap<String, serde_json::Value>,
    /// Priority queue capacity for this reaction (default: 10000)
    #[serde(default = "default_priority_queue_capacity")]
    pub priority_queue_capacity: usize,
}

impl DrasiServerCoreConfig {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let content = fs::read_to_string(path_ref).map_err(|e| {
            anyhow::anyhow!("Failed to read config file {}: {}", path_ref.display(), e)
        })?;

        // Try YAML first, then JSON
        match serde_yaml::from_str::<DrasiServerCoreConfig>(&content) {
            Ok(config) => Ok(config),
            Err(yaml_err) => {
                // If YAML fails, try JSON
                match serde_json::from_str::<DrasiServerCoreConfig>(&content) {
                    Ok(config) => Ok(config),
                    Err(json_err) => {
                        // Both failed, return detailed error
                        Err(anyhow::anyhow!(
                            "Failed to parse config file '{}':\n  YAML error: {}\n  JSON error: {}",
                            path_ref.display(),
                            yaml_err,
                            json_err
                        ))
                    }
                }
            }
        }
    }

    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let content = serde_yaml::to_string(self)?;
        fs::write(path, content)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        // Validate unique source ids
        let mut source_ids = std::collections::HashSet::new();
        for source in &self.sources {
            if !source_ids.insert(&source.id) {
                return Err(anyhow::anyhow!("Duplicate source id: '{}'", source.id));
            }
        }

        // Validate unique query ids
        let mut query_ids = std::collections::HashSet::new();
        for query in &self.queries {
            if !query_ids.insert(&query.id) {
                return Err(anyhow::anyhow!("Duplicate query id: '{}'", query.id));
            }
        }

        // Validate unique reaction ids
        let mut reaction_ids = std::collections::HashSet::new();
        for reaction in &self.reactions {
            if !reaction_ids.insert(&reaction.id) {
                return Err(anyhow::anyhow!("Duplicate reaction id: '{}'", reaction.id));
            }
        }

        // Validate source references in queries
        for query in &self.queries {
            for source_id in &query.sources {
                if !source_ids.contains(source_id) {
                    return Err(anyhow::anyhow!(
                        "Query '{}' references unknown source: '{}'",
                        query.id,
                        source_id
                    ));
                }
            }
        }

        // Validate query references in reactions
        for reaction in &self.reactions {
            for query_id in &reaction.queries {
                if !query_ids.contains(query_id) {
                    return Err(anyhow::anyhow!(
                        "Reaction '{}' references unknown query: '{}'",
                        reaction.id,
                        query_id
                    ));
                }
            }
        }

        Ok(())
    }
}

impl Default for DrasiServerCoreSettings {
    fn default() -> Self {
        Self {
            // Default server ID to a random UUID
            id: uuid::Uuid::new_v4().to_string(),
            priority_queue_capacity: default_priority_queue_capacity(),
        }
    }
}

fn default_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn default_auto_start() -> bool {
    true
}

fn default_enable_bootstrap() -> bool {
    true
}

fn default_bootstrap_buffer_size() -> usize {
    10000
}

fn default_priority_queue_capacity() -> usize {
    10000
}

/// Helper to deserialize null as empty HashMap
fn deserialize_null_as_empty_map<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, serde_json::Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<HashMap<String, serde_json::Value>>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

// Conversion implementations for QueryJoin types
impl From<QueryJoinKeyConfig> for drasi_core::models::QueryJoinKey {
    fn from(config: QueryJoinKeyConfig) -> Self {
        drasi_core::models::QueryJoinKey {
            label: config.label,
            property: config.property,
        }
    }
}

impl From<QueryJoinConfig> for drasi_core::models::QueryJoin {
    fn from(config: QueryJoinConfig) -> Self {
        drasi_core::models::QueryJoin {
            id: config.id,
            keys: config.keys.into_iter().map(|k| k.into()).collect(),
        }
    }
}
