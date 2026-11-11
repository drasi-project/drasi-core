// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Build the native libraries and run with `bash lib/tests/run-query-plugin.sh smoke`.

#[path = "native_query_plugin/bootstrap.rs"]
mod bootstrap;
#[path = "native_query_plugin/durable.rs"]
mod durable;
#[path = "native_query_plugin/semantics.rs"]
mod semantics;

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_host_sdk::computation;
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::computation::v1::*;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

fn library() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    target.join("debug").join(format!(
        "{}drasi_computation_transformer_query{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

fn item(value: i64, effective_from: u64) -> Element {
    item_with_id("one", value, effective_from)
}

fn item_with_id(id: &str, value: i64, effective_from: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: Arc::from([Arc::from("Item")]),
            effective_from,
        },
        properties: ElementPropertyMap::from(json!({"value": value})),
    }
}

fn input(change: SourceChange, sequence: u64) -> Result<InputEnvelope> {
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_change(
            change,
            StreamId::try_new("source/out")?,
            sequence,
            None,
        )?,
    })
}

async fn run_query() -> Result<()> {
    let path = library();
    let plugin = computation::load(&path)
        .with_context(|| format!("load query plugin {}", path.display()))?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/volatile-query"
        })
        .context("query factory missing from loaded plugin")?;
    let configuration = json!({
        "query": "MATCH (n:Item) RETURN n.value AS value",
        "volatile": true
    });
    assert!(factory
        .create_component(
            ComponentId::try_new("query")?,
            json!({"query": "MATCH (n) RETURN n", "volatile": false})
        )
        .is_err());
    let mut query = factory.create_component(ComponentId::try_new("query")?, configuration)?;
    query.start().await?;
    let changes = [
        SourceChange::Insert {
            element: item(1, 1),
        },
        SourceChange::Replace {
            element: item(2, 2),
        },
        SourceChange::Delete {
            metadata: item(2, 3).get_metadata().clone(),
        },
    ];
    for (index, change) in changes.into_iter().enumerate() {
        let output = query.transform(input(change, index as u64 + 1)?).await?;
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].envelope.system().stream().as_str(), "query/out");
        assert_eq!(output[0].envelope.system().sequence(), index as u64 + 1);
        let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
        assert_eq!(results.len(), 1);
        let value = |number| VariableValue::from(json!(number));
        match (&results[0], index) {
            (QueryPartEvaluationContext::Adding { after, .. }, 0) => {
                assert_eq!(after.get("value"), Some(&value(1)));
            }
            (QueryPartEvaluationContext::Updating { before, after, .. }, 1) => {
                assert_eq!(before.get("value"), Some(&value(1)));
                assert_eq!(after.get("value"), Some(&value(2)));
            }
            (QueryPartEvaluationContext::Removing { before, .. }, 2) => {
                assert_eq!(before.get("value"), Some(&value(2)));
            }
            (result, _) => anyhow::bail!("unexpected query result: {result:?}"),
        }
    }
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn separately_loaded_query_runs_on_current_thread() -> Result<()> {
    run_query().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_loaded_query_runs_on_multi_thread() -> Result<()> {
    run_query().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_loaded_query_supports_gql() -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query"
        })
        .context("indexed query factory missing")?;
    let directory = tempfile::tempdir()?;
    let provider = Arc::new(RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
        ),
    ));
    let scope = drasi_computation_plugin_sdk::Scope {
        instance_id: "query-test".into(),
        graph_id: "query-test".into(),
        generation: 1,
    };
    assert!(factory
        .create_component_with_processing(
            ComponentId::try_new("query")?,
            json!({"query":"MATCH (n:Item) RETURN n.value AS value", "language":"unsupported"}),
            scope.clone(),
            provider.clone(),
        )
        .is_err());
    let mut query = factory.create_component_with_processing(
        ComponentId::try_new("query")?,
        json!({"query":"MATCH (n:Item) RETURN n.value AS value", "language":"gql"}),
        scope,
        provider,
    )?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(1, 1),
            },
            1,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
    match results.as_slice() {
        [QueryPartEvaluationContext::Adding { after, .. }] => {
            assert_eq!(after.get("value"), Some(&VariableValue::from(json!(1))));
        }
        _ => anyhow::bail!("unexpected GQL query result: {results:?}"),
    }
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_loaded_query_reuses_host_indexes_after_recreation() -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query"
        })
        .context("indexed query factory missing")?;
    let directory = tempfile::tempdir()?;
    let provider = Arc::new(RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
        ),
    ));
    let scope = drasi_computation_plugin_sdk::Scope {
        instance_id: "query-test".into(),
        graph_id: "query-test".into(),
        generation: 1,
    };
    let configuration = json!({"query": "MATCH (n:Item) RETURN n.value AS value"});
    let create = || {
        factory.create_component_with_processing(
            ComponentId::try_new("query")?,
            configuration.clone(),
            scope.clone(),
            provider.clone(),
        )
    };
    let mut query = create()?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(1, 1),
            },
            1,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    query.delivery_completed(&output).await?;
    query.stop().await?;
    drop(query);

    let mut query = create()?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Replace {
                element: item(2, 2),
            },
            2,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
    match results.as_slice() {
        [QueryPartEvaluationContext::Updating { before, after, .. }] => {
            assert_eq!(before.get("value"), Some(&VariableValue::from(json!(1))));
            assert_eq!(after.get("value"), Some(&VariableValue::from(json!(2))));
        }
        _ => anyhow::bail!("unexpected query result after recreation: {results:?}"),
    }
    query.delivery_completed(&output).await?;
    let output = query
        .transform(input(
            SourceChange::Delete {
                metadata: item(2, 3).get_metadata().clone(),
            },
            3,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
    match results.as_slice() {
        [QueryPartEvaluationContext::Removing { before, .. }] => {
            assert_eq!(before.get("value"), Some(&VariableValue::from(json!(2))));
        }
        _ => anyhow::bail!("unexpected query result after deletion: {results:?}"),
    }
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_loaded_aggregate_reuses_host_result_index() -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query"
        })
        .context("indexed query factory missing")?;
    let directory = tempfile::tempdir()?;
    let provider = Arc::new(RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
        ),
    ));
    let scope = drasi_computation_plugin_sdk::Scope {
        instance_id: "query-test".into(),
        graph_id: "query-test".into(),
        generation: 1,
    };
    let create = || {
        factory.create_component_with_processing(
            ComponentId::try_new("aggregate")?,
            json!({"query": "MATCH (n:Item) RETURN sum(n.value) AS total"}),
            scope.clone(),
            provider.clone(),
        )
    };
    let mut query = create()?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(1, 1),
            },
            1,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    query.delivery_completed(&output).await?;
    query.stop().await?;
    drop(query);

    let mut query = create()?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Replace {
                element: item(2, 2),
            },
            2,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
    match results.as_slice() {
        [QueryPartEvaluationContext::Aggregation { before, after, .. }] => {
            assert_eq!(
                before.as_ref().and_then(|values| values.get("total")),
                Some(&VariableValue::from(json!(1.0)))
            );
            assert_eq!(after.get("total"), Some(&VariableValue::from(json!(2.0))));
        }
        _ => anyhow::bail!("unexpected aggregate result after recreation: {results:?}"),
    }
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_loaded_grouped_query_reuses_host_result_index() -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query"
        })
        .context("indexed query factory missing")?;
    let directory = tempfile::tempdir()?;
    let provider = Arc::new(RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
        ),
    ));
    let scope = drasi_computation_plugin_sdk::Scope {
        instance_id: "query-test".into(),
        graph_id: "query-test".into(),
        generation: 1,
    };
    let create = || {
        factory.create_component_with_processing(
            ComponentId::try_new("grouped")?,
            json!({"query": "MATCH (n:Item) RETURN n.value AS value, count(n) AS total"}),
            scope.clone(),
            provider.clone(),
        )
    };
    let mut query = create()?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(1, 1),
            },
            1,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    query.delivery_completed(&output).await?;
    query.stop().await?;
    drop(query);

    let mut query = create()?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item_with_id("two", 1, 2),
            },
            2,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
    match results.as_slice() {
        [QueryPartEvaluationContext::Aggregation { before, after, .. }] => {
            assert_eq!(
                before.as_ref().and_then(|values| values.get("total")),
                Some(&VariableValue::from(json!(1)))
            );
            assert_eq!(after.get("total"), Some(&VariableValue::from(json!(2))));
            assert_eq!(after.get("value"), Some(&VariableValue::from(json!(1))));
        }
        _ => anyhow::bail!("unexpected grouped result after recreation: {results:?}"),
    }
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_loaded_min_query_reuses_sorted_index() -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query"
        })
        .context("indexed query factory missing")?;
    let directory = tempfile::tempdir()?;
    let provider = Arc::new(RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
        ),
    ));
    let scope = drasi_computation_plugin_sdk::Scope {
        instance_id: "query-test".into(),
        graph_id: "query-test".into(),
        generation: 1,
    };
    let create = || {
        factory.create_component_with_processing(
            ComponentId::try_new("minimum")?,
            json!({"query": "MATCH (n:Item) RETURN min(n.value) AS minimum"}),
            scope.clone(),
            provider.clone(),
        )
    };
    let mut query = create()?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(1, 1),
            },
            1,
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item_with_id("two", 2, 2),
            },
            2,
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    query.stop().await?;
    drop(query);

    let mut query = create()?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Delete {
                metadata: item(1, 3).get_metadata().clone(),
            },
            3,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
    match results.as_slice() {
        [QueryPartEvaluationContext::Aggregation { before, after, .. }] => {
            assert_eq!(
                before.as_ref().and_then(|values| values.get("minimum")),
                Some(&VariableValue::from(json!(1.0)))
            );
            assert_eq!(after.get("minimum"), Some(&VariableValue::from(json!(2.0))));
        }
        _ => anyhow::bail!("unexpected minimum result after recreation: {results:?}"),
    }
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_loaded_indexed_query_matches_relationships() -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.experimental/indexed-query"
        })
        .context("indexed query factory missing")?;
    let directory = tempfile::tempdir()?;
    let provider = Arc::new(RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
        ),
    ));
    let mut query = factory.create_component_with_processing(
        ComponentId::try_new("join")?,
        json!({"query": "MATCH (a:Item)-[:LINK]->(b:Item) RETURN a.value AS left, b.value AS right"}),
        drasi_computation_plugin_sdk::Scope {
            instance_id: "query-test".into(),
            graph_id: "query-test".into(),
            generation: 1,
        },
        provider,
    )?;
    query.start().await?;
    for (sequence, element) in [
        item_with_id("one", 1, 1),
        item_with_id("two", 2, 2),
        Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", "link"),
                labels: Arc::from([Arc::from("LINK")]),
                effective_from: 3,
            },
            in_node: ElementReference::new("source", "one"),
            out_node: ElementReference::new("source", "two"),
            properties: ElementPropertyMap::new(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        let output = query
            .transform(input(
                SourceChange::Insert { element },
                sequence as u64 + 1,
            )?)
            .await?;
        if sequence < 2 {
            assert!(output.is_empty());
        } else {
            assert_eq!(output.len(), 1);
            let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
            match results.as_slice() {
                [QueryPartEvaluationContext::Adding { after, .. }] => {
                    assert_eq!(after.get("left"), Some(&VariableValue::from(json!(1))));
                    assert_eq!(after.get("right"), Some(&VariableValue::from(json!(2))));
                }
                _ => anyhow::bail!("unexpected relationship result: {results:?}"),
            }
        }
        query.delivery_completed(&output).await?;
    }
    let output = query
        .transform(input(
            SourceChange::Replace {
                element: item_with_id("one", 3, 4),
            },
            4,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
    match results.as_slice() {
        [QueryPartEvaluationContext::Updating { before, after, .. }] => {
            assert_eq!(before.get("left"), Some(&VariableValue::from(json!(1))));
            assert_eq!(after.get("left"), Some(&VariableValue::from(json!(3))));
            assert_eq!(after.get("right"), Some(&VariableValue::from(json!(2))));
        }
        _ => anyhow::bail!("unexpected relationship update: {results:?}"),
    }
    query.delivery_completed(&output).await?;
    let output = query
        .transform(input(
            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "link"),
                    labels: Arc::from([Arc::from("LINK")]),
                    effective_from: 5,
                },
            },
            5,
        )?)
        .await?;
    assert_eq!(output.len(), 1);
    let results = QueryChangeCodec::decode_evaluation(&output[0].envelope)?;
    assert!(matches!(
        results.as_slice(),
        [QueryPartEvaluationContext::Removing { .. }]
    ));
    query.stop().await?;
    Ok(())
}

