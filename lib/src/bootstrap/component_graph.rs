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

//! Component Graph Bootstrap Provider
//!
//! Generates bootstrap data from the [`ComponentGraph`] for the component graph source.
//! Takes a single atomic snapshot of the graph and translates all nodes and edges
//! into `SourceChange::Insert` elements, guaranteeing a consistent view of the
//! component topology.

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use log::info;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use crate::channels::*;
use crate::component_graph::{ComponentGraph, ComponentKind, RelationshipKind};
use crate::config::SourceSubscriptionSettings;
use crate::sources::component_graph_source::COMPONENT_GRAPH_SOURCE_ID;

/// Bootstrap provider that generates a consistent snapshot from the [`ComponentGraph`].
///
/// Takes a single read lock on the graph, iterates all nodes and edges, and
/// translates them into `SourceChange::Insert` elements. This replaces the
/// previous approach of querying three separate managers independently.
pub struct ComponentGraphBootstrapProvider {
    graph: Arc<RwLock<ComponentGraph>>,
}

impl ComponentGraphBootstrapProvider {
    pub fn new(graph: Arc<RwLock<ComponentGraph>>) -> Self {
        Self { graph }
    }
}

#[async_trait]
impl BootstrapProvider for ComponentGraphBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        _context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<usize> {
        info!(
            "Component graph bootstrap for query '{}' starting",
            request.query_id
        );

        // Take a single atomic snapshot of the graph
        let snapshot = self.graph.read().await.snapshot();
        let mut count: u64 = 0;

        // Emit all nodes
        for node in &snapshot.nodes {
            // Skip the component graph source itself to avoid self-reference
            if node.id == COMPONENT_GRAPH_SOURCE_ID {
                continue;
            }

            let (label, prefix) = match node.kind {
                ComponentKind::Instance => ("DrasiInstance", "instance"),
                ComponentKind::Source => ("Source", "source"),
                ComponentKind::Query => ("Query", "query"),
                ComponentKind::Reaction => ("Reaction", "reaction"),
                ComponentKind::BootstrapProvider => continue,
                ComponentKind::IdentityProvider => continue,
            };

            let node_id = format!("{prefix}:{}", node.id);

            // Build properties from status + metadata
            let mut props = ElementPropertyMap::new();
            props.insert("id", ElementValue::String(Arc::from(node.id.as_str())));
            props.insert(
                "status",
                ElementValue::String(Arc::from(status_str(&node.status))),
            );

            // Add metadata properties (kind, query, autoStart, etc.)
            for (k, v) in &node.metadata {
                props.insert(k, ElementValue::String(Arc::from(v.as_str())));
            }

            // Special case: DrasiInstance gets "running" property
            if matches!(node.kind, ComponentKind::Instance) {
                props.insert("running", ElementValue::String(Arc::from("true")));
            }

            let element = make_node(&node_id, &[label], props);
            let _ = event_tx
                .send(BootstrapEvent {
                    source_id: COMPONENT_GRAPH_SOURCE_ID.to_string(),
                    change: SourceChange::Insert { element },
                    timestamp: chrono::Utc::now(),
                    sequence: count,
                })
                .await;
            count += 1;
        }

