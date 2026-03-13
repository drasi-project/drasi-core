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

//! Script file bootstrap provider for reading JSONL bootstrap data

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use log::{debug, error, info};
use std::path::PathBuf;
use std::sync::Arc;

use crate::script_reader::BootstrapScriptReader;
use crate::script_types::{BootstrapScriptRecord, NodeRecord, RelationRecord};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::sources::manager::convert_json_to_element_properties;

use drasi_lib::bootstrap::ScriptFileBootstrapConfig;

/// Bootstrap provider that reads data from JSONL script files
#[derive(Default)]
pub struct ScriptFileBootstrapProvider {
    file_paths: Vec<String>,
}

impl ScriptFileBootstrapProvider {
    /// Create a new script file bootstrap provider from configuration
    ///
    /// # Arguments
    /// * `config` - ScriptFile bootstrap configuration
    pub fn new(config: ScriptFileBootstrapConfig) -> Self {
        Self {
            file_paths: config.file_paths,
        }
    }

    /// Create a new script file bootstrap provider with explicit file paths
    ///
    /// # Arguments
    /// * `file_paths` - List of JSONL file paths to read in order
    pub fn with_paths(file_paths: Vec<String>) -> Self {
        Self { file_paths }
    }

    /// Create a builder for ScriptFileBootstrapProvider
    pub fn builder() -> ScriptFileBootstrapProviderBuilder {
        ScriptFileBootstrapProviderBuilder::new()
    }
}

/// Builder for ScriptFileBootstrapProvider
///
/// # Example
///
/// ```no_run
/// use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;
///
/// let provider = ScriptFileBootstrapProvider::builder()
///     .with_file("/path/to/data.jsonl")
///     .with_file("/path/to/more_data.jsonl")
///     .build();
/// ```
pub struct ScriptFileBootstrapProviderBuilder {
    file_paths: Vec<String>,
}

impl ScriptFileBootstrapProviderBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self {
            file_paths: Vec::new(),
        }
    }

    /// Set all file paths at once
    pub fn with_file_paths(mut self, paths: Vec<String>) -> Self {
        self.file_paths = paths;
        self
    }

    /// Add a single file path
    pub fn with_file(mut self, path: impl Into<String>) -> Self {
        self.file_paths.push(path.into());
        self
    }

    /// Build the ScriptFileBootstrapProvider
    pub fn build(self) -> ScriptFileBootstrapProvider {
        ScriptFileBootstrapProvider::with_paths(self.file_paths)
    }
}

