// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[path = "support/mod.rs"]
mod fixture_support;

use std::{sync::Arc, time::Duration};

use drasi_lib::{computation::v1::*, ComponentStatus, DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use serde_json::json;

const DEADLINE: Duration = Duration::from_secs(10);

fn input_id() -> ComponentId {
    fixture_support::component_id(113)
}

fn endpoint(id: u128, port: &str) -> Endpoint {
    Endpoint::new(
        fixture_support::component_id(id),
        PortId::try_new(port).expect("fixture port"),
    )
}

struct BlockingStop {
    descriptor: ComponentDescriptor,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    hold: bool,
}

#[async_trait::async_trait]
impl ComputationComponent for BlockingStop {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        if self.hold {
            self.hold = false;
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl ComputationService for BlockingStop {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn native_query_stop_preserves_an_unrelated_inflight_stop() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let (source, _input) = ApplicationSource::new(drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071").expect("fixture UUID"), ApplicationSourceConfig {
            properties: Default::default(), durability: None,
        })?;
        let core = DrasiLib::builder().with_allow_external_component_ids(true).with_query_factory(drasi_lib::computation::v1::FactoryRegistry::standard().get(&drasi_lib::computation::v1::ImplementationIdentity::try_new("drasi/continuous-query", "1").expect("reference query identity")).expect("reference query factory").clone()).with_source(drasi_core_types::ComponentName::try_new("input").expect("fixture component name"), source).build().await?;
        core.component_by_name(&drasi_core_types::ComponentName::try_new("input").expect("fixture component name"))?.wait_created().await?;
        let batch = core.computation_pipeline()?
            .source(core.borrow_computation_source_by_id(&drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071").expect("fixture UUID")).await?, SourceSubscriptionOptions::default())?
            .query(Query::cypher(drasi_core_types::ComponentName::try_new("query").expect("fixture component name")).query("MATCH (n) RETURN n")
                .from_input_by_id(input_id()).enable_bootstrap(false).build()?
                .with_assigned_id(fixture_support::component_id(46))).build()?;
        core.add_components(batch).await?;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let other = core.add_computation_component(ComponentAddition::new(
            ComponentName::try_new("other")?,
            ConstructedComponent::service(Box::new(BlockingStop {
                descriptor: ComponentDescriptor::try_new(fixture_support::component_id(666), Vec::new())?,
                entered: entered.clone(), release: release.clone(), hold: true,
            })),
        )).await?;
        core.start().await?;
        let mut other_stop = Box::pin(other.stop());
        tokio::select! {
            result = &mut other_stop => panic!("unrelated stop finished before its release: {result:?}"),
            _ = entered.notified() => {}
        }
        let query_name =
            drasi_core_types::ComponentName::try_new("query").expect("fixture component name");
        let mut query_stop = Box::pin(core.stop_query_by_name(&query_name));
        tokio::select! {
            result = &mut other_stop => panic!("unrelated stop completed while blocked: {result:?}"),
            result = &mut query_stop => result?,
            _ = tokio::time::sleep(DEADLINE) => panic!("independent query stop stalled behind an unrelated stop"),
        }
        assert_eq!(core.get_query_status_by_name(&drasi_core_types::ComponentName::try_new("query").expect("fixture component name")).await?, ComponentStatus::Stopped);
        assert_eq!(other.observed()?.lifecycle, ComponentLifecycle::Stopping);
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut other_stop).await.is_err());
        release.notify_one();
        other_stop.await?;
        assert_eq!(other.observed()?.lifecycle, ComponentLifecycle::Stopped);
        assert_eq!(core.get_query_status_by_name(&drasi_core_types::ComponentName::try_new("query").expect("fixture component name")).await?, ComponentStatus::Stopped);
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}

struct NativeInput {
    descriptor: ComponentDescriptor,
    receiver: tokio::sync::mpsc::Receiver<OutputEnvelope>,
}

struct PendingQuery {
    descriptor: ComponentDescriptor,
    entered: Arc<tokio::sync::Notify>,
    stops: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl ComputationComponent for PendingQuery {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.entered.notify_one();
        std::future::pending().await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Transformer for PendingQuery {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::bail!("a pending query must not process input")
    }
}

#[tokio::test]
async fn native_query_stop_cancels_its_pending_start() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let core = DrasiLib::builder()
            .with_allow_external_component_ids(true)
            .with_query_factory(
                drasi_lib::computation::v1::FactoryRegistry::standard()
                    .get(
                        &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .build()
            .await?;
        let entered = Arc::new(tokio::sync::Notify::new());
        let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_input, receiver) = tokio::sync::mpsc::channel(2);
        let (sender, _output) = tokio::sync::mpsc::channel(2);
        let batch = ComponentBatch::builder()
            .source(
                ComponentName::try_new("input")?,
                Box::new(NativeInput {
                    descriptor: ComponentDescriptor::try_new(
                        fixture_support::component_id(113),
                        vec![PortDescriptor::new(
                            PortId::try_new("out")?,
                            PortDirection::Output,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )?,
                    receiver,
                }),
            )
            .transformer(
                ComponentName::try_new("pending")?,
                Box::new(PendingQuery {
                    descriptor: ComponentDescriptor::try_new(
                        fixture_support::component_id(753),
                        vec![
                            PortDescriptor::new(
                                PortId::try_new("in")?,
                                PortDirection::Input,
                                GraphChangeCodec::schema().descriptor().clone(),
                                PipeRequirements::default(),
                            ),
                            PortDescriptor::new(
                                PortId::try_new("out")?,
                                PortDirection::Output,
                                QueryChangeCodec::schema().descriptor().clone(),
                                PipeRequirements::default(),
                            ),
                        ],
                    )?
                    .with_semantic_kind(ComponentSemanticKind::Query),
                    entered: entered.clone(),
                    stops: stops.clone(),
                }),
            )
            .sink(
                ComponentName::try_new("output")?,
                Box::new(NativeOutput {
                    descriptor: ComponentDescriptor::try_new(
                        fixture_support::component_id(311),
                        vec![PortDescriptor::new(
                            PortId::try_new("in")?,
                            PortDirection::Input,
                            QueryChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )?,
                    sender,
                }),
            )
            .lifecycle_policy(
                fixture_support::component_id(753),
                LifecyclePolicy { auto_start: false },
            )
            .bind_stream(endpoint(113, "out"), StreamId::try_new("input/out")?)
            .bind_stream(endpoint(753, "out"), StreamId::try_new("pending/out")?)
            .connect(
                EdgeDefinition::new(endpoint(113, "out"), endpoint(753, "in")),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(753, "out"), endpoint(311, "in")),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            )
            .build()?;
        core.add_components(batch).await?;
        core.start().await?;
        let pending_name =
            drasi_core_types::ComponentName::try_new("pending").expect("fixture component name");
        let mut starting = Box::pin(core.start_query_by_name(&pending_name));
        tokio::select! {
            result = &mut starting => panic!("query start did not remain pending: {result:?}"),
            _ = entered.notified() => {}
        }
        core.stop_query_by_name(
            &drasi_core_types::ComponentName::try_new("pending").expect("fixture component name"),
        )
        .await?;
        assert!(starting.await.is_err());
        assert_eq!(
            core.get_query_status_by_name(
                &drasi_core_types::ComponentName::try_new("pending")
                    .expect("fixture component name")
            )
            .await?,
            ComponentStatus::Stopped
        );
        assert_eq!(stops.load(std::sync::atomic::Ordering::SeqCst), 1);
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[async_trait::async_trait]
impl ComputationComponent for NativeInput {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl EnvelopeSource for NativeInput {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.receiver.recv().await)
    }
}

struct NativeOutput {
    descriptor: ComponentDescriptor,
    sender: tokio::sync::mpsc::Sender<ChangeEnvelope>,
}

#[async_trait::async_trait]
impl ComputationComponent for NativeOutput {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl EnvelopeSink for NativeOutput {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.sender.send(input.envelope).await?;
        Ok(())
    }
}

#[tokio::test]
async fn preconstructed_native_query_exposes_its_existing_results_without_a_catalogue(
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        use drasi_core::models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        };
        let core = DrasiLib::builder()
            .with_allow_external_component_ids(true)
            .with_query_factory(
                drasi_lib::computation::v1::FactoryRegistry::standard()
                    .get(
                        &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .build()
            .await?;
        let query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: core.computation_info().await?.id,
                id: fixture_support::component_id(461),
                query: "MATCH (n:Item) RETURN n.value AS value".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("direct/out")?,
                outbox_capacity: std::num::NonZeroUsize::new(8).unwrap(),
            },
            Arc::new(drasi_core::computation::InMemoryComputationProvider),
        )
        .await?;
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        let (output, mut received) = tokio::sync::mpsc::channel(2);
        let batch = ComponentBatch::builder()
            .source(
                ComponentName::try_new("input")?,
                Box::new(NativeInput {
                    descriptor: ComponentDescriptor::try_new(
                        fixture_support::component_id(113),
                        vec![PortDescriptor::new(
                            PortId::try_new("out")?,
                            PortDirection::Output,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )?,
                    receiver,
                }),
            )
            .transformer(ComponentName::try_new("direct")?, Box::new(query))
            .sink(
                ComponentName::try_new("output")?,
                Box::new(NativeOutput {
                    descriptor: ComponentDescriptor::try_new(
                        fixture_support::component_id(311),
                        vec![PortDescriptor::new(
                            PortId::try_new("in")?,
                            PortDirection::Input,
                            QueryChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )?,
                    sender: output,
                }),
            )
            .bind_stream(endpoint(113, "out"), StreamId::try_new("input/out")?)
            .bind_stream(endpoint(461, "out"), StreamId::try_new("direct/out")?)
            .connect(
                EdgeDefinition::new(endpoint(113, "out"), endpoint(461, "in")),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(461, "out"), endpoint(311, "in")),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            )
            .build()?;
        core.add_components(batch).await?;
        let queries = core.list_queries().await?;
        assert_eq!(queries.len(), 1);
        assert_eq!(
            queries[0].name,
            drasi_core_types::ComponentName::try_new("direct").expect("fixture component name")
        );
        assert_eq!(queries[0].status, ComponentStatus::Added);
        assert_eq!(
            core.get_query_config_by_name(
                &drasi_core_types::ComponentName::try_new("direct")
                    .expect("fixture component name")
            )
            .await?
            .query,
            "MATCH (n:Item) RETURN n.value AS value"
        );
        core.start().await?;
        sender
            .send(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: GraphChangeCodec::encode_change(
                    SourceChange::Insert {
                        element: Element::Node {
                            metadata: ElementMetadata {
                                reference: ElementReference::new("input", "one"),
                                labels: vec![Arc::from("Item")].into(),
                                effective_from: 1,
                            },
                            properties: ElementPropertyMap::from(json!({"value":42})),
                        },
                    },
                    StreamId::try_new("input/out")?,
                    1,
                    None,
                )?,
            })
            .await?;
        let event = tokio::time::timeout(DEADLINE, received.recv())
            .await?
            .unwrap();
        assert_eq!(QueryChangeCodec::query_sequence(&event)?, 1);
        assert_eq!(
            core.get_query_results_by_name(
                &drasi_core_types::ComponentName::try_new("direct")
                    .expect("fixture component name")
            )
            .await?,
            vec![json!({"value":42})]
        );
        let query = core
            .query_manager()
            .get_query_instance_by_name(
                &drasi_core_types::ComponentName::try_new("direct")
                    .expect("fixture component name"),
            )
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(query.fetch_outbox(0).await?.results.len(), 1);
        assert!(query
            .subscribe(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d4")
                    .expect("fixture UUID")
            )
            .await
            .is_err());
        core.shutdown().await?;
        assert!(query.fetch_snapshot().await.is_err());
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

async fn query_api(native: bool, added_while_running: bool) -> anyhow::Result<()> {
    let (source, input) = ApplicationSource::new(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071")
            .expect("fixture UUID"),
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?;
    let core = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_id(format!("query-api-{native}-{added_while_running}"))
        .with_source(
            drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
            source,
        )
        .build()
        .await?;
    core.component_by_name(
        &drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
    )?
    .wait_created()
    .await?;
    if added_while_running {
        core.start().await?;
    }
    let config = Query::cypher(
        drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
    )
    .query("MATCH (n:Item) RETURN n.value AS value")
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
    )
    .enable_bootstrap(false)
    .auto_start(true)
    .with_outbox_capacity(17)
    .build()?;
    if native {
        let mut native_config = config.clone();
        native_config.inputs[0].component =
            drasi_lib::config::ComponentReference::by_id(input_id());
        let batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source_by_id(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071")
                        .expect("fixture UUID"),
                )
                .await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(native_config.with_assigned_id(fixture_support::component_id(435)))
            .build()?;
        core.add_components(batch).await?;
    } else {
        core.add_query(config.clone()).await?;
    }
    let query_id = core.component_by_name(&config.name)?.id().clone();
    let mut expected = config.clone().with_assigned_id(query_id.clone());
    expected.inputs[0].component = drasi_lib::config::ComponentReference::by_id(input_id());
    assert!(core
        .list_queries()
        .await?
        .iter()
        .any(|entry| entry.name == config.name));
    assert_eq!(
        serde_json::to_value(
            core.get_query_config_by_name(
                &drasi_core_types::ComponentName::try_new("items").expect("fixture component name")
            )
            .await?
        )?,
        serde_json::to_value(&expected)?,
        "query configuration must not depend on its admission path"
    );
    let info = core
        .get_query_info_by_name(
            &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
        )
        .await?;
    assert_eq!(info.query, config.query);
    assert_eq!(info.inputs[0].component.as_id(), Some(&input_id()));
    core.subscribe_query_logs_by_name(
        &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
    )
    .await?;
    core.subscribe_query_events_by_name(
        &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
    )
    .await?;
    assert!(
        core.remove_source_by_name(
            &drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
            false
        )
        .await
        .is_err(),
        "a query must protect its source dependency regardless of admission path"
    );
    assert!(core.get_graph().await?.contains_by_name(
        &drasi_core_types::ComponentName::try_new("items").expect("fixture component name")
    ));
    let ordinary = core.snapshot_configuration().await?;
    assert_eq!(
        ordinary
            .queries
            .iter()
            .filter(|query| query.id == query_id)
            .count(),
        1
    );
    let full = core.snapshot_computation_configuration().await?;
    assert_eq!(
        full.instance
            .queries
            .iter()
            .filter(|query| query.id == query_id)
            .count()
            + full
                .native_components
                .iter()
                .flat_map(|native| &native.topology.components)
                .filter(|node| node.descriptor.id() == &query_id)
                .count(),
        1,
        "configuration export must not duplicate native query recipes"
    );

    let (reaction, handle) = ApplicationReaction::new(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000137")
            .expect("fixture UUID"),
        vec![drasi_computation_contracts::ComponentReference::by_id(query_id.clone())],
    );
    let mut received = handle.take_receiver().await.expect("application receiver");
    core.add_reaction(
        drasi_core_types::ComponentName::try_new("output").expect("fixture component name"),
        reaction,
    )
    .await?;
    if !added_while_running {
        assert_eq!(
            core.get_query_status_by_name(
                &drasi_core_types::ComponentName::try_new("items").expect("fixture component name")
            )
            .await?,
            ComponentStatus::Added
        );
        assert!(core
            .get_query_results_by_name(
                &drasi_core_types::ComponentName::try_new("items").expect("fixture component name")
            )
            .await
            .is_err());
        core.start().await?;
    }
    tokio::time::timeout(
        DEADLINE,
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("output").expect("fixture component name"),
        )?
        .wait_started(),
    )
    .await??;
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    for sequence in 1..=2 {
        input
            .send_node_insert(
                sequence.to_string(),
                vec!["Item"],
                PropertyMapBuilder::new()
                    .with_integer("value", sequence)
                    .build(),
            )
            .await?;
        let result = tokio::time::timeout(DEADLINE, received.recv())
            .await?
            .expect("query result");
        assert_eq!(result.query_id, query_id);
        assert_eq!(result.sequence, sequence as u64);
        assert!(
            matches!(&result.results[..], [drasi_lib::channels::ResultDiff::Add { data, .. }]
            if data == &json!({"value":sequence}))
        );
    }
    let mut rows = core
        .get_query_results_by_name(
            &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
        )
        .await?;
    rows.sort_by_key(|row| row["value"].as_i64());
    assert_eq!(rows, [json!({"value":1}), json!({"value":2})]);
    assert_eq!(query.fetch_snapshot().await?.as_of_sequence, 2);
    assert_eq!(query.fetch_outbox(0).await?.results.len(), 2);
    core.get_query_output_metrics_by_name(
        &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
    )
    .await?;
    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
    )
    .await?;
    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core_types::ComponentName::try_new("items").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Stopped
    );
    assert!(query.fetch_snapshot().await.is_err());
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
    )
    .await?;
    tokio::time::timeout(
        DEADLINE,
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
        )?
        .wait_started(),
    )
    .await??;
    assert_eq!(
        core.get_query_results_by_name(
            &drasi_core_types::ComponentName::try_new("items").expect("fixture component name")
        )
        .await?
        .len(),
        2
    );
    let topology = core.inspect_computation_graph()?.topology();
    assert!(matches!(
        topology.nodes.get(&GraphEntityId::Component(query_id)),
        Some(GraphEntity::Component(_))
    ));
    core.shutdown().await?;
    assert!(query.fetch_snapshot().await.is_err());
    Ok(())
}

#[tokio::test]
async fn ordinary_query_apis_preserve_creation_and_runtime_behavior() -> anyhow::Result<()> {
    for running in [false, true] {
        tokio::time::timeout(Duration::from_secs(30), query_api(false, running)).await??;
    }
    Ok(())
}

#[tokio::test]
async fn native_query_apis_match_ordinary_creation_and_runtime_behavior() -> anyhow::Result<()> {
    for running in [false, true] {
        tokio::time::timeout(Duration::from_secs(30), query_api(true, running)).await??;
    }
    Ok(())
}

#[tokio::test]
async fn manual_native_query_lifecycle_controls_its_helpers_without_stopping_sibling_queries(
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (source, input) = ApplicationSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071")
                .expect("fixture UUID"),
            ApplicationSourceConfig {
                properties: Default::default(),
                durability: None,
            },
        )?;
        let core = DrasiLib::builder()
            .with_allow_external_component_ids(true)
            .with_query_factory(
                drasi_lib::computation::v1::FactoryRegistry::standard()
                    .get(
                        &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_source(
                drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
                source,
            )
            .build()
            .await?;
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
        )?
        .wait_created()
        .await?;
        for (id, auto_start) in [("manual", false), ("sibling", true)] {
            let query_name =
                drasi_core_types::ComponentName::try_new(id).expect("fixture component name");
            let query_id = match id {
                "manual" => fixture_support::component_id(1),
                "sibling" => fixture_support::component_id(2),
                other => panic!("missing explicit fixture component ID for {other}"),
            };
            let query = Query::cypher(query_name)
                .query("MATCH (n:Item) RETURN n.value AS value")
                .from_input_by_id(input_id())
                .auto_start(auto_start)
                .enable_bootstrap(false)
                .build()?
                .with_assigned_id(query_id);
            let batch = core
                .computation_pipeline()?
                .source(
                    core.borrow_computation_source_by_id(
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-000000000071",
                        )
                        .expect("fixture UUID"),
                    )
                    .await?,
                    SourceSubscriptionOptions::default(),
                )?
                .query(query)
                .build()?;
            core.add_components(batch).await?;
        }
        core.start().await?;
        assert_eq!(
            core.get_query_status_by_name(
                &drasi_core_types::ComponentName::try_new("manual")
                    .expect("fixture component name")
            )
            .await?,
            ComponentStatus::Added
        );
        assert_eq!(
            core.get_query_status_by_name(
                &drasi_core_types::ComponentName::try_new("sibling")
                    .expect("fixture component name")
            )
            .await?,
            ComponentStatus::Running
        );
        let sibling = core.component_by_name(
            &drasi_core_types::ComponentName::try_new("sibling").expect("fixture component name"),
        )?;
        core.start_query_by_name(
            &drasi_core_types::ComponentName::try_new("manual").expect("fixture component name"),
        )
        .await?;
        for (sequence, manual_running) in [(1, true), (2, false), (3, true)] {
            if sequence == 2 {
                core.stop_query_by_name(
                    &drasi_core_types::ComponentName::try_new("manual")
                        .expect("fixture component name"),
                )
                .await?;
                assert_eq!(
                    core.get_query_status_by_name(
                        &drasi_core_types::ComponentName::try_new("manual")
                            .expect("fixture component name")
                    )
                    .await?,
                    ComponentStatus::Stopped
                );
                assert!(core
                    .get_query_results_by_name(
                        &drasi_core_types::ComponentName::try_new("manual")
                            .expect("fixture component name")
                    )
                    .await
                    .is_err());
                assert_eq!(
                    core.get_source_status_by_name(
                        &drasi_core_types::ComponentName::try_new("input")
                            .expect("fixture component name")
                    )
                    .await?,
                    ComponentStatus::Running
                );
            } else if sequence == 3 {
                core.start_query_by_name(
                    &drasi_core_types::ComponentName::try_new("manual")
                        .expect("fixture component name"),
                )
                .await?;
            }
            input
                .send_node_insert(
                    sequence.to_string(),
                    vec!["Item"],
                    PropertyMapBuilder::new()
                        .with_integer("value", sequence)
                        .build(),
                )
                .await?;
            for (id, count) in
                [("sibling", sequence as usize), ("manual", if sequence == 1 { 1 } else { 2 })]
            {
                if id == "manual" && !manual_running {
                    continue;
                }
                loop {
                    let rows = core
                        .get_query_results_by_name(
                            &drasi_core_types::ComponentName::try_new(id)
                                .expect("fixture component name"),
                        )
                        .await?;
                    if rows.len() == count {
                        break;
                    }
                    assert!(
                        rows.len() < count,
                        "query processed an unexpected extra row"
                    );
                    tokio::task::yield_now().await;
                }
            }
            assert_eq!(
                sibling.generation(),
                core.component_by_name(
                    &drasi_core_types::ComponentName::try_new("sibling")
                        .expect("fixture component name")
                )?
                .generation()
            );
            assert_eq!(
                core.get_query_status_by_name(
                    &drasi_core_types::ComponentName::try_new("sibling")
                        .expect("fixture component name")
                )
                .await?,
                ComponentStatus::Running
            );
        }
        let mut rows = core
            .get_query_results_by_name(
                &drasi_core_types::ComponentName::try_new("manual")
                    .expect("fixture component name"),
            )
            .await?;
        rows.sort_by_key(|row| row["value"].as_i64());
        assert_eq!(rows, [json!({"value":1}), json!({"value":3})]);
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn native_query_handles_and_subscriptions_cannot_observe_replacement_instances(
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (source, input) = ApplicationSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071")
                .expect("fixture UUID"),
            ApplicationSourceConfig {
                properties: Default::default(),
                durability: None,
            },
        )?;
        let core = DrasiLib::builder()
            .with_allow_external_component_ids(true)
            .with_query_factory(
                drasi_lib::computation::v1::FactoryRegistry::standard()
                    .get(
                        &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_source(
                drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
                source,
            )
            .build()
            .await?;
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
        )?
        .wait_created()
        .await?;
        let config = Query::cypher(
            drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
        )
        .query("MATCH (n:Item) RETURN n.value AS value")
        .from_input_by_id(input_id())
        .enable_bootstrap(false)
        .build()?
        .with_assigned_id(fixture_support::component_id(435));
        let batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source_by_id(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071")
                        .expect("fixture UUID"),
                )
                .await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(config)
            .build()?;
        core.add_components(batch).await?;
        core.start().await?;
        let old = core
            .query_manager()
            .get_query_instance_by_name(
                &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
            )
            .await
            .map_err(anyhow::Error::msg)?;
        let mut old_subscription = old
            .subscribe(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b5")
                    .expect("fixture UUID"),
            )
            .await?;
        input
            .send_node_insert(
                "one",
                vec!["Item"],
                PropertyMapBuilder::new().with_integer("value", 1).build(),
            )
            .await?;
        assert_eq!(
            tokio::time::timeout(DEADLINE, old_subscription.receiver.recv())
                .await??
                .sequence,
            1
        );
        let replacement = old.get_config().clone();
        core.remove_query_by_id(&replacement.id).await?;
        let replacement = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source_by_id(&input_id()).await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(replacement)
            .build()?;
        core.add_components(replacement).await?;
        tokio::time::timeout(
            DEADLINE,
            core.component_by_name(&ComponentName::try_new("items")?)?
                .wait_started(),
        )
        .await??;
        let new = core
            .query_manager()
            .get_query_instance_by_name(
                &drasi_core_types::ComponentName::try_new("items").expect("fixture component name"),
            )
            .await
            .map_err(anyhow::Error::msg)?;
        assert!(!Arc::ptr_eq(&old, &new));
        assert_eq!(old.status().await, ComponentStatus::Stopped);
        assert!(old.start().await.is_err());
        assert!(old.fetch_snapshot().await.is_err());
        assert!(
            tokio::time::timeout(DEADLINE, old_subscription.receiver.recv())
                .await?
                .is_err()
        );
        assert_eq!(new.status().await, ComponentStatus::Running);
        assert_eq!(core.list_queries().await?.len(), 1);
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn failed_native_query_declarations_keep_configuration_status_and_events_visible(
) -> anyhow::Result<()> {
    let (source, _) = ApplicationSource::new(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071")
            .expect("fixture UUID"),
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?;
    let core = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_source(
            drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
            source,
        )
        .build()
        .await?;
    core.component_by_name(
        &drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
    )?
    .wait_created()
    .await?;
    let mut config = Query::cypher(
        drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name"),
    )
    .query("MATCH (n:Item) RETURN n.value AS value")
    .from_input_by_id(input_id())
    .auto_start(false)
    .enable_bootstrap(false)
    .build()?;
    let mut batch = core
        .computation_pipeline()?
        .source(
            core.borrow_computation_source_by_id(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000071")
                    .expect("fixture UUID"),
            )
            .await?,
            SourceSubscriptionOptions::default(),
        )?
        .query(
            config
                .clone()
                .with_assigned_id(fixture_support::component_id(185)),
        )
        .build()?;
    config.query = "NOT A QUERY".into();
    let query = batch
        .definition
        .components
        .iter_mut()
        .find(|node| node.descriptor.id() == &fixture_support::component_id(185))
        .unwrap();
    let ComponentConstruction::Factory(spec) = &mut query.construction else {
        unreachable!()
    };
    spec.configuration.insert(
        "query".into(),
        ConfigurationValue::Literal(json!(config.query)),
    );
    spec.configuration.insert(
        "query_config".into(),
        ConfigurationValue::Literal(serde_json::to_value(
            config
                .clone()
                .with_assigned_id(fixture_support::component_id(185)),
        )?),
    );
    core.add_components(batch).await?;
    assert!(core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name")
        )?
        .wait_created()
        .await
        .is_err());
    let queries = core.list_queries().await?;
    assert_eq!(queries.len(), 1);
    assert_eq!(
        queries[0].name,
        drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name")
    );
    assert_eq!(queries[0].status, ComponentStatus::Error);
    assert_eq!(
        core.get_query_config_by_name(
            &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name")
        )
        .await?
        .query,
        "NOT A QUERY"
    );
    assert!(core
        .get_query_info_by_name(
            &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name")
        )
        .await?
        .error_message
        .is_some());
    let (events, _) = core
        .subscribe_query_events_by_name(
            &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name"),
        )
        .await?;
    assert!(events
        .iter()
        .any(|event| event.status == ComponentStatus::Error));
    assert!(core
        .get_query_results_by_name(
            &drasi_core_types::ComponentName::try_new("invalid").expect("fixture component name")
        )
        .await
        .is_err());
    assert!(core
        .get_query_config_by_name(
            &drasi_core_types::ComponentName::try_new("missing").expect("fixture component name")
        )
        .await
        .is_err());
    assert!(core
        .get_query_config_by_name(
            &drasi_core_types::ComponentName::try_new("input").expect("fixture component name")
        )
        .await
        .is_err());
    core.shutdown().await?;
    Ok(())
}
