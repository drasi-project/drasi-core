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

//! Semantic fingerprint checks for the single native query configuration record.
use super::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::DispatchMode;
    use crate::config::{QueryConfig, QueryJoinConfig, QueryLanguage};
    use crate::config::{QueryJoinKeyConfig, SourceSubscriptionConfig};
    use crate::recovery::RecoveryPolicy;
    use drasi_core::models::SourceMiddlewareConfig;
    use serde_json::{Map, Value};
    use std::sync::Arc;

    fn compute_config_hash(config: &QueryConfig) -> u64 {
        ContinuousQueryDefinition {
            graph_id: "graph".into(),
            id: ComponentId::try_new(config.id.as_str()).unwrap(),
            query: config.query.clone(),
            language: match config.query_language {
                QueryLanguage::Cypher => ComputationQueryLanguage::Cypher,
                QueryLanguage::GQL => ComputationQueryLanguage::Gql,
            },
            // Query identity is separate from its unchanged output binding.
            output_stream: StreamId::try_new("output").unwrap(),
            outbox_capacity: NonZeroUsize::new(config.outbox_capacity).unwrap(),
        }
        .configuration_hash(&QueryExecutionSettings::from_legacy_config(config))
        .unwrap()
    }

    fn base() -> QueryConfig {
        QueryConfig {
            id: "q1".into(),
            query: "MATCH (n) RETURN n".into(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![SourceSubscriptionConfig {
                source_id: "s1".into(),
                nodes: vec!["A".into()],
                relations: vec![],
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
            recovery_policy: None,
            outbox_capacity: 1000,
            bootstrap_timeout_secs: 300,
        }
    }

    #[test]
    fn same_config_same_hash() {
        let a = base();
        let b = base();
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_query_different_hash() {
        let a = base();
        let mut b = base();
        b.query = "MATCH (m) RETURN m".into();
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_query_language_different_hash() {
        let a = base();
        let mut b = base();
        b.query_language = QueryLanguage::GQL;
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_source_id_different_hash() {
        let a = base();
        let mut b = base();
        b.sources[0].source_id = "s2".into();
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_source_labels_different_hash() {
        let a = base();
        let mut b = base();
        b.sources[0].nodes = vec!["B".into()];
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_middleware_name_different_hash() {
        let mut a = base();
        a.middleware = vec![SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("m1"),
            config: Map::new(),
        }];

        let mut b = base();
        b.middleware = vec![SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("m2"),
            config: Map::new(),
        }];
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_middleware_config_different_hash() {
        let mut a = base();
        a.middleware = vec![SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("m1"),
            config: Map::new(),
        }];

        let mut b = base();
        let mut cfg = Map::new();
        cfg.insert("k".into(), Value::String("v".into()));
        b.middleware = vec![SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("m1"),
            config: cfg,
        }];
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn different_join_property_different_hash() {
        let mut a = base();
        a.joins = Some(vec![QueryJoinConfig {
            id: "J1".into(),
            keys: vec![QueryJoinKeyConfig {
                label: "A".into(),
                property: "x".into(),
            }],
        }]);

        let mut b = base();
        b.joins = Some(vec![QueryJoinConfig {
            id: "J1".into(),
            keys: vec![QueryJoinKeyConfig {
                label: "A".into(),
                property: "y".into(),
            }],
        }]);
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    // ----------------------------------------------------------------
    // Operational tuning fields — MUST NOT affect the hash.
    // ----------------------------------------------------------------

    #[test]
    fn id_change_same_hash() {
        let a = base();
        let mut b = base();
        b.id = "q-other".into();
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn auto_start_change_same_hash() {
        let a = base();
        let mut b = base();
        b.auto_start = !b.auto_start;
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn enable_bootstrap_change_same_hash() {
        let a = base();
        let mut b = base();
        b.enable_bootstrap = !b.enable_bootstrap;
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn priority_queue_capacity_change_same_hash() {
        let a = base();
        let mut b = base();
        b.priority_queue_capacity = Some(123456);
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn dispatch_buffer_capacity_change_same_hash() {
        let a = base();
        let mut b = base();
        b.dispatch_buffer_capacity = Some(42);
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn dispatch_mode_change_same_hash() {
        let a = base();
        let mut b = base();
        b.dispatch_mode = Some(DispatchMode::Broadcast);
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn recovery_policy_change_same_hash() {
        let a = base();
        let mut b = base();
        b.recovery_policy = Some(RecoveryPolicy::AutoReset);
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    // ----------------------------------------------------------------
    // Ordering invariance.
    // ----------------------------------------------------------------

    #[test]
    fn source_reorder_changes_hash() {
        let mut a = base();
        a.sources = vec![
            SourceSubscriptionConfig {
                source_id: "s1".into(),
                nodes: vec!["A".into()],
                relations: vec![],
                pipeline: vec![],
            },
            SourceSubscriptionConfig {
                source_id: "s2".into(),
                nodes: vec!["B".into()],
                relations: vec![],
                pipeline: vec![],
            },
        ];

        let mut b = base();
        b.sources = vec![
            SourceSubscriptionConfig {
                source_id: "s2".into(),
                nodes: vec!["B".into()],
                relations: vec![],
                pipeline: vec![],
            },
            SourceSubscriptionConfig {
                source_id: "s1".into(),
                nodes: vec!["A".into()],
                relations: vec![],
                pipeline: vec![],
            },
        ];

        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn joins_reorder_same_hash() {
        let mut a = base();
        a.joins = Some(vec![
            QueryJoinConfig {
                id: "JA".into(),
                keys: vec![],
            },
            QueryJoinConfig {
                id: "JB".into(),
                keys: vec![],
            },
        ]);

        let mut b = base();
        b.joins = Some(vec![
            QueryJoinConfig {
                id: "JB".into(),
                keys: vec![],
            },
            QueryJoinConfig {
                id: "JA".into(),
                keys: vec![],
            },
        ]);

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn nodes_reorder_same_hash() {
        let mut a = base();
        a.sources[0].nodes = vec!["Order".into(), "Customer".into()];

        let mut b = base();
        b.sources[0].nodes = vec!["Customer".into(), "Order".into()];

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn relations_reorder_same_hash() {
        let mut a = base();
        a.sources[0].relations = vec!["PLACED_BY".into(), "CONTAINS".into()];

        let mut b = base();
        b.sources[0].relations = vec!["CONTAINS".into(), "PLACED_BY".into()];

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn duplicate_nodes_same_hash_as_deduped() {
        let mut a = base();
        a.sources[0].nodes = vec!["Order".into(), "Order".into(), "Customer".into()];

        let mut b = base();
        b.sources[0].nodes = vec!["Order".into(), "Customer".into()];

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn duplicate_relations_same_hash_as_deduped() {
        let mut a = base();
        a.sources[0].relations = vec!["R".into(), "R".into()];

        let mut b = base();
        b.sources[0].relations = vec!["R".into()];

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn join_keys_reorder_same_hash() {
        let mut a = base();
        a.joins = Some(vec![QueryJoinConfig {
            id: "J1".into(),
            keys: vec![
                QueryJoinKeyConfig {
                    label: "A".into(),
                    property: "x".into(),
                },
                QueryJoinKeyConfig {
                    label: "B".into(),
                    property: "y".into(),
                },
            ],
        }]);

        let mut b = base();
        b.joins = Some(vec![QueryJoinConfig {
            id: "J1".into(),
            keys: vec![
                QueryJoinKeyConfig {
                    label: "B".into(),
                    property: "y".into(),
                },
                QueryJoinKeyConfig {
                    label: "A".into(),
                    property: "x".into(),
                },
            ],
        }]);

        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn pipeline_reorder_different_hash() {
        let mut a = base();
        a.sources[0].pipeline = vec!["decode".into(), "map".into()];

        let mut b = base();
        b.sources[0].pipeline = vec!["map".into(), "decode".into()];

        // Pipeline order within a source is semantically meaningful.
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn middleware_reorder_different_hash() {
        let m1 = SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("first"),
            config: Map::new(),
        };
        let m2 = SourceMiddlewareConfig {
            kind: Arc::from("map"),
            name: Arc::from("second"),
            config: Map::new(),
        };

        let mut a = base();
        a.middleware = vec![m1.clone(), m2.clone()];

        let mut b = base();
        b.middleware = vec![m2, m1];

        // Pipeline order is semantically meaningful — swapping changes the hash.
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }
}