        // Emit all edges as relations
        for edge in &snapshot.edges {
            // Skip edges involving the component graph source itself
            if edge.from == COMPONENT_GRAPH_SOURCE_ID || edge.to == COMPONENT_GRAPH_SOURCE_ID {
                continue;
            }

            let (label, rel_id, from_prefix, to_prefix) = match &edge.relationship {
                RelationshipKind::Owns => {
                    // Instance → Component: emit HAS_SOURCE/HAS_QUERY/HAS_REACTION
                    let to_node = snapshot.nodes.iter().find(|n| n.id == edge.to);
                    let (has_label, rel_prefix) = match to_node.map(|n| &n.kind) {
                        Some(ComponentKind::Source) => ("HAS_SOURCE", "has_source"),
                        Some(ComponentKind::Query) => ("HAS_QUERY", "has_query"),
                        Some(ComponentKind::Reaction) => ("HAS_REACTION", "has_reaction"),
                        _ => continue,
                    };
                    let rel_id = format!("rel:{rel_prefix}:{}:{}", edge.from, edge.to);
                    (has_label, rel_id, "instance", "")
                }
                RelationshipKind::Feeds => {
                    // Source→Query or Query→Reaction
                    let from_node = snapshot.nodes.iter().find(|n| n.id == edge.from);
                    match from_node.map(|n| &n.kind) {
                        Some(ComponentKind::Source) => {
                            // Source feeds Query → SUBSCRIBES_TO (Query→Source in Cypher)
                            let rel_id = format!("rel:subscribes:{}:{}", edge.to, edge.from);
                            ("SUBSCRIBES_TO", rel_id, "query", "source")
                        }
                        Some(ComponentKind::Query) => {
                            // Query feeds Reaction → LISTENS_TO (Reaction→Query in Cypher)
                            let rel_id = format!("rel:listens:{}:{}", edge.to, edge.from);
                            ("LISTENS_TO", rel_id, "reaction", "query")
                        }
                        _ => continue,
                    }
                }
                // Skip reverse/non-data edges (OwnedBy, SubscribesTo, etc.)
                _ => continue,
            };

            let (in_node_id, out_node_id) = match &edge.relationship {
                RelationshipKind::Owns => {
                    let to_node = snapshot.nodes.iter().find(|n| n.id == edge.to);
                    let to_prefix = match to_node.map(|n| &n.kind) {
                        Some(ComponentKind::Source) => "source",
                        Some(ComponentKind::Query) => "query",
                        Some(ComponentKind::Reaction) => "reaction",
                        _ => continue,
                    };
                    (
                        format!("{from_prefix}:{}", edge.from),
                        format!("{to_prefix}:{}", edge.to),
                    )
                }
                RelationshipKind::Feeds => (
                    format!("{from_prefix}:{}", edge.to),
                    format!("{to_prefix}:{}", edge.from),
                ),
                _ => continue,
            };

            let rel = make_relation(
                &rel_id,
                &[label],
                &in_node_id,
                &out_node_id,
                ElementPropertyMap::new(),
            );
            let _ = event_tx
                .send(BootstrapEvent {
                    source_id: COMPONENT_GRAPH_SOURCE_ID.to_string(),
                    change: SourceChange::Insert { element: rel },
                    timestamp: chrono::Utc::now(),
                    sequence: count,
                })
                .await;
            count += 1;
        }

        info!(
            "Component graph bootstrap complete: {} elements for query '{}'",
            count, request.query_id
        );
        Ok(count as usize)
    }
}

// ============================================================================
// Helper functions for building graph elements
// ============================================================================

fn make_node(element_id: &str, labels: &[&str], props: ElementPropertyMap) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(COMPONENT_GRAPH_SOURCE_ID, element_id),
            labels: labels
                .iter()
                .map(|l| Arc::from(*l))
                .collect::<Vec<_>>()
                .into(),
            effective_from: now_ms(),
        },
        properties: props,
    }
}

