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

//! Schema discovery types for source introspection.
//!
//! This module provides types for best-effort schema reporting by sources. Higher
//! layers (inspection APIs, MCP adapters, LLM-powered tools) consume these types
//! to understand the shape of the data graph without reverse-engineering
//! source-specific configuration.
//!
//! # Architecture
//!
//! - [`SourceSchema`] — per-source schema reported by `Source::describe_schema()`
//! - [`GraphSchema`] — merged view across all sources and queries (used by inspection APIs)
//! - [`normalize_table_label`] — utility for deriving node labels from qualified table names

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Best-effort schema information reported by a source.
///
/// Sources may return this from `Source::describe_schema()` so higher layers
/// (such as inspection APIs or MCP adapters) can understand the graph shape
/// without reverse-engineering source-specific configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SourceSchema {
    #[serde(default)]
    pub nodes: Vec<NodeSchema>,
    #[serde(default)]
    pub relations: Vec<RelationSchema>,
}

impl SourceSchema {
    /// Returns true when the schema contains no node or relation declarations.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.relations.is_empty()
    }
}

/// Schema for a single node label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct NodeSchema {
    pub label: String,
    #[serde(default)]
    pub properties: Vec<PropertySchema>,
}

impl NodeSchema {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            properties: Vec::new(),
        }
    }
}

/// Schema for a single relationship label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RelationSchema {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default)]
    pub properties: Vec<PropertySchema>,
}

impl RelationSchema {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            from: None,
            to: None,
            properties: Vec::new(),
        }
    }
}

/// Schema for a single property.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PropertySchema {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<PropertyType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl PropertySchema {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            data_type: None,
            description: None,
        }
    }
}

/// Cross-source property type hints for schema discovery.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PropertyType {
    String,
    Integer,
    Float,
    Boolean,
    Timestamp,
    Json,
}

/// Merged graph schema used by inspection APIs and future MCP tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct GraphSchema {
    #[serde(default)]
    pub nodes: BTreeMap<String, GraphNodeSchema>,
    #[serde(default)]
    pub relations: BTreeMap<String, GraphRelationSchema>,
    #[serde(default)]
    pub sources_without_schema: BTreeSet<String>,
}

impl GraphSchema {
    /// Merge a source-provided schema into the aggregate graph view.
    pub fn merge_source_schema(&mut self, source_id: &str, schema: &SourceSchema) {
        for node in &schema.nodes {
            let entry = self.nodes.entry(node.label.clone()).or_default();
            entry.sources.insert(source_id.to_string());
            merge_properties(&mut entry.properties, &node.properties);
        }

        for relation in &schema.relations {
            let entry = self.relations.entry(relation.label.clone()).or_default();
            entry.sources.insert(source_id.to_string());

            if entry.from.is_none() {
                entry.from = relation.from.clone();
            } else if entry.from != relation.from && relation.from.is_some() {
                log::debug!(
                    "Relation '{}': source '{}' reports from={:?}, but existing entry has from={:?}; keeping existing",
                    relation.label, source_id, relation.from, entry.from
                );
            }
            if entry.to.is_none() {
                entry.to = relation.to.clone();
            } else if entry.to != relation.to && relation.to.is_some() {
                log::debug!(
                    "Relation '{}': source '{}' reports to={:?}, but existing entry has to={:?}; keeping existing",
                    relation.label, source_id, relation.to, entry.to
                );
            }

            merge_properties(&mut entry.properties, &relation.properties);
        }
    }

    /// Mark node labels as being referenced by a query.
    pub fn mark_queried_nodes<'a, I>(&mut self, labels: I, query_id: &str)
    where
        I: IntoIterator<Item = &'a str>,
    {
        for label in labels {
            let entry = self.nodes.entry(label.to_string()).or_default();
            entry.queried_by.insert(query_id.to_string());
        }
    }

    /// Mark relationship labels as being referenced by a query.
    pub fn mark_queried_relations<'a, I>(&mut self, labels: I, query_id: &str)
    where
        I: IntoIterator<Item = &'a str>,
    {
        for label in labels {
            let entry = self.relations.entry(label.to_string()).or_default();
            entry.queried_by.insert(query_id.to_string());
        }
    }

    /// Record a source that exists but could not describe its schema.
    pub fn record_source_without_schema(&mut self, source_id: &str) {
        self.sources_without_schema.insert(source_id.to_string());
    }
}

/// Aggregated node schema across one or more sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct GraphNodeSchema {
    #[serde(default)]
    pub sources: BTreeSet<String>,
    #[serde(default)]
    pub queried_by: BTreeSet<String>,
    #[serde(default)]
    pub properties: Vec<PropertySchema>,
}

/// Aggregated relationship schema across one or more sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct GraphRelationSchema {
    #[serde(default)]
    pub sources: BTreeSet<String>,
    #[serde(default)]
    pub queried_by: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default)]
    pub properties: Vec<PropertySchema>,
}

fn merge_properties(target: &mut Vec<PropertySchema>, incoming: &[PropertySchema]) {
    for property in incoming {
        if let Some(existing) = target.iter_mut().find(|p| p.name == property.name) {
            if existing.data_type.is_none() {
                existing.data_type = property.data_type;
            }
            if existing.description.is_none() {
                existing.description = property.description.clone();
            }
        } else {
            target.push(property.clone());
        }
    }

    target.sort_by(|a, b| a.name.cmp(&b.name));
}

/// Strip an optional schema prefix from a qualified table name to derive a node label.
///
/// For example, `"public.users"` becomes `"users"` and `"orders"` stays `"orders"`.
/// This is used by database sources (Postgres, MSSQL) when converting configured
/// table names into graph node labels.
pub fn normalize_table_label(table: &str) -> String {
    table.rsplit('.').next().unwrap_or(table).to_string()
}
