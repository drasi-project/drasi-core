// Copyright 2026 The Drasi Authors.
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

//! Neo4j bootstrap provider implementation.

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use log::info;
use neo4rs::{query, BoltMap, BoltType, ConfigBuilder, Graph};
use ordered_float::OrderedFloat;
use std::sync::Arc;

use crate::config::Neo4jBootstrapConfig;

pub struct Neo4jBootstrapProvider {
    config: Neo4jBootstrapConfig,
}

impl Neo4jBootstrapProvider {
    pub fn new(config: Neo4jBootstrapConfig) -> Self {
        Self { config }
    }

    pub fn builder() -> Neo4jBootstrapProviderBuilder {
        Neo4jBootstrapProviderBuilder::new()
    }
}

pub struct Neo4jBootstrapProviderBuilder {
    uri: String,
    user: String,
    password: String,
    database: String,
    labels: Vec<String>,
    rel_types: Vec<String>,
}

impl Neo4jBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            uri: "bolt://localhost:7687".to_string(),
            user: "neo4j".to_string(),
            password: String::new(),
            database: "neo4j".to_string(),
            labels: Vec::new(),
            rel_types: Vec::new(),
        }
    }

    pub fn with_uri(mut self, uri: impl Into<String>) -> Self {
        self.uri = uri.into();
        self
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = password.into();
        self
    }

    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    pub fn with_labels(mut self, labels: Vec<String>) -> Self {
        self.labels = labels;
        self
    }

    pub fn with_rel_types(mut self, rel_types: Vec<String>) -> Self {
        self.rel_types = rel_types;
        self
    }

    pub fn build(self) -> Neo4jBootstrapProvider {
        Neo4jBootstrapProvider::new(Neo4jBootstrapConfig {
            uri: self.uri,
            user: self.user,
            password: self.password,
            database: self.database,
            labels: self.labels,
            rel_types: self.rel_types,
        })
    }
}

impl Default for Neo4jBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BootstrapProvider for Neo4jBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        let graph = connect_graph(&self.config).await?;
        let mut total = 0usize;

        let mut cursor_stream = graph
            .execute(query("CALL db.cdc.current() YIELD id RETURN id"))
            .await?;
        if let Ok(Some(row)) = cursor_stream.next().await {
            let cursor: String = row.get("id")?;
            info!(
                "Neo4j bootstrap for source '{}' starting at CDC cursor '{}'",
                context.source_id, cursor
            );
        }

        let node_labels = if request.node_labels.is_empty() {
            self.config.labels.clone()
        } else {
            request.node_labels.clone()
        };
        let relation_types = if request.relation_labels.is_empty() {
            self.config.rel_types.clone()
        } else {
            request.relation_labels.clone()
        };

        total +=
            bootstrap_nodes(&graph, &context.source_id, &node_labels, context, &event_tx).await?;
        total += bootstrap_relationships(
            &graph,
            &context.source_id,
            &relation_types,
            context,
            &event_tx,
        )
        .await?;

        Ok(total)
    }
}

async fn bootstrap_nodes(
    graph: &Graph,
    source_id: &str,
    labels: &[String],
    context: &BootstrapContext,
    event_tx: &BootstrapEventSender,
) -> Result<usize> {
    let mut count = 0usize;
    if labels.is_empty() {
        count += stream_nodes_query(
            graph,
            "MATCH (n) RETURN elementId(n) AS element_id, labels(n) AS labels, properties(n) AS props",
            source_id,
            context,
            event_tx,
        )
        .await?;
        return Ok(count);
    }

    for label in labels {
        let cypher = format!(
            "MATCH (n:{}) RETURN elementId(n) AS element_id, labels(n) AS labels, properties(n) AS props",
            quote_ident(label)
        );
        count += stream_nodes_query(graph, &cypher, source_id, context, event_tx).await?;
    }
    Ok(count)
}

async fn stream_nodes_query(
    graph: &Graph,
    cypher: &str,
    source_id: &str,
    context: &BootstrapContext,
    event_tx: &BootstrapEventSender,
) -> Result<usize> {
    let mut count = 0usize;
    let mut stream = graph.execute(query(cypher)).await?;
    while let Ok(Some(row)) = stream.next().await {
        let element_id: String = row.get("element_id")?;
        let labels: Vec<String> = row.get("labels")?;
        let props = row.get::<BoltType>("props")?;

        let properties = match props {
            BoltType::Map(map) => bolt_map_to_properties(&map)?,
            _ => ElementPropertyMap::new(),
        };

        let metadata = ElementMetadata {
            reference: ElementReference::new(source_id, &element_id),
            labels: labels
                .iter()
                .map(|label| Arc::from(label.as_str()))
                .collect::<Vec<_>>()
                .into(),
            effective_from: now_ms(),
        };
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata,
                properties,
            },
        };

        event_tx
            .send(BootstrapEvent {
                source_id: source_id.to_string(),
                change,
                timestamp: Utc::now(),
                sequence: context.next_sequence(),
            })
            .await?;
        count += 1;
    }
    Ok(count)
}

