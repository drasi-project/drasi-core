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

use std::collections::HashMap;

use anyhow::{bail, Result};

use crate::config::{QueryConfig, SourceSubscriptionConfig, SourceSubscriptionSettings};
use crate::queries::QueryLabels;

/// Builder for creating SourceSubscriptionSettings from QueryConfig
pub struct SubscriptionSettingsBuilder;

impl SubscriptionSettingsBuilder {
    /// Build subscription settings for each source based on query config and extracted labels
    pub fn build_subscription_settings(
        query_config: &QueryConfig,
        query_labels: &QueryLabels,
    ) -> Result<Vec<SourceSubscriptionSettings>> {
        // Create a Vec of SourceSubscriptionSettings, one for each unique SourceSubscriptionConfig
        let mut settings_vec: Vec<SourceSubscriptionSettings> = query_config
            .sources
            .iter()
            .map(|source_config| SourceSubscriptionSettings {
                source_id: source_config.source_id.clone(),
                enable_bootstrap: query_config.enable_bootstrap,
                query_id: query_config.id.clone(),
                query_text: query_config.query.clone(),
                nodes: source_config.nodes.iter().cloned().collect(),
                relations: source_config.relations.iter().cloned().collect(),
            })
            .collect();

        // Allocate node labels
        Self::allocate_node_labels(&mut settings_vec, &query_config.sources, query_labels)?;

        // Allocate relation labels
        Self::allocate_relation_labels(
            &mut settings_vec,
            &query_config.sources,
            query_labels,
            &query_config.joins,
        )?;

        Ok(settings_vec)
    }

    /// Allocate node labels to the correct source subscription settings
    fn allocate_node_labels(
        settings_vec: &mut [SourceSubscriptionSettings],
        source_configs: &[SourceSubscriptionConfig],
        query_labels: &QueryLabels,
    ) -> Result<()> {
        for node_label in &query_labels.node_labels {
            // Count how many sources have this node label in their config
            let mut matching_indices = Vec::new();
            for (idx, config) in source_configs.iter().enumerate() {
                if config.nodes.contains(node_label) {
                    matching_indices.push(idx);
                }
            }

            match matching_indices.len() {
                0 => {
                    // Not found in any source config - add to first source (default)
                    if let Some(first_settings) = settings_vec.first_mut() {
                        first_settings.nodes.insert(node_label.clone());
                    } else {
                        bail!("No sources configured for query");
                    }
                }
                1 => {
                    // Found in exactly one source - already in the HashSet from initialization
                    // Nothing to do, it's already there
                }
                _ => {
                    // Found in multiple sources - error
                    bail!(
                        "Node label '{}' is configured in multiple sources. Each node label must be assigned to exactly one source.",
                        node_label
                    );
                }
            }
        }

        Ok(())
    }

