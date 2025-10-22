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

//! Platform bootstrap provider for remote Drasi sources
//!
//! This provider bootstraps data from a Query API service running in a remote
//! Drasi environment. It makes HTTP requests to the Query API service and
//! processes streaming JSON-NL responses.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;
use log::{debug, info, warn};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Map;
use std::sync::Arc;
use std::time::Duration;

use crate::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use crate::sources::manager::convert_json_to_element_properties;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};

/// Request format for Query API subscription
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionRequest {
    query_id: String,
    query_node_id: String,
    node_labels: Vec<String>,
    rel_labels: Vec<String>,
}

/// Bootstrap element format from Query API service (matches platform SDK)
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapElement {
    id: String,
    labels: Vec<String>,
    properties: Map<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_id: Option<String>,
}

/// Platform bootstrap provider that connects to Query API service
pub struct PlatformBootstrapProvider {
    query_api_url: String,
    _timeout: Duration,
    client: Client,
}

impl PlatformBootstrapProvider {
    /// Create a new platform bootstrap provider
    ///
    /// # Arguments
    /// * `query_api_url` - Base URL of the Query API service (e.g., "http://my-source-query-api:8080")
    /// * `timeout_seconds` - Timeout for HTTP requests in seconds
    ///
    /// # Returns
    /// Returns a new instance of PlatformBootstrapProvider or an error if the URL is invalid
    pub fn new(query_api_url: String, timeout_seconds: u64) -> Result<Self> {
        // Validate URL format
        reqwest::Url::parse(&query_api_url)
            .context(format!("Invalid query_api_url: {}", query_api_url))?;

        let timeout = Duration::from_secs(timeout_seconds);
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            query_api_url,
            _timeout: timeout,
            client,
        })
    }

    /// Make HTTP subscription request to Query API service
    ///
    /// Constructs a subscription request and sends it to the Query API service's
    /// subscription endpoint.
    async fn make_subscription_request(
        &self,
        request: &BootstrapRequest,
        context: &BootstrapContext,
    ) -> Result<reqwest::Response> {
        let subscription_req = SubscriptionRequest {
            query_id: request.query_id.clone(),
            query_node_id: context.server_id.clone(),
            node_labels: request.node_labels.clone(),
            rel_labels: request.relation_labels.clone(),
        };

        let url = format!("{}/subscription", self.query_api_url);
        debug!(
            "Making bootstrap subscription request to {} for query {}",
            url, request.query_id
        );

        let response = self
            .client
            .post(&url)
            .json(&subscription_req)
            .send()
            .await
            .context(format!("Failed to connect to Query API at {}", url))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read error response".to_string());
            return Err(anyhow::anyhow!(
                "Query API returned error status {}: {}",
                status,
                error_text
            ));
        }

        debug!(
            "Successfully connected to Query API, preparing to stream bootstrap data for query {}",
            request.query_id
        );
        Ok(response)
    }

    /// Process streaming JSON-NL response from Query API
    ///
    /// Reads the response body as a stream of bytes, splits by newlines,
    /// and parses each line as a BootstrapElement.
    async fn process_bootstrap_stream(
        &self,
        response: reqwest::Response,
    ) -> Result<Vec<BootstrapElement>> {
        let mut elements = Vec::new();
        let mut line_buffer = String::new();
        let mut byte_stream = response.bytes_stream();
        let mut element_count = 0;

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = chunk_result.context("Error reading stream chunk")?;
            let chunk_str =
                std::str::from_utf8(&chunk).context("Invalid UTF-8 in stream response")?;

            // Add chunk to buffer
            line_buffer.push_str(chunk_str);

            // Process complete lines
            while let Some(newline_pos) = line_buffer.find('\n') {
                let line = line_buffer[..newline_pos].trim().to_string();
                line_buffer = line_buffer[newline_pos + 1..].to_string();

                // Skip empty lines
                if line.is_empty() {
                    continue;
                }

                // Parse JSON line
                match serde_json::from_str::<BootstrapElement>(&line) {
                    Ok(element) => {
                        elements.push(element);
                        element_count += 1;
                        if element_count % 1000 == 0 {
                            debug!("Received {} bootstrap elements from stream", element_count);
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Failed to parse bootstrap element from JSON: {} - Line: {}",
                            e, line
                        );
                        // Continue processing other elements
                    }
                }
            }
        }

        // Process any remaining data in buffer
        let remaining = line_buffer.trim();
        if !remaining.is_empty() {
            match serde_json::from_str::<BootstrapElement>(remaining) {
                Ok(element) => {
                    elements.push(element);
                    element_count += 1;
                }
                Err(e) => {
                    warn!(
                        "Failed to parse final bootstrap element from JSON: {} - Line: {}",
                        e, remaining
                    );
                }
            }
        }

        info!(
            "Received total of {} bootstrap elements from Query API stream",
            element_count
        );
        Ok(elements)
    }
}