fn make_relation(
    element_id: &str,
    labels: &[&str],
    in_node_id: &str,
    out_node_id: &str,
    props: ElementPropertyMap,
) -> Element {
    Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new(COMPONENT_GRAPH_SOURCE_ID, element_id),
            labels: labels
                .iter()
                .map(|l| Arc::from(*l))
                .collect::<Vec<_>>()
                .into(),
            effective_from: now_ms(),
        },
        in_node: ElementReference::new(COMPONENT_GRAPH_SOURCE_ID, in_node_id),
        out_node: ElementReference::new(COMPONENT_GRAPH_SOURCE_ID, out_node_id),
        properties: props,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn status_str(status: &ComponentStatus) -> &'static str {
    match status {
        ComponentStatus::Stopped => "Stopped",
        ComponentStatus::Starting => "Starting",
        ComponentStatus::Running => "Running",
        ComponentStatus::Stopping => "Stopping",
        ComponentStatus::Reconfiguring => "Reconfiguring",
        ComponentStatus::Error => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    fn make_request(query_id: &str) -> BootstrapRequest {
        BootstrapRequest {
            query_id: query_id.to_string(),
            node_labels: vec![],
            relation_labels: vec![],
            request_id: "test-request".to_string(),
        }
    }

    fn make_context() -> BootstrapContext {
        BootstrapContext::new_minimal(
            "test-server".to_string(),
            COMPONENT_GRAPH_SOURCE_ID.to_string(),
        )
    }

    #[test]
    fn test_new_creates_provider_with_graph_reference() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        let graph = Arc::new(RwLock::new(graph));
        let _provider = ComponentGraphBootstrapProvider::new(graph);
    }

    #[tokio::test]
    async fn test_bootstrap_empty_graph() {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        let graph = Arc::new(RwLock::new(graph));
        let provider = ComponentGraphBootstrapProvider::new(graph);

        let (tx, mut rx) = mpsc::channel::<BootstrapEvent>(100);
        let request = make_request("test-query");
        let context = make_context();

        let count = provider
            .bootstrap(request, &context, tx, None)
            .await
            .unwrap();

        // Only the instance root node is present; it is emitted as DrasiInstance
        assert_eq!(count, 1);

        let event = rx.recv().await.unwrap();
        match &event.change {
            SourceChange::Insert { element } => match element {
                Element::Node { metadata, .. } => {
                    let labels: Vec<&str> = metadata.labels.iter().map(|l| l.as_ref()).collect();
                    assert!(labels.contains(&"DrasiInstance"));
                }
                _ => panic!("Expected Node element for instance"),
            },
            _ => panic!("Expected Insert change"),
        }
    }

    #[tokio::test]
    async fn test_bootstrap_with_sources_and_queries() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.register_source("src1", HashMap::new()).unwrap();
        graph.register_source("src2", HashMap::new()).unwrap();
        graph
            .register_query(
                "q1",
                HashMap::new(),
                &["src1".to_string(), "src2".to_string()],
            )
            .unwrap();

        let graph = Arc::new(RwLock::new(graph));
        let provider = ComponentGraphBootstrapProvider::new(graph);

        let (tx, mut rx) = mpsc::channel::<BootstrapEvent>(100);
        let request = make_request("test-query");
        let context = make_context();

        let count = provider
            .bootstrap(request, &context, tx, None)
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        assert_eq!(count, events.len());

        let mut node_count = 0;
        let mut relation_count = 0;
        for event in &events {
            match &event.change {
                SourceChange::Insert { element } => match element {
                    Element::Node { .. } => node_count += 1,
                    Element::Relation { .. } => relation_count += 1,
                },
                _ => panic!("Expected Insert change"),
            }
        }

        // 4 nodes: instance + src1 + src2 + q1
        assert_eq!(node_count, 4);
        // 5 relations: HAS_SOURCE×2 + HAS_QUERY×1 + SUBSCRIBES_TO×2
        assert_eq!(relation_count, 5);
        assert_eq!(count, 9);
    }

    #[tokio::test]
    async fn test_bootstrap_emitted_labels_match_component_kind() {
        let (mut graph, _rx) = ComponentGraph::new("test-instance");
        graph.register_source("my-source", HashMap::new()).unwrap();
        graph
            .register_query("my-query", HashMap::new(), &["my-source".to_string()])
            .unwrap();
        graph
            .register_reaction("my-reaction", HashMap::new(), &["my-query".to_string()])
            .unwrap();

        let graph = Arc::new(RwLock::new(graph));
        let provider = ComponentGraphBootstrapProvider::new(graph);

        let (tx, mut rx) = mpsc::channel::<BootstrapEvent>(100);
        let request = make_request("test-query");
        let context = make_context();

        provider
            .bootstrap(request, &context, tx, None)
            .await
            .unwrap();

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let mut found_instance = false;
        let mut found_source = false;
        let mut found_query = false;
        let mut found_reaction = false;

        for event in &events {
            if let SourceChange::Insert { element } = &event.change {
                if let Element::Node { metadata, .. } = element {
                    let labels: Vec<&str> = metadata.labels.iter().map(|l| l.as_ref()).collect();
                    if labels.contains(&"DrasiInstance") {
                        found_instance = true;
                    }
                    if labels.contains(&"Source") {
                        found_source = true;
                    }
                    if labels.contains(&"Query") {
                        found_query = true;
                    }
                    if labels.contains(&"Reaction") {
                        found_reaction = true;
                    }
                }
            }
        }

        assert!(found_instance, "Should emit DrasiInstance label");
        assert!(found_source, "Should emit Source label");
        assert!(found_query, "Should emit Query label");
        assert!(found_reaction, "Should emit Reaction label");
    }
}
