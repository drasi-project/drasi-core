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

#![cfg(test)]

use async_trait::async_trait;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationIndexes, InMemoryComputationProvider},
    interface::{
        ElementIndex, IndexError, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        SourceMiddlewareConfig,
    },
};
use drasi_lib::{
    computation::v1::*,
    config::{QueryJoinConfig, QueryJoinKeyConfig},
};
use std::{
    num::NonZeroUsize,
    result::Result,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

struct PrefixFactory(Arc<AtomicUsize>);
struct Prefix {
    value: String,
    calls: Arc<AtomicUsize>,
}
impl SourceMiddlewareFactory for PrefixFactory {
    fn name(&self) -> String {
        "prefix".into()
    }
    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        Ok(Arc::new(Prefix {
            value: config.config["value"]
                .as_str()
                .ok_or_else(|| MiddlewareSetupError::InvalidConfiguration("missing prefix".into()))?
                .into(),
            calls: self.0.clone(),
        }))
    }
}
#[async_trait]
impl SourceMiddleware for Prefix {
    async fn process(
        &self,
        change: SourceChange,
        _: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let SourceChange::Insert {
            element:
                Element::Node {
                    metadata,
                    mut properties,
                },
        } = change
        else {
            return Err(MiddlewareError::SourceChangeError(
                "test accepts inserts".into(),
            ));
        };
        if let drasi_core::models::ElementValue::String(name) = &properties["name"] {
            properties.insert(
                "name",
                drasi_core::models::ElementValue::String(format!("{}{name}", self.value).into()),
            );
        }
        Ok(vec![SourceChange::Insert {
            element: Element::Node {
                metadata,
                properties,
            },
        }])
    }
}
fn config() -> drasi_lib::config::QueryConfig {
    drasi_lib::Query::cypher("joined")
        .query(
            "MATCH (o:Order)-[:PLACED_BY]->(c:Customer) RETURN o.name AS item, c.name AS customer",
        )
        .from_source_with_pipeline("orders", vec!["normalize".into()])
        .from_source("customers")
        .with_middleware(SourceMiddlewareConfig::new(
            "prefix",
            "normalize",
            serde_json::json!({"value":"normalized:"})
                .as_object()
                .expect("object")
                .clone(),
        ))
        .with_joins(vec![QueryJoinConfig {
            id: "PLACED_BY".into(),
            keys: vec![
                QueryJoinKeyConfig {
                    label: "Order".into(),
                    property: "customerId".into(),
                },
                QueryJoinKeyConfig {
                    label: "Customer".into(),
                    property: "id".into(),
                },
            ],
        }])
        .build()
}
fn definition(config: &drasi_lib::config::QueryConfig) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "configured".into(),
        id: ComponentId::try_new(config.id.as_str()).expect("id"),
        query: config.query.clone(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("joined/out").expect("stream"),
        outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
    }
}
fn input(source: &str, label: &str, properties: serde_json::Value) -> InputEnvelope {
    change_input(
        source,
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(source, source),
                    labels: Arc::from([Arc::from(label)]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(properties),
            },
        },
        1,
    )
}

fn change_input(source: &str, change: SourceChange, sequence: u64) -> InputEnvelope {
    InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope: GraphChangeCodec::encode_change(
            change,
            StreamId::try_new(format!("{source}/out")).expect("stream"),
            sequence,
            None,
        )
        .expect("input"),
    }
}