#[async_trait]
impl BootstrapProvider for PlatformBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: crate::channels::BootstrapEventSender,
    ) -> Result<usize> {
        info!(
            "Starting platform bootstrap for query {} from source {}",
            request.query_id, context.source_id
        );

        // Make HTTP request to Query API service
        let response = self
            .make_subscription_request(&request, context)
            .await
            .context("Failed to make subscription request to Query API")?;

        // Process streaming response
        let bootstrap_elements = self
            .process_bootstrap_stream(response)
            .await
            .context("Failed to process bootstrap stream from Query API")?;

        debug!(
            "Processing {} bootstrap elements for query {}",
            bootstrap_elements.len(),
            request.query_id
        );

        let mut sent_count = 0;
        let mut filtered_nodes = 0;
        let mut filtered_relations = 0;

        for bootstrap_elem in bootstrap_elements {
            // Determine if this is a node or relation based on start_id/end_id
            let is_relation = bootstrap_elem.start_id.is_some() && bootstrap_elem.end_id.is_some();

            // Filter by appropriate labels
            let should_process = if is_relation {
                matches_labels(&bootstrap_elem.labels, &request.relation_labels)
            } else {
                matches_labels(&bootstrap_elem.labels, &request.node_labels)
            };

            if !should_process {
                if is_relation {
                    filtered_relations += 1;
                } else {
                    filtered_nodes += 1;
                }
                continue;
            }

            // Transform to drasi-core Element
            let element = transform_element(&context.source_id, bootstrap_elem)
                .context("Failed to transform bootstrap element")?;

            // Wrap in SourceChange::Insert
            let source_change = SourceChange::Insert { element };

            // Get next sequence number for this bootstrap event
            let sequence = context.next_sequence();

            // Send via channel
            let bootstrap_event = crate::channels::BootstrapEvent {
                source_id: context.source_id.clone(),
                change: source_change,
                timestamp: Utc::now(),
                sequence,
            };

            event_tx
                .send(bootstrap_event)
                .await
                .context("Failed to send bootstrap element via channel")?;

            sent_count += 1;
        }

        debug!(
            "Filtered {} nodes and {} relations based on requested labels",
            filtered_nodes, filtered_relations
        );

        info!(
            "Completed platform bootstrap for query {}: sent {} elements",
            request.query_id, sent_count
        );

        Ok(sent_count)
    }
}

/// Check if element labels match requested labels
///
/// Returns true if requested_labels is empty (match all) or if any element label
/// is present in requested_labels
fn matches_labels(element_labels: &[String], requested_labels: &[String]) -> bool {
    requested_labels.is_empty()
        || element_labels
            .iter()
            .any(|label| requested_labels.contains(label))
}