impl Default for ScriptFileBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptFileBootstrapProvider {
    /// Convert a NodeRecord to an Element::Node
    fn convert_node_to_element(source_id: &str, node: &NodeRecord) -> Result<Element> {
        // Convert properties from JSON to ElementPropertyMap
        let properties = if let serde_json::Value::Object(obj) = &node.properties {
            convert_json_to_element_properties(obj)?
        } else if node.properties.is_null() {
            Default::default()
        } else {
            return Err(anyhow!(
                "ScriptFile bootstrap error: Node '{}' has invalid properties type. \
                 Properties must be a JSON object or null, found: {}",
                node.id,
                node.properties
            ));
        };

        // Convert labels to Arc slice
        let labels: Arc<[Arc<str>]> = node
            .labels
            .iter()
            .map(|l| Arc::from(l.as_str()))
            .collect::<Vec<_>>()
            .into();

        Ok(Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, &node.id),
                labels,
                effective_from: 0,
            },
            properties,
        })
    }

    /// Convert a RelationRecord to an Element::Relation
    fn convert_relation_to_element(source_id: &str, relation: &RelationRecord) -> Result<Element> {
        // Convert properties from JSON to ElementPropertyMap
        let properties = if let serde_json::Value::Object(obj) = &relation.properties {
            convert_json_to_element_properties(obj)?
        } else if relation.properties.is_null() {
            Default::default()
        } else {
            return Err(anyhow!(
                "ScriptFile bootstrap error: Relation '{}' has invalid properties type. \
                 Properties must be a JSON object or null, found: {}",
                relation.id,
                relation.properties
            ));
        };

        // Convert labels to Arc slice
        let labels: Arc<[Arc<str>]> = relation
            .labels
            .iter()
            .map(|l| Arc::from(l.as_str()))
            .collect::<Vec<_>>()
            .into();

        // Create start and end element references
        let start_ref = ElementReference::new(source_id, &relation.start_id);
        let end_ref = ElementReference::new(source_id, &relation.end_id);

        Ok(Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, &relation.id),
                labels,
                effective_from: 0,
            },
            properties,
            in_node: start_ref,
            out_node: end_ref,
        })
    }

    /// Check if a record matches the requested labels
    fn matches_labels(
        record_labels: &[String],
        requested_labels: &[String],
        _is_node: bool,
    ) -> bool {
        // If no labels requested, include all records
        if requested_labels.is_empty() {
            return true;
        }

        // Check if any of the record's labels match the requested labels
        record_labels
            .iter()
            .any(|label| requested_labels.contains(label))
    }

    /// Process records from the script and send matching elements
    async fn process_records(
        &self,
        reader: &mut BootstrapScriptReader,
        request: &BootstrapRequest,
        context: &BootstrapContext,
        event_tx: drasi_lib::channels::BootstrapEventSender,
    ) -> Result<usize> {
        let mut count = 0;

        for record_result in reader {
            let seq_record = record_result?;

            match seq_record.record {
                BootstrapScriptRecord::Node(node) => {
                    // Check if node matches requested labels
                    if Self::matches_labels(&node.labels, &request.node_labels, true) {
                        debug!("Processing node: id={}, labels={:?}", node.id, node.labels);

                        // Convert to element
                        let element = Self::convert_node_to_element(&context.source_id, &node)?;

                        // Send as insert
                        let source_change = SourceChange::Insert { element };

                        // Get next sequence number for this bootstrap event
                        let sequence = context.next_sequence();

                        let bootstrap_event = drasi_lib::channels::BootstrapEvent {
                            source_id: context.source_id.clone(),
                            change: source_change,
                            timestamp: chrono::Utc::now(),
                            sequence,
                        };

                        event_tx
                            .send(bootstrap_event)
                            .await
                            .map_err(|e| anyhow!("Failed to send node: {e}"))?;

                        count += 1;
                    }
                }
                BootstrapScriptRecord::Relation(relation) => {
                    // Check if relation matches requested labels
                    if Self::matches_labels(&relation.labels, &request.relation_labels, false) {
                        debug!(
                            "Processing relation: id={}, labels={:?}, start={}, end={}",
                            relation.id, relation.labels, relation.start_id, relation.end_id
                        );

                        // Convert to element
                        let element =
                            Self::convert_relation_to_element(&context.source_id, &relation)?;

                        // Send as insert
                        let source_change = SourceChange::Insert { element };

                        // Get next sequence number for this bootstrap event
                        let sequence = context.next_sequence();

                        let bootstrap_event = drasi_lib::channels::BootstrapEvent {
                            source_id: context.source_id.clone(),
                            change: source_change,
                            timestamp: chrono::Utc::now(),
                            sequence,
                        };

                        event_tx
                            .send(bootstrap_event)
                            .await
                            .map_err(|e| anyhow!("Failed to send relation: {e}"))?;

                        count += 1;
                    }
                }
                BootstrapScriptRecord::Finish(finish) => {
                    debug!("Reached finish record: {}", finish.description);
                    break;
                }
                BootstrapScriptRecord::Label(label) => {
                    debug!("Skipping label record: {}", label.label);
                }
                BootstrapScriptRecord::Comment(_) => {
                    // Comments are filtered by the reader, but handle just in case
                    debug!("Skipping comment record");
                }
                BootstrapScriptRecord::Header(_) => {
                    // Header already processed by reader
                    debug!("Skipping header record in iteration");
                }
            }
        }

        Ok(count)
    }
}