async fn disconnected_native_context_lifecycle(language: ComputationQueryLanguage) {
    let config = drasi_lib::Query::cypher("inventory-context")
        .query("MATCH (w:workload_requirements) OPTIONAL MATCH (a:AppliedPlan) RETURN w.workload_id AS workload_id, a.fleet_id AS fleet_id")
        .build();
    let mut query_definition = definition(&config);
    query_definition.language = language;
    let mut query = ContinuousQueryTransformer::new_configured(
        query_definition,
        Arc::new(InMemoryComputationProvider),
        QueryOptions::default(),
        QueryExecutionSettings::default(),
        None,
    )
    .await
    .expect("query");
    query.start().await.expect("start");
    let plan_update = SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("plan", "plan"),
                labels: Arc::from([Arc::from("AppliedPlan")]),
                effective_from: 2000,
            },
            properties: ElementPropertyMap::from(serde_json::json!({"fleet_id":"updated"})),
        },
    };
    let delete = |source| SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, source),
            labels: Arc::from([]),
            effective_from: 3000,
        },
    };
    for (change, expected) in [
        (
            input(
                "workload",
                "workload_requirements",
                serde_json::json!({"workload_id":"workload"}),
            ),
            vec![serde_json::json!({"workload_id":"workload","fleet_id":null})],
        ),
        (
            input(
                "plan",
                "AppliedPlan",
                serde_json::json!({"fleet_id":"demo"}),
            ),
            vec![serde_json::json!({"workload_id":"workload","fleet_id":"demo"})],
        ),
        (
            change_input("plan", plan_update, 2),
            vec![serde_json::json!({"workload_id":"workload","fleet_id":"updated"})],
        ),
        (
            change_input("plan", delete("plan"), 3),
            vec![serde_json::json!({"workload_id":"workload","fleet_id":null})],
        ),
        (change_input("workload", delete("workload"), 2), Vec::new()),
    ] {
        let output = query.transform(change).await.expect("change");
        query.delivery_completed(&output).await.expect("delivery");
        let rows: Vec<_> = query
            .results()
            .snapshot()
            .expect("snapshot")
            .rows
            .values()
            .map(|row| {
                serde_json::to_value(QueryChangeCodec::decode_row(row).expect("row").values)
                    .expect("json")
            })
            .collect();
        assert_eq!(rows, expected);
    }
    query.stop().await.expect("stop");
}

#[tokio::test]
async fn disconnected_native_context_keeps_inventory_and_tracks_context_lifecycle() {
    disconnected_native_context_lifecycle(ComputationQueryLanguage::Cypher).await;
}

#[tokio::test]
async fn disconnected_gql_context_keeps_inventory_and_tracks_context_lifecycle() {
    disconnected_native_context_lifecycle(ComputationQueryLanguage::Gql).await;
}

#[tokio::test]
async fn optional_list_comprehension_preserves_unknown_and_empty_contexts() {
    let config = drasi_lib::Query::cypher("optional-list")
        .query("MATCH (w:Workload) OPTIONAL MATCH (p:Plan) RETURN w.id AS id, [d IN p.assignments WHERE d = 'gpu'] AS matches, size([d IN p.assignments WHERE d = 'gpu']) AS matched")
        .build();
    let mut query = ContinuousQueryTransformer::new_configured(
        definition(&config),
        Arc::new(InMemoryComputationProvider),
        QueryOptions::default(),
        QueryExecutionSettings::default(),
        None,
    )
    .await
    .expect("query");
    query.start().await.expect("start");
    let metadata = ElementMetadata {
        reference: ElementReference::new("plan", "plan"),
        labels: Arc::from([Arc::from("Plan")]),
        effective_from: 2000,
    };
    for (change, expected) in [
        (
            input("workload", "Workload", serde_json::json!({"id":"workload"})),
            serde_json::json!({"id":"workload","matches":null,"matched":null}),
        ),
        (
            input(
                "plan",
                "Plan",
                serde_json::json!({"assignments":["gpu","other"]}),
            ),
            serde_json::json!({"id":"workload","matches":["gpu"],"matched":1}),
        ),
        (
            change_input(
                "plan",
                SourceChange::Update {
                    element: Element::Node {
                        metadata: metadata.clone(),
                        properties: ElementPropertyMap::from(serde_json::json!({"assignments":[]})),
                    },
                },
                2,
            ),
            serde_json::json!({"id":"workload","matches":[],"matched":0}),
        ),
        (
            change_input("plan", SourceChange::Delete { metadata }, 3),
            serde_json::json!({"id":"workload","matches":null,"matched":null}),
        ),
    ] {
        let output = query.transform(change).await.expect("optional list update");
        query.delivery_completed(&output).await.expect("delivery");
        let rows: Vec<_> = query
            .results()
            .snapshot()
            .expect("snapshot")
            .rows
            .values()
            .map(|row| {
                serde_json::to_value(QueryChangeCodec::decode_row(row).expect("row").values)
                    .expect("json")
            })
            .collect();
        assert_eq!(rows, vec![expected]);
    }
    query.stop().await.expect("stop");
}

