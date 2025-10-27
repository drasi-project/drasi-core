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

//! Query configuration builders

use crate::api::Properties;
use crate::config::{QueryConfig, QueryJoinConfig, QueryLanguage};
use serde_json::Value;
use std::collections::HashMap;

/// Fluent builder for Query configuration
#[derive(Debug, Clone)]
pub struct QueryBuilder {
    id: String,
    query: String,
    query_language: QueryLanguage,
    sources: Vec<String>,
    auto_start: bool,
    properties: HashMap<String, Value>,
    joins: Option<Vec<QueryJoinConfig>>,
}

impl QueryBuilder {
    /// Create a new query builder
    fn new(id: impl Into<String>, query: impl Into<String>, language: QueryLanguage) -> Self {
        Self {
            id: id.into(),
            query: query.into(),
            query_language: language,
            sources: Vec::new(),
            auto_start: true,
            properties: HashMap::new(),
            joins: None,
        }
    }

    /// Set the query string
    pub fn query(mut self, query: impl Into<String>) -> Self {
        self.query = query.into();
        self
    }

    /// Add a source this query subscribes to
    pub fn from_source(mut self, source_id: impl Into<String>) -> Self {
        self.sources.push(source_id.into());
        self
    }

    /// Add multiple sources this query subscribes to
    pub fn from_sources(mut self, source_ids: Vec<String>) -> Self {
        self.sources.extend(source_ids);
        self
    }

    /// Set whether to auto-start this query (default: true)
    pub fn auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Add a property
    pub fn with_property(mut self, key: impl Into<String>, value: Value) -> Self {
        self.properties.insert(key.into(), value);
        self
    }

    /// Set properties using the Properties builder
    pub fn with_properties(mut self, properties: Properties) -> Self {
        self.properties = properties.build();
        self
    }

    /// Add a join configuration
    pub fn with_join(mut self, join: QueryJoinConfig) -> Self {
        self.joins.get_or_insert_with(Vec::new).push(join);
        self
    }

    /// Add multiple join configurations
    pub fn with_joins(mut self, joins: Vec<QueryJoinConfig>) -> Self {
        self.joins = Some(joins);
        self
    }

    /// Build the query configuration
    pub fn build(self) -> QueryConfig {
        QueryConfig {
            id: self.id,
            query: self.query,
            query_language: self.query_language,
            sources: self.sources,
            auto_start: self.auto_start,
            properties: self.properties,
            joins: self.joins,
            enable_bootstrap: true,           // Default: bootstrap enabled
            bootstrap_buffer_size: 10000,     // Default buffer size
            priority_queue_capacity: None,    // Default: inherit from server global setting
            broadcast_channel_capacity: None, // Default: inherit from server global setting
            dispatch_mode: None,               // Default: inherit from server global setting or Broadcast
        }
    }
}

/// Query configuration factory
pub struct Query;

impl Query {
    /// Create a Cypher query
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::Query;
    ///
    /// let query = Query::cypher("active-orders")
    ///     .query("MATCH (o:Order) WHERE o.status = 'active' RETURN o")
    ///     .from_source("orders")
    ///     .build();
    /// ```
    pub fn cypher(id: impl Into<String>) -> QueryBuilder {
        QueryBuilder::new(id, "", QueryLanguage::Cypher)
    }

    /// Create a GQL (GraphQL) query
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::Query;
    ///
    /// let query = Query::gql("users-query")
    ///     .query("{ users { id name } }")
    ///     .from_source("users-source")
    ///     .build();
    /// ```
    pub fn gql(id: impl Into<String>) -> QueryBuilder {
        QueryBuilder::new(id, "", QueryLanguage::GQL)
    }
}
