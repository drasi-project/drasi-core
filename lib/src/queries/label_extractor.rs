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
use drasi_query_ast::api::{QueryConfiguration, QueryParser};
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
        let mut all_node_labels = false;
        let mut all_relation_labels = false;

        for query_part in &parsed_query.parts {
            for clause in &query_part.match_clauses {
                all_node_labels |= clause.start.labels.is_empty();
                node_labels.extend(clause.start.labels.iter().map(ToString::to_string));
                for (relation, node) in &clause.path {
                    all_node_labels |= node.labels.is_empty()
                        || relation.variable_length.as_ref().is_some_and(|length| {
                            length
                                .max_hops
                                .or(length.min_hops)
                                .map_or(true, |max| max > 1)
                        });
                    all_relation_labels |= relation.labels.is_empty();
                    relation_labels.extend(relation.labels.iter().map(ToString::to_string));
                    node_labels.extend(node.labels.iter().map(ToString::to_string));
                }
            }
        }

        debug!("Extracted node labels: {node_labels:?}");
        debug!("Extracted relation labels: {relation_labels:?}");

        Ok(QueryLabels {
            node_labels: node_labels.into_iter().collect(),
            relation_labels: relation_labels.into_iter().collect(),
            all_node_labels,
            all_relation_labels,
        })
    }
}

/// The set of node and relation labels extracted from a parsed query.
#[derive(Debug, Clone, Default)]
pub struct QueryLabels {
    pub node_labels: Vec<String>,
    pub relation_labels: Vec<String>,
    pub all_node_labels: bool,
    pub all_relation_labels: bool,
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
        assert!(!labels.all_node_labels);
        assert!(!labels.all_relation_labels);
    }

    #[test]
    fn repetition_broadens_nodes_only_when_intermediate_nodes_are_possible() {
        for language in [QueryLanguage::Cypher, QueryLanguage::GQL] {
            for (repetition, all_nodes) in [
                ("*0", false),
                ("*1", false),
                ("*0..1", false),
                ("*2", true),
                ("*0..2", true),
                ("*..2", true),
            ] {
                let text = format!("MATCH (a:Start)-[:R{repetition}]->(b:End) RETURN b");
                let labels = LabelExtractor::extract_labels(&text, &language).unwrap();
                assert_eq!(labels.all_node_labels, all_nodes, "{text}");
                assert!(!labels.all_relation_labels, "{text}");
                assert!(labels.node_labels.contains(&"Start".into()));
                assert!(labels.node_labels.contains(&"End".into()));
            }
        }
    }

    #[test]
    fn unlabeled_elements_require_unrestricted_labels() {
        let labels = LabelExtractor::extract_labels(
            "MATCH (a)-[r]->(b:End) RETURN b",
            &QueryLanguage::Cypher,
        )
        .unwrap();
        assert!(labels.all_node_labels);
        assert!(labels.all_relation_labels);
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