struct Source {
    descriptor: ComponentDescriptor,
    envelope: Option<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.envelope.take().map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").expect("constant port"),
            envelope,
        }))
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    received: Arc<Mutex<Vec<ChangeEnvelope>>>,
    notification: Arc<Notify>,
}

#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.received
            .lock()
            .map_err(|_| anyhow::anyhow!("query test output poisoned"))?
            .push(input.envelope);
        self.notification.notify_one();
        Ok(())
    }
}

fn port(name: &str, direction: PortDirection) -> Result<PortDescriptor> {
    Ok(PortDescriptor::new(
        PortId::try_new(name)?,
        direction,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    ))
}

#[tokio::test(flavor = "current_thread")]
async fn separately_loaded_query_runs_in_a_graph() -> Result<()> {
    graph_query(false, QueryPublicationMode::Atomic).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separately_loaded_indexed_query_runs_in_a_graph() -> Result<()> {
    graph_query(true, QueryPublicationMode::Atomic).await
}

#[tokio::test(flavor = "current_thread")]
async fn separately_loaded_non_atomic_query_uses_declared_resource_profile() -> Result<()> {
    graph_query(true, QueryPublicationMode::NonAtomic).await
}

async fn graph_query(indexed: bool, publication: QueryPublicationMode) -> Result<()> {
    let plugin = computation::load(library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == if indexed {
                    "drasi.experimental/indexed-query"
                } else {
                    "drasi.experimental/volatile-query"
                }
        })
        .context("missing query factory")?
        .clone();
    let source = ComponentId::try_new("source")?;
    let query = ComponentId::try_new("query")?;
    let sink = ComponentId::try_new("sink")?;
    let source_endpoint = Endpoint::new(source.clone(), PortId::try_new("out")?);
    let query_input = Endpoint::new(query.clone(), PortId::try_new("in")?);
    let query_output = Endpoint::new(query.clone(), PortId::try_new("out")?);
    let sink_endpoint = Endpoint::new(sink.clone(), PortId::try_new("in")?);
    let received = Arc::new(Mutex::new(Vec::new()));
    let notification = Arc::new(Notify::new());
    let directory = tempfile::tempdir()?;
    let mut specification = factory.specification(
        query,
        if indexed {
            json!({
                "query":"MATCH (n:Item) RETURN n.value AS value",
                "publication": if publication == QueryPublicationMode::Atomic { "atomic" } else { "non_atomic" },
            })
        } else {
            json!({"query":"MATCH (n:Item) RETURN n.value AS value","volatile":true})
        },
    )?;
    let mut builder = ComputationGraph::builder("plugin-query");
    if indexed {
        let resource = ResourceId::try_new("indexes")?;
        specification
            .dependencies
            .insert("processing".into(), vec![resource.clone()]);
        let provider = Arc::new(RocksDbComputationProvider::new(
            directory.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
            ),
        ));
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "processing".into(),
            })?
            .provide_resource(
                resource,
                ResourceHandle::new(
                    ResourceRole::StateStore,
                    Arc::new(
                        computation::NativeProcessingResource::new(provider)
                            .with_publication(publication),
                    ),
                ),
            )?;
    }
    let mut graph = builder
        .source(Box::new(Source {
            descriptor: ComponentDescriptor::try_new(
                source,
                vec![port("out", PortDirection::Output)?],
            )?,
            envelope: Some(
                input(
                    SourceChange::Insert {
                        element: item(1, 1),
                    },
                    1,
                )?
                .envelope,
            ),
        }))
        .component(specification, factory)
        .sink(Box::new(Sink {
            descriptor: ComponentDescriptor::try_new(
                sink,
                vec![port("in", PortDirection::Input)?],
            )?,
            received: received.clone(),
            notification: notification.clone(),
        }))
        .connect(
            EdgeDefinition::new(source_endpoint.clone(), query_input),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(query_output.clone(), sink_endpoint),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .bind_stream(source_endpoint, StreamId::try_new("source/out")?)
        .bind_stream(query_output, StreamId::try_new("query/out")?)
        .build()?;
    tokio::time::timeout(Duration::from_secs(10), graph.start()?).await??;
    tokio::time::timeout(Duration::from_secs(10), notification.notified()).await?;
    graph.shutdown().await?;
    let received = received
        .lock()
        .map_err(|_| anyhow::anyhow!("query test output poisoned"))?;
    assert_eq!(received.len(), 1);
    let results = QueryChangeCodec::decode_evaluation(&received[0])?;
    match results.as_slice() {
        [QueryPartEvaluationContext::Adding { after, .. }] => {
            assert_eq!(after.get("value"), Some(&VariableValue::from(json!(1))));
        }
        _ => anyhow::bail!("unexpected graph query result: {results:?}"),
    }
    Ok(())
}