    /// Allocate relation labels to the correct source subscription settings
    fn allocate_relation_labels(
        settings_vec: &mut [SourceSubscriptionSettings],
        source_configs: &[SourceSubscriptionConfig],
        query_labels: &QueryLabels,
        joins: &Option<Vec<crate::config::QueryJoinConfig>>,
    ) -> Result<()> {
        for relation_label in &query_labels.relation_labels {
            // Count how many sources have this relation label in their config
            let mut matching_indices = Vec::new();
            for (idx, config) in source_configs.iter().enumerate() {
                if config.relations.contains(relation_label) {
                    matching_indices.push(idx);
                }
            }

            match matching_indices.len() {
                0 => {
                    // Not found in any source config
                    // Check if this is a join relation
                    if let Some(join_configs) = joins {
                        if let Some(join_config) =
                            join_configs.iter().find(|j| j.id == *relation_label)
                        {
                            // This is a join relation - verify that all node labels in the join keys
                            // match node labels from the query
                            for key in &join_config.keys {
                                if !query_labels.node_labels.contains(&key.label) {
                                    bail!(
                                        "Join relation '{}' references node label '{}' which is not found in the query",
                                        relation_label,
                                        key.label
                                    );
                                }
                            }
                            // Join relation is valid, don't add to any source
                            continue;
                        }
                    }

                    // Not a join relation - add to first source (default)
                    if let Some(first_settings) = settings_vec.first_mut() {
                        first_settings.relations.insert(relation_label.clone());
                    } else {
                        bail!("No sources configured for query");
                    }
                }
                1 => {
                    // Found in exactly one source - already in the HashSet from initialization
                    // Nothing to do, it's already there
                }
                _ => {
                    // Found in multiple sources - error
                    bail!(
                        "Relation label '{}' is configured in multiple sources. Each relation label must be assigned to exactly one source.",
                        relation_label
                    );
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        QueryConfig, QueryJoinConfig, QueryJoinKeyConfig, QueryLanguage, SourceSubscriptionConfig,
    };

    fn create_test_query_config(sources: Vec<SourceSubscriptionConfig>) -> QueryConfig {
        QueryConfig {
            id: "test-query".to_string(),
            query: "MATCH (n:Person) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources,
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        }
    }

    #[test]
    fn test_node_label_in_one_source() {
        let sources = vec![SourceSubscriptionConfig {
            source_id: "source1".to_string(),
            nodes: vec!["Person".to_string()],
            relations: vec![],
            pipeline: vec![],
        }];

        let query_config = create_test_query_config(sources);
        let query_labels = QueryLabels {
            node_labels: vec!["Person".to_string()],
            relation_labels: vec![],
        };

        let result =
            SubscriptionSettingsBuilder::build_subscription_settings(&query_config, &query_labels);
        assert!(result.is_ok());

        let settings = result.unwrap();
        assert_eq!(settings.len(), 1);
        assert!(settings[0].nodes.contains("Person"));
    }

    #[test]
    fn test_node_label_not_in_any_source_goes_to_first() {
        let sources = vec![
            SourceSubscriptionConfig {
                source_id: "source1".to_string(),
                nodes: vec![],
                relations: vec![],
                pipeline: vec![],
            },
            SourceSubscriptionConfig {
                source_id: "source2".to_string(),
                nodes: vec![],
                relations: vec![],
                pipeline: vec![],
            },
        ];

        let query_config = create_test_query_config(sources);
        let query_labels = QueryLabels {
            node_labels: vec!["Person".to_string()],
            relation_labels: vec![],
        };

        let result =
            SubscriptionSettingsBuilder::build_subscription_settings(&query_config, &query_labels);
        assert!(result.is_ok());

        let settings = result.unwrap();
        assert_eq!(settings.len(), 2);
        assert!(settings[0].nodes.contains("Person"));
        assert!(!settings[1].nodes.contains("Person"));
    }

    #[test]
    fn test_node_label_in_multiple_sources_error() {
        let sources = vec![
            SourceSubscriptionConfig {
                source_id: "source1".to_string(),
                nodes: vec!["Person".to_string()],
                relations: vec![],
                pipeline: vec![],
            },
            SourceSubscriptionConfig {
                source_id: "source2".to_string(),
                nodes: vec!["Person".to_string()],
                relations: vec![],
                pipeline: vec![],
            },
        ];

        let query_config = create_test_query_config(sources);
        let query_labels = QueryLabels {
            node_labels: vec!["Person".to_string()],
            relation_labels: vec![],
        };

        let result =
            SubscriptionSettingsBuilder::build_subscription_settings(&query_config, &query_labels);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("multiple sources"));
    }

    #[test]
    fn test_relation_label_in_one_source() {
        let sources = vec![SourceSubscriptionConfig {
            source_id: "source1".to_string(),
            nodes: vec![],
            relations: vec!["KNOWS".to_string()],
            pipeline: vec![],
        }];

        let query_config = create_test_query_config(sources);
        let query_labels = QueryLabels {
            node_labels: vec![],
            relation_labels: vec!["KNOWS".to_string()],
        };

        let result =
            SubscriptionSettingsBuilder::build_subscription_settings(&query_config, &query_labels);
        assert!(result.is_ok());

        let settings = result.unwrap();
        assert_eq!(settings.len(), 1);
        assert!(settings[0].relations.contains("KNOWS"));
    }

    #[test]
    fn test_relation_label_not_in_any_source_goes_to_first() {
        let sources = vec![SourceSubscriptionConfig {
            source_id: "source1".to_string(),
            nodes: vec![],
            relations: vec![],
            pipeline: vec![],
        }];

        let query_config = create_test_query_config(sources);
        let query_labels = QueryLabels {
            node_labels: vec![],
            relation_labels: vec!["KNOWS".to_string()],
        };

        let result =
            SubscriptionSettingsBuilder::build_subscription_settings(&query_config, &query_labels);
        assert!(result.is_ok());

        let settings = result.unwrap();
        assert_eq!(settings.len(), 1);
        assert!(settings[0].relations.contains("KNOWS"));
    }

    #[test]
    fn test_relation_label_in_multiple_sources_error() {
        let sources = vec![
            SourceSubscriptionConfig {
                source_id: "source1".to_string(),
                nodes: vec![],
                relations: vec!["KNOWS".to_string()],
                pipeline: vec![],
            },
            SourceSubscriptionConfig {
                source_id: "source2".to_string(),
                nodes: vec![],
                relations: vec!["KNOWS".to_string()],
                pipeline: vec![],
            },
        ];

        let query_config = create_test_query_config(sources);
        let query_labels = QueryLabels {
            node_labels: vec![],
            relation_labels: vec!["KNOWS".to_string()],
        };

        let result =
            SubscriptionSettingsBuilder::build_subscription_settings(&query_config, &query_labels);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("multiple sources"));
    }

    #[test]
    fn test_join_relation_not_added_to_source() {
        let sources = vec![SourceSubscriptionConfig {
            source_id: "source1".to_string(),
            nodes: vec![],
            relations: vec![],
            pipeline: vec![],
        }];

        let mut query_config = create_test_query_config(sources);
        query_config.joins = Some(vec![QueryJoinConfig {
            id: "CUSTOMER".to_string(),
            keys: vec![
                QueryJoinKeyConfig {
                    label: "Order".to_string(),
                    property: "customer_id".to_string(),
                },
                QueryJoinKeyConfig {
                    label: "Customer".to_string(),
                    property: "id".to_string(),
                },
            ],
        }]);

        let query_labels = QueryLabels {
            node_labels: vec!["Order".to_string(), "Customer".to_string()],
            relation_labels: vec!["CUSTOMER".to_string()],
        };

        let result =
            SubscriptionSettingsBuilder::build_subscription_settings(&query_config, &query_labels);
        assert!(result.is_ok());

        let settings = result.unwrap();
        assert_eq!(settings.len(), 1);
        // CUSTOMER should not be in relations since it's a join
        assert!(!settings[0].relations.contains("CUSTOMER"));
        // But Order and Customer should be in nodes
        assert!(settings[0].nodes.contains("Order"));
        assert!(settings[0].nodes.contains("Customer"));
    }

    #[test]
    fn test_join_relation_with_missing_node_label_error() {
        let sources = vec![SourceSubscriptionConfig {
            source_id: "source1".to_string(),
            nodes: vec![],
            relations: vec![],
            pipeline: vec![],
        }];

        let mut query_config = create_test_query_config(sources);
        query_config.joins = Some(vec![QueryJoinConfig {
            id: "CUSTOMER".to_string(),
            keys: vec![
                QueryJoinKeyConfig {
                    label: "Order".to_string(),
                    property: "customer_id".to_string(),
                },
                QueryJoinKeyConfig {
                    label: "Customer".to_string(),
                    property: "id".to_string(),
                },
            ],
        }]);

        let query_labels = QueryLabels {
            node_labels: vec!["Order".to_string()], // Customer is missing
            relation_labels: vec!["CUSTOMER".to_string()],
        };

        let result =
            SubscriptionSettingsBuilder::build_subscription_settings(&query_config, &query_labels);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not found in the query"));
    }

    #[test]
    fn test_complex_multi_source_scenario() {
        let sources = vec![
            SourceSubscriptionConfig {
                source_id: "orders_db".to_string(),
                nodes: vec!["Order".to_string()],
                relations: vec![],
                pipeline: vec![],
            },
            SourceSubscriptionConfig {
                source_id: "customers_db".to_string(),
                nodes: vec!["Customer".to_string()],
                relations: vec![],
                pipeline: vec![],
            },
        ];

        let mut query_config = create_test_query_config(sources);
        query_config.joins = Some(vec![QueryJoinConfig {
            id: "PLACED_BY".to_string(),
            keys: vec![
                QueryJoinKeyConfig {
                    label: "Order".to_string(),
                    property: "customer_id".to_string(),
                },
                QueryJoinKeyConfig {
                    label: "Customer".to_string(),
                    property: "id".to_string(),
                },
            ],
        }]);

        let query_labels = QueryLabels {
            node_labels: vec!["Order".to_string(), "Customer".to_string(), "Product".to_string()],
            relation_labels: vec!["PLACED_BY".to_string(), "CONTAINS".to_string()],
        };

        let result =
            SubscriptionSettingsBuilder::build_subscription_settings(&query_config, &query_labels);
        assert!(result.is_ok());

        let settings = result.unwrap();
        assert_eq!(settings.len(), 2);

        // Order should be in first source
        assert!(settings[0].nodes.contains("Order"));
        // Customer should be in second source
        assert!(settings[1].nodes.contains("Customer"));
        // Product not in any source config, should go to first
        assert!(settings[0].nodes.contains("Product"));

        // PLACED_BY is a join, should not be in any relations
        assert!(!settings[0].relations.contains("PLACED_BY"));
        assert!(!settings[1].relations.contains("PLACED_BY"));

        // CONTAINS is not in any config and not a join, should go to first
        assert!(settings[0].relations.contains("CONTAINS"));
    }
}