async fn bootstrap_relationships(
    graph: &Graph,
    source_id: &str,
    rel_types: &[String],
    context: &BootstrapContext,
    event_tx: &BootstrapEventSender,
) -> Result<usize> {
    let mut count = 0usize;
    if rel_types.is_empty() {
        count += stream_relationship_query(
            graph,
            "MATCH (a)-[r]->(b) \
             RETURN elementId(r) AS element_id, type(r) AS rel_type, \
                    elementId(a) AS start_element_id, elementId(b) AS end_element_id, \
                    properties(r) AS props",
            source_id,
            context,
            event_tx,
        )
        .await?;
        return Ok(count);
    }

    for rel_type in rel_types {
        let cypher = format!(
            "MATCH (a)-[r:{}]->(b) \
             RETURN elementId(r) AS element_id, type(r) AS rel_type, \
                    elementId(a) AS start_element_id, elementId(b) AS end_element_id, \
                    properties(r) AS props",
            quote_ident(rel_type)
        );
        count += stream_relationship_query(graph, &cypher, source_id, context, event_tx).await?;
    }
    Ok(count)
}

async fn stream_relationship_query(
    graph: &Graph,
    cypher: &str,
    source_id: &str,
    context: &BootstrapContext,
    event_tx: &BootstrapEventSender,
) -> Result<usize> {
    let mut count = 0usize;
    let mut stream = graph.execute(query(cypher)).await?;
    while let Ok(Some(row)) = stream.next().await {
        let element_id: String = row.get("element_id")?;
        let rel_type: String = row.get("rel_type")?;
        let start_element_id: String = row.get("start_element_id")?;
        let end_element_id: String = row.get("end_element_id")?;
        let props = row.get::<BoltType>("props")?;

        let properties = match props {
            BoltType::Map(map) => bolt_map_to_properties(&map)?,
            _ => ElementPropertyMap::new(),
        };

        let metadata = ElementMetadata {
            reference: ElementReference::new(source_id, &element_id),
            labels: vec![Arc::from(rel_type.as_str())].into(),
            effective_from: now_ms(),
        };
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata,
                properties,
                in_node: ElementReference::new(source_id, &start_element_id),
                out_node: ElementReference::new(source_id, &end_element_id),
            },
        };

        event_tx
            .send(BootstrapEvent {
                source_id: source_id.to_string(),
                change,
                timestamp: Utc::now(),
                sequence: context.next_sequence(),
            })
            .await?;
        count += 1;
    }
    Ok(count)
}

fn quote_ident(input: &str) -> String {
    format!("`{}`", input.replace('`', "``"))
}

fn normalize_uri(uri: &str) -> String {
    uri.trim()
        .trim_start_matches("bolt://")
        .trim_start_matches("neo4j://")
        .to_string()
}

async fn connect_graph(config: &Neo4jBootstrapConfig) -> Result<Graph> {
    let neo4j_config = ConfigBuilder::default()
        .uri(normalize_uri(&config.uri))
        .user(config.user.as_str())
        .password(config.password.as_str())
        .db(config.database.as_str())
        .build()?;
    Ok(Graph::connect(neo4j_config).await?)
}

fn bolt_map_to_properties(map: &BoltMap) -> Result<ElementPropertyMap> {
    let mut properties = ElementPropertyMap::new();
    for (key, value) in &map.value {
        properties.insert(&key.value, bolt_type_to_value(value)?);
    }
    Ok(properties)
}

fn bolt_type_to_value(value: &BoltType) -> Result<drasi_core::models::ElementValue> {
    Ok(match value {
        BoltType::Null(_) => drasi_core::models::ElementValue::Null,
        BoltType::Boolean(v) => drasi_core::models::ElementValue::Bool(v.value),
        BoltType::Integer(v) => drasi_core::models::ElementValue::Integer(v.value),
        BoltType::Float(v) => drasi_core::models::ElementValue::Float(OrderedFloat(v.value)),
        BoltType::String(v) => {
            drasi_core::models::ElementValue::String(Arc::from(v.value.as_str()))
        }
        BoltType::List(v) => drasi_core::models::ElementValue::List(
            v.value
                .iter()
                .map(bolt_type_to_value)
                .collect::<Result<Vec<_>>>()?,
        ),
        BoltType::Map(v) => drasi_core::models::ElementValue::Object(bolt_map_to_properties(v)?),
        other => drasi_core::models::ElementValue::String(Arc::from(other.to_string())),
    })
}

fn now_ms() -> u64 {
    Utc::now().timestamp_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_builds_provider() {
        let provider = Neo4jBootstrapProvider::builder()
            .with_uri("bolt://localhost:7687")
            .with_user("neo4j")
            .with_password("pw")
            .with_database("neo4j")
            .with_labels(vec!["Person".to_string()])
            .with_rel_types(vec!["ACTED_IN".to_string()])
            .build();

        let _ = provider;
    }
}