/// Transform BootstrapElement to drasi-core Element
///
/// Converts platform SDK format to drasi-core format. Determines if element is
/// a Node or Relation based on presence of start_id/end_id fields.
fn transform_element(source_id: &str, bootstrap_elem: BootstrapElement) -> Result<Element> {
    // Convert properties from JSON Map to ElementPropertyMap
    let properties = convert_json_to_element_properties(&bootstrap_elem.properties)
        .context("Failed to convert element properties")?;

    // Convert labels to Arc slice
    let labels: Arc<[Arc<str>]> = bootstrap_elem
        .labels
        .iter()
        .map(|l| Arc::from(l.as_str()))
        .collect::<Vec<_>>()
        .into();

    // Check if this is a relation (has start_id and end_id)
    if let (Some(start_id), Some(end_id)) = (&bootstrap_elem.start_id, &bootstrap_elem.end_id) {
        // Create start and end element references
        let in_node = ElementReference::new(source_id, end_id);
        let out_node = ElementReference::new(source_id, start_id);

        Ok(Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, &bootstrap_elem.id),
                labels,
                effective_from: 0,
            },
            properties,
            in_node,
            out_node,
        })
    } else {
        // This is a node
        Ok(Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, &bootstrap_elem.id),
                labels,
                effective_from: 0,
            },
            properties,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches_labels_empty_requested() {
        // Empty requested labels should match all
        let element_labels = vec!["Person".to_string(), "Employee".to_string()];
        let requested_labels = vec![];

        assert!(matches_labels(&element_labels, &requested_labels));
    }

    #[test]
    fn test_matches_labels_matching() {
        // Should return true when element has one of the requested labels
        let element_labels = vec!["Person".to_string(), "Employee".to_string()];
        let requested_labels = vec!["Person".to_string()];

        assert!(matches_labels(&element_labels, &requested_labels));
    }

    #[test]
    fn test_matches_labels_non_matching() {
        // Should return false when element has none of the requested labels
        let element_labels = vec!["Person".to_string(), "Employee".to_string()];
        let requested_labels = vec!["Company".to_string()];

        assert!(!matches_labels(&element_labels, &requested_labels));
    }

    #[test]
    fn test_matches_labels_partial_overlap() {
        // Should return true when there's any overlap
        let element_labels = vec!["Person".to_string(), "Employee".to_string()];
        let requested_labels = vec!["Employee".to_string(), "Company".to_string()];

        assert!(matches_labels(&element_labels, &requested_labels));
    }

    #[test]
    fn test_matches_labels_empty_element() {
        // Empty element labels with non-empty requested should not match
        let element_labels = vec![];
        let requested_labels = vec!["Person".to_string()];

        assert!(!matches_labels(&element_labels, &requested_labels));
    }

    #[test]
    fn test_matches_labels_both_empty() {
        // Both empty should match (empty requested matches all)
        let element_labels = vec![];
        let requested_labels = vec![];

        assert!(matches_labels(&element_labels, &requested_labels));
    }

    #[test]
    fn test_transform_element_node() {
        // Test transforming a node (no start_id/end_id)
        let mut properties = Map::new();
        properties.insert("name".to_string(), serde_json::json!("Alice"));
        properties.insert("age".to_string(), serde_json::json!(30));

        let bootstrap_elem = BootstrapElement {
            id: "1".to_string(),
            labels: vec!["Person".to_string()],
            properties,
            start_id: None,
            end_id: None,
        };

        let element = transform_element("test-source", bootstrap_elem).unwrap();

        match element {
            Element::Node { metadata, .. } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "1");
                assert_eq!(metadata.labels.len(), 1);
                assert_eq!(metadata.labels[0].as_ref(), "Person");
            }
            _ => panic!("Expected Node element"),
        }
    }

    #[test]
    fn test_transform_element_relation() {
        // Test transforming a relation (with start_id and end_id)
        let mut properties = Map::new();
        properties.insert("since".to_string(), serde_json::json!("2020"));

        let bootstrap_elem = BootstrapElement {
            id: "r1".to_string(),
            labels: vec!["WORKS_FOR".to_string()],
            properties,
            start_id: Some("1".to_string()),
            end_id: Some("2".to_string()),
        };

        let element = transform_element("test-source", bootstrap_elem).unwrap();

        match element {
            Element::Relation {
                metadata,
                in_node,
                out_node,
                ..
            } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "r1");
                assert_eq!(metadata.labels.len(), 1);
                assert_eq!(metadata.labels[0].as_ref(), "WORKS_FOR");
                assert_eq!(out_node.element_id.as_ref(), "1");
                assert_eq!(in_node.element_id.as_ref(), "2");
            }
            _ => panic!("Expected Relation element"),
        }
    }

    #[test]
    fn test_transform_element_various_property_types() {
        // Test property conversion with various JSON value types
        let mut properties = Map::new();
        properties.insert("string_prop".to_string(), serde_json::json!("text"));
        properties.insert("number_prop".to_string(), serde_json::json!(42));
        properties.insert("float_prop".to_string(), serde_json::json!(3.14));
        properties.insert("bool_prop".to_string(), serde_json::json!(true));
        properties.insert("null_prop".to_string(), serde_json::json!(null));

        let bootstrap_elem = BootstrapElement {
            id: "1".to_string(),
            labels: vec!["Test".to_string()],
            properties,
            start_id: None,
            end_id: None,
        };

        let element = transform_element("test-source", bootstrap_elem).unwrap();

        match element {
            Element::Node { metadata, .. } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "1");
                // Properties were successfully converted
            }
            _ => panic!("Expected Node element"),
        }
    }

    #[test]
    fn test_transform_element_empty_properties() {
        // Test with empty properties
        let bootstrap_elem = BootstrapElement {
            id: "1".to_string(),
            labels: vec!["Empty".to_string()],
            properties: Map::new(),
            start_id: None,
            end_id: None,
        };

        let element = transform_element("test-source", bootstrap_elem).unwrap();

        match element {
            Element::Node { metadata, .. } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "1");
                // Properties transformed successfully (even if empty)
            }
            _ => panic!("Expected Node element"),
        }
    }
}