#[async_trait]
impl BootstrapProvider for ScriptFileBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: drasi_lib::channels::BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        info!(
            "Starting script file bootstrap for query {} from {} file(s)",
            request.query_id,
            self.file_paths.len()
        );

        // Convert file paths to PathBuf
        let paths: Vec<PathBuf> = self.file_paths.iter().map(PathBuf::from).collect();

        // Create the script reader
        let mut reader = BootstrapScriptReader::new(paths).map_err(|e| {
            error!("Failed to create script reader: {e}");
            anyhow!("Failed to create script reader: {e}")
        })?;

        // Get and log header information
        let header = reader.get_header();
        info!(
            "Script header - start_time: {}, description: {}",
            header.start_time, header.description
        );

        // Process records and send matching elements
        let count = self
            .process_records(&mut reader, &request, context, event_tx)
            .await?;

        info!(
            "Completed script file bootstrap for query {}: sent {} elements (requested node labels: {:?}, relation labels: {:?})",
            request.query_id, count, request.node_labels, request.relation_labels
        );

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script_types::{NodeRecord, RelationRecord};
    use serde_json::json;

    #[test]
    fn test_convert_node_to_element() {
        let node = NodeRecord {
            id: "n1".to_string(),
            labels: vec!["Person".to_string()],
            properties: json!({"name": "Alice", "age": 30}),
        };

        let element =
            ScriptFileBootstrapProvider::convert_node_to_element("test_source", &node).unwrap();

        match element {
            Element::Node {
                metadata,
                properties,
            } => {
                assert_eq!(metadata.reference.source_id.as_ref(), "test_source");
                assert_eq!(metadata.reference.element_id.as_ref(), "n1");
                assert_eq!(metadata.labels.len(), 1);
                assert_eq!(metadata.labels[0].as_ref(), "Person");
                assert!(properties.get(&Arc::from("name")).is_some());
            }
            _ => panic!("Expected Node element"),
        }
    }

    #[test]
    fn test_convert_node_with_null_properties() {
        let node = NodeRecord {
            id: "n1".to_string(),
            labels: vec!["Person".to_string()],
            properties: serde_json::Value::Null,
        };

        let element =
            ScriptFileBootstrapProvider::convert_node_to_element("test_source", &node).unwrap();

        match element {
            Element::Node { properties, .. } => {
                // Properties should be empty for null input
                assert!(properties.get(&Arc::from("test")).is_none());
            }
            _ => panic!("Expected Node element"),
        }
    }

    #[test]
    fn test_convert_relation_to_element() {
        let relation = RelationRecord {
            id: "r1".to_string(),
            labels: vec!["KNOWS".to_string()],
            start_id: "n1".to_string(),
            start_label: Some("Person".to_string()),
            end_id: "n2".to_string(),
            end_label: Some("Person".to_string()),
            properties: json!({"since": 2020}),
        };

        let element =
            ScriptFileBootstrapProvider::convert_relation_to_element("test_source", &relation)
                .unwrap();

        match element {
            Element::Relation {
                metadata,
                out_node,
                in_node,
                properties,
            } => {
                assert_eq!(metadata.reference.source_id.as_ref(), "test_source");
                assert_eq!(metadata.reference.element_id.as_ref(), "r1");
                assert_eq!(metadata.labels.len(), 1);
                assert_eq!(metadata.labels[0].as_ref(), "KNOWS");
                assert_eq!(in_node.source_id.as_ref(), "test_source");
                assert_eq!(in_node.element_id.as_ref(), "n1");
                assert_eq!(out_node.source_id.as_ref(), "test_source");
                assert_eq!(out_node.element_id.as_ref(), "n2");
                assert!(properties.get(&Arc::from("since")).is_some());
            }
            _ => panic!("Expected Relation element"),
        }
    }

    #[test]
    fn test_matches_labels_empty_request() {
        let record_labels = vec!["Person".to_string()];
        let requested_labels = vec![];

        assert!(ScriptFileBootstrapProvider::matches_labels(
            &record_labels,
            &requested_labels,
            true
        ));
    }

    #[test]
    fn test_matches_labels_match() {
        let record_labels = vec!["Person".to_string(), "Employee".to_string()];
        let requested_labels = vec!["Person".to_string()];

        assert!(ScriptFileBootstrapProvider::matches_labels(
            &record_labels,
            &requested_labels,
            true
        ));
    }

    #[test]
    fn test_matches_labels_no_match() {
        let record_labels = vec!["Person".to_string()];
        let requested_labels = vec!["Company".to_string()];

        assert!(!ScriptFileBootstrapProvider::matches_labels(
            &record_labels,
            &requested_labels,
            true
        ));
    }

    // Note: Full integration tests require tokio runtime and channels
    // These are handled in the main test suite
}
