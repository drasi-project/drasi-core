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
use log::debug;
use std::collections::HashSet;
use std::sync::Arc;

// Import drasi-core components
use crate::config::QueryLanguage;
use drasi_query_ast::{
    api::{QueryConfiguration, QueryParser},
    ast::{MatchClause, QueryPart},
};
use drasi_query_cypher::CypherParser;
use drasi_query_gql::GQLParser;

/// Default configuration for label extraction
struct DefaultQueryConfig;

impl QueryConfiguration for DefaultQueryConfig {
    fn get_aggregating_function_names(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        set.insert("count".into());
        set.insert("sum".into());
        set.insert("min".into());
        set.insert("max".into());
        set.insert("avg".into());
        set.insert("collect".into());
        set.insert("stdev".into());
        set.insert("stdevp".into());
        set
    }
}

/// Extracts all node and relation labels from a query
pub struct LabelExtractor;

impl LabelExtractor {
    /// Extract all labels referenced in a query
    pub fn extract_labels(query_str: &str, query_language: &QueryLanguage) -> Result<QueryLabels> {
        // Create parser based on query language
        let config = Arc::new(DefaultQueryConfig);
        let parser: Arc<dyn QueryParser> = match query_language {
            QueryLanguage::Cypher => Arc::new(CypherParser::new(config)),
            QueryLanguage::GQL => Arc::new(GQLParser::new(config)),
        };

        // Parse the query using drasi-core's parser
        let parsed_query = parser.parse(query_str)?;

        let mut node_labels = HashSet::new();
        let mut relation_labels = HashSet::new();

        // Process each query part
        for query_part in &parsed_query.parts {
            Self::extract_from_query_part(query_part, &mut node_labels, &mut relation_labels);
        }

        debug!("Extracted node labels: {node_labels:?}");
        debug!("Extracted relation labels: {relation_labels:?}");

        Ok(QueryLabels {
            node_labels: node_labels.into_iter().collect(),
            relation_labels: relation_labels.into_iter().collect(),
        })
    }

    fn extract_from_query_part(
        query_part: &QueryPart,
        node_labels: &mut HashSet<String>,
        relation_labels: &mut HashSet<String>,
    ) {
        // Process all match clauses
        for match_clause in &query_part.match_clauses {
            Self::extract_from_match_clause(match_clause, node_labels, relation_labels);
        }
    }

    fn extract_from_match_clause(
        match_clause: &MatchClause,
        node_labels: &mut HashSet<String>,
        relation_labels: &mut HashSet<String>,
    ) {
        // Extract labels from the start node
        for label in &match_clause.start.labels {
            node_labels.insert(label.to_string());
        }

        // Extract labels from the path (relations and nodes)
        for (relation_match, node_match) in &match_clause.path {
            // Extract relation labels
            for label in &relation_match.labels {
                relation_labels.insert(label.to_string());
            }

            // Extract node labels
            for label in &node_match.labels {
                node_labels.insert(label.to_string());
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueryLabels {
    pub node_labels: Vec<String>,
    pub relation_labels: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_simple_node_labels() {
        let query = "MATCH (n:Person) RETURN n";
        let labels = LabelExtractor::extract_labels(query, &QueryLanguage::Cypher).unwrap();

        assert_eq!(labels.node_labels.len(), 1);
        assert!(labels.node_labels.contains(&"Person".to_string()));
        assert_eq!(labels.relation_labels.len(), 0);
    }

    #[test]
    fn test_extract_multiple_node_labels() {
        let query = "MATCH (n:Person|Employee) RETURN n";
        let labels = LabelExtractor::extract_labels(query, &QueryLanguage::Cypher).unwrap();

        assert_eq!(labels.node_labels.len(), 2);
        assert!(labels.node_labels.contains(&"Person".to_string()));
        assert!(labels.node_labels.contains(&"Employee".to_string()));
    }

    #[test]
    fn test_extract_relation_labels() {
        let query = "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a, b";
        let labels = LabelExtractor::extract_labels(query, &QueryLanguage::Cypher).unwrap();

        assert_eq!(labels.node_labels.len(), 1);
        assert!(labels.node_labels.contains(&"Person".to_string()));
        assert_eq!(labels.relation_labels.len(), 1);
        assert!(labels.relation_labels.contains(&"KNOWS".to_string()));
    }

    #[test]
    fn test_extract_complex_query() {
        let query = r#"
            MATCH (p:Person)-[r:WORKS_AT]->(c:Company)
            OPTIONAL MATCH (p)-[f:FRIEND_OF]->(friend:Person)
            WHERE c.name = 'Acme Corp'
            RETURN p, c, friend
        "#;
        let labels = LabelExtractor::extract_labels(query, &QueryLanguage::Cypher).unwrap();

        assert_eq!(labels.node_labels.len(), 2);
        assert!(labels.node_labels.contains(&"Person".to_string()));
        assert!(labels.node_labels.contains(&"Company".to_string()));

        assert_eq!(labels.relation_labels.len(), 2);
        assert!(labels.relation_labels.contains(&"WORKS_AT".to_string()));
        assert!(labels.relation_labels.contains(&"FRIEND_OF".to_string()));
    }
}
