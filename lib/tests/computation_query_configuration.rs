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

#[path = "support/mod.rs"]
mod fixture_support;
#[path = "fixtures/query_semantics.rs"]
mod query_semantics;

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
    channels::{SourceEvent, SourceEventWrapper},
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
fn config() -> drasi_lib::config::AssignedQueryConfig {
    drasi_lib::Query::cypher(
        drasi_core_types::ComponentName::try_new("joined").expect("fixture component name"),
    )
    .query("MATCH (o:Order)-[:PLACED_BY]->(c:Customer) RETURN o.name AS item, c.name AS customer")
    .from_input_with_pipeline_by_id(fixture_support::component_id(1), vec!["normalize".into()])
    .from_input_by_id(fixture_support::component_id(2))
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
    .expect("valid query configuration")
    .with_assigned_id(fixture_support::component_id(752))
}
fn definition(config: &drasi_lib::config::AssignedQueryConfig) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "configured".into(),
        id: config.id.clone(),
        query: config.query.clone(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("joined/out").expect("stream"),
        outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
    }
}
fn input(
    id: ComponentId,
    source: &str,
    label: &str,
    properties: serde_json::Value,
) -> InputEnvelope {
    change_input(
        id,
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

fn change_input(
    id: ComponentId,
    source: &str,
    change: SourceChange,
    sequence: u64,
) -> InputEnvelope {
    InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope: GraphChangeCodec::encode_source_event(
            Arc::new(SourceEventWrapper {
                source_id: id.clone(),
                event: SourceEvent::Change(change),
                timestamp: chrono::DateTime::from_timestamp_millis(1000).expect("fixture time"),
                sequence,
                source_position: None,
                profiling: None,
            }),
            &id,
            StreamId::try_new(format!("{source}/out")).expect("stream"),
            sequence,
            None,
        )
        .expect("input"),
    }
}

#[tokio::test]
async fn disconnected_native_context_keeps_inventory_and_tracks_context_lifecycle() {
    query_semantics::reference(
        query_semantics::disconnected(ComputationQueryLanguage::Cypher).expect("case"),
    )
    .await
    .expect("reference semantics");
}

#[tokio::test]
async fn disconnected_gql_context_keeps_inventory_and_tracks_context_lifecycle() {
    query_semantics::reference(
        query_semantics::disconnected(ComputationQueryLanguage::Gql).expect("case"),
    )
    .await
    .expect("reference semantics");
}

#[tokio::test]
async fn optional_list_comprehension_preserves_unknown_and_empty_contexts() {
    query_semantics::reference(query_semantics::optional_list().expect("case"))
        .await
        .expect("reference semantics");
}

#[tokio::test]
async fn native_assignment_list_matches_equal_property_maps() {
    query_semantics::reference(query_semantics::assignment_maps().expect("case"))
        .await
        .expect("reference semantics");
}

#[tokio::test]
async fn native_aggregate_count_equals_integer_list_size() {
    query_semantics::reference(query_semantics::numeric_count().expect("case"))
        .await
        .expect("reference semantics");
}

#[tokio::test]
async fn optional_native_join_replaces_the_unmatched_snapshot_row() {
    query_semantics::reference(query_semantics::optional_join().expect("case"))
        .await
        .expect("reference semantics");
}

#[tokio::test]
async fn native_query_executes_synthetic_joins_and_the_configured_source_middleware_pipeline() {
    let config = config();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(PrefixFactory(calls.clone())));
    let settings =
        QueryExecutionSettings::from_configuration(&config).expect("fixture execution settings");
    let subscriptions =
        QueryExecutionSettings::subscriptions(&config).expect("same subscription planning");
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
    query
        .bind_inputs(&[
            InputBinding {
                input_port: PortId::try_new("in").expect("port"),
                output: OutputEndpoint::new(
                    fixture_support::component_id(1),
                    PortId::try_new("out").expect("port"),
                ),
                stream: StreamId::try_new("orders/out").expect("stream"),
            },
            InputBinding {
                input_port: PortId::try_new("in").expect("port"),
                output: OutputEndpoint::new(
                    fixture_support::component_id(2),
                    PortId::try_new("out").expect("port"),
                ),
                stream: StreamId::try_new("customers/out").expect("stream"),
            },
        ])
        .await
        .expect("host-owned input bindings");
    query.start().await.expect("start");
    assert!(query
        .transform(input(
            fixture_support::component_id(2),
            "customers",
            "Customer",
            serde_json::json!({"id":"C1","name":"Ada"})
        ))
        .await
        .expect("customer")
        .is_empty());
    let output = query
        .transform(input(
            fixture_support::component_id(1),
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
        query: &drasi_core_types::ComponentId,
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
        QueryExecutionSettings::from_configuration(&config).expect("fixture execution settings"),
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
            QueryExecutionSettings::from_configuration(&config)
                .expect("fixture execution settings"),
            Some(registry.clone()),
        )
        .await
        .expect("construct");
        query.start().await.expect("start");
        query.stop().await.expect("stop");
    }
    let mut changed =
        QueryExecutionSettings::from_configuration(&config).expect("fixture execution settings");
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