#[tokio::test]
async fn native_assignment_list_matches_equal_property_maps() {
    let config = drasi_lib::Query::cypher("assignment-maps")
        .query("MATCH (w:workload_requirements) OPTIONAL MATCH (p:gpu_placements) RETURN w.workload_id AS workload_id, [d IN p.assignments WHERE d = w.assignment] AS matching")
        .build();
    let mut query = ContinuousQueryTransformer::new_configured(
        definition(&config),
        Arc::new(InMemoryComputationProvider),
        QueryOptions::default(),
        QueryExecutionSettings::default(),
        None,
    )
    .await
    .expect("query");
    query.start().await.expect("start");
    let assignment =
        serde_json::json!({"gpu_id":"gpu-1","workload_revision":"1","memory_mib":4096});
    for (change, expected) in [
        (
            input(
                "workload",
                "workload_requirements",
                serde_json::json!({"workload_id":"workload","assignment":assignment.clone()}),
            ),
            serde_json::json!({"workload_id":"workload","matching":null}),
        ),
        (
            input(
                "plan",
                "gpu_placements",
                serde_json::json!({"assignments":[assignment.clone()]}),
            ),
            serde_json::json!({"workload_id":"workload","matching":[assignment]}),
        ),
    ] {
        let output = query.transform(change).await.expect("assignment input");
        query.delivery_completed(&output).await.expect("delivery");
        let rows: Vec<_> = query
            .results()
            .snapshot()
            .expect("snapshot")
            .rows
            .values()
            .map(|row| {
                serde_json::to_value(QueryChangeCodec::decode_row(row).expect("row").values)
                    .expect("json")
            })
            .collect();
        assert_eq!(rows, vec![expected]);
    }
    query.stop().await.expect("stop");
}

#[tokio::test]
async fn native_aggregate_count_equals_integer_list_size() {
    let config = drasi_lib::Query::cypher("numeric-count")
        .query("MATCH (w:workload_requirements) WITH sum(w.replicas) AS count RETURN count, size([1,2]) AS expected, count = size([1,2]) AS equal, count <> size([1,2]) AS unequal")
        .build();
    let mut query = ContinuousQueryTransformer::new_configured(
        definition(&config),
        Arc::new(InMemoryComputationProvider),
        QueryOptions::default(),
        QueryExecutionSettings::default(),
        None,
    )
    .await
    .expect("query");
    query.start().await.expect("start");
    for (replicas, equal, sequence) in [(2, true, 1), (3, false, 2)] {
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("workload", "workload"),
                labels: Arc::from([Arc::from("workload_requirements")]),
                effective_from: sequence * 1000,
            },
            properties: ElementPropertyMap::from(serde_json::json!({"replicas":replicas})),
        };
        let change = if sequence == 1 {
            SourceChange::Insert { element }
        } else {
            SourceChange::Update { element }
        };
        let output = query
            .transform(change_input("workload", change, sequence))
            .await
            .expect("count input");
        query.delivery_completed(&output).await.expect("delivery");
        let rows: Vec<_> = query
            .results()
            .snapshot()
            .expect("snapshot")
            .rows
            .values()
            .map(|row| {
                serde_json::to_value(QueryChangeCodec::decode_row(row).expect("row").values)
                    .expect("json")
            })
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["count"].as_f64(), Some(f64::from(replicas)));
        assert_eq!(rows[0]["expected"], 2);
        assert_eq!(rows[0]["equal"], equal);
        assert_eq!(rows[0]["unequal"], !equal);
    }
    query.stop().await.expect("stop");
}

#[tokio::test]
async fn optional_native_join_replaces_the_unmatched_snapshot_row() {
    let config = drasi_lib::Query::cypher("decision")
        .query("MATCH (d:DecisionExplanation) OPTIONAL MATCH (d)-[:DECISION_WRITE]->(w:PlanWriteOutcome) RETURN d.decision_id AS decision_id, w.outcome AS outcome")
        .from_source("decisions").from_source("writes")
        .with_joins(vec![QueryJoinConfig {
            id: "DECISION_WRITE".into(),
            keys: ["DecisionExplanation", "PlanWriteOutcome"].into_iter()
                .map(|label| QueryJoinKeyConfig { label: label.into(), property: "decision_id".into() }).collect(),
        }]).build();
    let mut query = ContinuousQueryTransformer::new_configured(
        definition(&config),
        Arc::new(InMemoryComputationProvider),
        QueryOptions::default(),
        QueryExecutionSettings::from_legacy_config(&config),
        None,
    )
    .await
    .expect("query");
    query.start().await.expect("start");
    for (change, expected) in [
        (
            input(
                "decisions",
                "DecisionExplanation",
                serde_json::json!({"decision_id":"decision"}),
            ),
            serde_json::json!({"decision_id":"decision","outcome":null}),
        ),
        (
            input(
                "writes",
                "PlanWriteOutcome",
                serde_json::json!({"decision_id":"decision","outcome":"rejected"}),
            ),
            serde_json::json!({"decision_id":"decision","outcome":"rejected"}),
        ),
    ] {
        let output = query.transform(change).await.expect("change");
        query.delivery_completed(&output).await.expect("delivery");
        let rows: Vec<_> = query
            .results()
            .snapshot()
            .expect("snapshot")
            .rows
            .values()
            .map(|row| {
                serde_json::to_value(QueryChangeCodec::decode_row(row).expect("row").values)
                    .expect("json")
            })
            .collect();
        assert_eq!(
            rows,
            vec![expected],
            "retained rows must retract the unmatched optional identity"
        );
    }
    query.stop().await.expect("stop");
}

#[tokio::test]
async fn native_query_executes_synthetic_joins_and_the_configured_source_middleware_pipeline() {
    let config = config();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(PrefixFactory(calls.clone())));
    let settings = QueryExecutionSettings::from_legacy_config(&config);
    let subscriptions =
        QueryExecutionSettings::legacy_subscriptions(&config).expect("same subscription planning");
    assert!(
        subscriptions
            .iter()
            .all(|source| !source.relations.contains("PLACED_BY")),
        "synthetic relationships are not requested from plugins"
    );
    let mut query = ContinuousQueryTransformer::new_configured(
        definition(&config),
        Arc::new(InMemoryComputationProvider),
        QueryOptions::default(),
        settings,
        Some(Arc::new(registry)),
    )
    .await
    .expect("construct");
    query.start().await.expect("start");
    assert!(query
        .transform(input(
            "customers",
            "Customer",
            serde_json::json!({"id":"C1","name":"Ada"})
        ))
        .await
        .expect("customer")
        .is_empty());
    let output = query
        .transform(input(
            "orders",
            "Order",
            serde_json::json!({"customerId":"C1","name":"order"}),
        ))
        .await
        .expect("join");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "middleware applies only to orders"
    );
    let legacy = QueryChangeCodec::to_legacy_result(&output[0].envelope).expect("result");
    assert!(
        matches!(&legacy.results[0], drasi_lib::channels::ResultDiff::Add { data, .. } if *data == serde_json::json!({"item":"normalized:order","customer":"Ada"}))
    );
    query.stop().await.expect("stop");
}

struct CountingProvider(Arc<AtomicUsize>);
#[async_trait]
impl ComputationIndexProvider for CountingProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        query: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        InMemoryComputationProvider
            .create_indexes(graph, query)
            .await
    }
    fn is_volatile(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn invalid_middleware_is_rejected_before_constructing_query_resources() {
    let config = config();
    let count = Arc::new(AtomicUsize::new(0));
    let result = ContinuousQueryTransformer::new_configured(
        definition(&config),
        Arc::new(CountingProvider(count.clone())),
        QueryOptions::default(),
        QueryExecutionSettings::from_legacy_config(&config),
        Some(Arc::new(MiddlewareTypeRegistry::new())),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "computation-rocksdb-tests")]
#[tokio::test]
async fn recovery_detects_changed_join_or_middleware_configuration() {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    let temp = tempfile::tempdir().expect("temp");
    let provider: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
        temp.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
        ),
    ));
    let config = config();
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(PrefixFactory(Arc::new(AtomicUsize::new(0)))));
    let registry = Arc::new(registry);
    {
        let mut query = ContinuousQueryTransformer::new_configured(
            definition(&config),
            provider.clone(),
            QueryOptions::default(),
            QueryExecutionSettings::from_legacy_config(&config),
            Some(registry.clone()),
        )
        .await
        .expect("construct");
        query.start().await.expect("start");
        query.stop().await.expect("stop");
    }
    let mut changed = QueryExecutionSettings::from_legacy_config(&config);
    changed.middleware[0]
        .config
        .insert("value".into(), "changed:".into());
    let mut query = ContinuousQueryTransformer::new_configured(
        definition(&config),
        provider,
        QueryOptions::default(),
        changed,
        Some(registry),
    )
    .await
    .expect("reconstruct");
    assert!(matches!(
        query
            .start()
            .await
            .expect_err("configuration changed")
            .downcast_ref::<QueryRecoveryError>(),
        Some(QueryRecoveryError::ConfigurationChanged)
    ));
    query.stop().await.expect("cleanup");
}
