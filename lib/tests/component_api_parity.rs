// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{sync::Arc, time::Duration};

use drasi_lib::{computation::v1::*, ComponentStatus, DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use serde_json::json;

const DEADLINE: Duration = Duration::from_secs(10);

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
        let (source, _input) = ApplicationSource::new("input", ApplicationSourceConfig {
            properties: Default::default(), durability: None,
        })?;
        let core = DrasiLib::builder().with_source(source).build().await?;
        core.computation_component("input")?.wait_created().await?;
        let batch = core.computation_pipeline()?
            .source(core.borrow_computation_source("input").await?, SourceSubscriptionOptions::default())?
            .query(Query::cypher("query").query("MATCH (n) RETURN n")
                .from_source("input").enable_bootstrap(false).build()).build()?;
        core.add_components(batch).await?;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let other = core.add_computation_component(ComponentAddition::new(ConstructedComponent::service(Box::new(BlockingStop {
            descriptor: ComponentDescriptor::try_new(ComponentId::try_new("other")?, Vec::new())?,
            entered: entered.clone(), release: release.clone(), hold: true,
        })))).await?;
        core.start().await?;
        let mut other_stop = Box::pin(other.stop());
        tokio::select! {
            result = &mut other_stop => panic!("unrelated stop finished before its release: {result:?}"),
            _ = entered.notified() => {}
        }
        let mut query_stop = Box::pin(core.stop_query("query"));
        tokio::select! {
            result = &mut other_stop => panic!("unrelated stop completed while blocked: {result:?}"),
            result = &mut query_stop => panic!("query stop completed while another stop was blocked: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        assert_eq!(core.get_query_status("query").await?, ComponentStatus::Running);
        release.notify_one();
        let (other_result, query_result) = tokio::join!(other_stop, query_stop);
        other_result?;
        query_result?;
        assert_eq!(other.observed()?.lifecycle, ComponentLifecycle::Stopped);
        assert_eq!(core.get_query_status("query").await?, ComponentStatus::Stopped);
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
        let core = DrasiLib::builder().build().await?;
        let entered = Arc::new(tokio::sync::Notify::new());
        let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (_input, receiver) = tokio::sync::mpsc::channel(2);
        let (sender, _output) = tokio::sync::mpsc::channel(2);
        let endpoint = |id, port| {
            Endpoint::new(
                ComponentId::try_new(id).unwrap(),
                PortId::try_new(port).unwrap(),
            )
        };
        let batch = ComponentBatch::builder()
            .source(Box::new(NativeInput {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("input")?,
                    vec![PortDescriptor::new(
                        PortId::try_new("out")?,
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                receiver,
            }))
            .query(Box::new(PendingQuery {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("pending")?,
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
                )?,
                entered: entered.clone(),
                stops: stops.clone(),
            }))
            .sink(Box::new(NativeOutput {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("output")?,
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                sender,
            }))
            .lifecycle_policy(
                ComponentId::try_new("pending")?,
                LifecyclePolicy { auto_start: false },
            )
            .bind_stream(endpoint("input", "out"), StreamId::try_new("input/out")?)
            .bind_stream(
                endpoint("pending", "out"),
                StreamId::try_new("pending/out")?,
            )
            .connect(
                EdgeDefinition::new(endpoint("input", "out"), endpoint("pending", "in")),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("pending", "out"), endpoint("output", "in")),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            )
            .build()?;
        core.add_components(batch).await?;
        core.start().await?;
        let mut starting = Box::pin(core.start_query("pending"));
        tokio::select! {
            result = &mut starting => panic!("query start did not remain pending: {result:?}"),
            _ = entered.notified() => {}
        }
        core.stop_query("pending").await?;
        assert!(starting.await.is_err());
        assert_eq!(
            core.get_query_status("pending").await?,
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
        let core = DrasiLib::builder().build().await?;
        let query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: core.computation_info().await?.id,
                id: ComponentId::try_new("direct")?,
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
        let endpoint = |id, port| {
            Endpoint::new(
                ComponentId::try_new(id).unwrap(),
                PortId::try_new(port).unwrap(),
            )
        };
        let batch = ComponentBatch::builder()
            .source(Box::new(NativeInput {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("input")?,
                    vec![PortDescriptor::new(
                        PortId::try_new("out")?,
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                receiver,
            }))
            .query(Box::new(query))
            .sink(Box::new(NativeOutput {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("output")?,
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                sender: output,
            }))
            .bind_stream(endpoint("input", "out"), StreamId::try_new("input/out")?)
            .bind_stream(endpoint("direct", "out"), StreamId::try_new("direct/out")?)
            .connect(
                EdgeDefinition::new(endpoint("input", "out"), endpoint("direct", "in")),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("direct", "out"), endpoint("output", "in")),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            )
            .build()?;
        core.add_components(batch).await?;
        assert_eq!(
            core.list_queries().await?,
            vec![("direct".into(), ComponentStatus::Added)]
        );
        assert_eq!(
            core.get_query_config("direct").await?.query,
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
            core.get_query_results("direct").await?,
            vec![json!({"value":42})]
        );
        let query = core
            .query_manager()
            .get_query_instance("direct")
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(query.fetch_outbox(0).await?.results.len(), 1);
        assert!(query.subscribe("unsupported".into()).await.is_err());
        core.shutdown().await?;
        assert!(query.fetch_snapshot().await.is_err());
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

async fn query_api(native: bool, added_while_running: bool) -> anyhow::Result<()> {
    let (source, input) = ApplicationSource::new(
        "input",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?;
    let core = DrasiLib::builder()
        .with_id(format!("query-api-{native}-{added_while_running}"))
        .with_source(source)
        .build()
        .await?;
    core.computation_component("input")?.wait_created().await?;
    if added_while_running {
        core.start().await?;
    }
    let config = Query::cypher("items")
        .query("MATCH (n:Item) RETURN n.value AS value")
        .from_source("input")
        .enable_bootstrap(false)
        .auto_start(true)
        .with_outbox_capacity(17)
        .build();
    if native {
        let batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source("input").await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(config.clone())
            .build()?;
        assert!(core.add_components(batch).await?.committed);
    } else {
        core.add_query(config.clone()).await?;
    }
    assert!(core
        .list_queries()
        .await?
        .iter()
        .any(|(id, _)| id == "items"));
    assert_eq!(
        serde_json::to_value(core.get_query_config("items").await?)?,
        serde_json::to_value(&config)?,
        "query configuration must not depend on its admission path"
    );
    let info = core.get_query_info("items").await?;
    assert_eq!(info.query, config.query);
    assert_eq!(info.source_subscriptions[0].source_id, "input");
    core.subscribe_query_logs("items").await?;
    core.subscribe_query_events("items").await?;
    assert!(
        core.remove_source("input", false).await.is_err(),
        "a query must protect its source dependency regardless of admission path"
    );
    assert!(core.get_graph().await.contains("items"));
    let ordinary = core.snapshot_configuration().await?;
    assert_eq!(
        ordinary
            .queries
            .iter()
            .filter(|query| query.id == "items")
            .count(),
        1
    );
    let full = core.snapshot_computation_configuration().await?;
    assert_eq!(
        full.instance
            .queries
            .iter()
            .filter(|query| query.id == "items")
            .count()
            + full
                .native_components
                .iter()
                .flat_map(|native| &native.topology.components)
                .filter(|node| node.descriptor.id().as_str() == "items")
                .count(),
        1,
        "configuration export must not duplicate native query recipes"
    );

    let (reaction, handle) = ApplicationReaction::new("output", vec!["items".into()]);
    let mut received = handle.take_receiver().await.expect("application receiver");
    core.add_reaction(reaction).await?;
    if !added_while_running {
        assert_eq!(
            core.get_query_status("items").await?,
            ComponentStatus::Added
        );
        assert!(core.get_query_results("items").await.is_err());
        core.start().await?;
    }
    tokio::time::timeout(
        DEADLINE,
        core.computation_component("output")?.wait_started(),
    )
    .await??;
    let query = core
        .query_manager()
        .get_query_instance("items")
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
        assert_eq!(result.query_id, "items");
        assert_eq!(result.sequence, sequence as u64);
        assert!(
            matches!(&result.results[..], [drasi_lib::channels::ResultDiff::Add { data, .. }]
            if data == &json!({"value":sequence}))
        );
    }
    let mut rows = core.get_query_results("items").await?;
    rows.sort_by_key(|row| row["value"].as_i64());
    assert_eq!(rows, [json!({"value":1}), json!({"value":2})]);
    assert_eq!(query.fetch_snapshot().await?.as_of_sequence, 2);
    assert_eq!(query.fetch_outbox(0).await?.results.len(), 2);
    core.get_query_output_metrics("items").await?;
    core.stop_query("items").await?;
    assert_eq!(
        core.get_query_status("items").await?,
        ComponentStatus::Stopped
    );
    assert!(query.fetch_snapshot().await.is_err());
    core.start_query("items").await?;
    tokio::time::timeout(
        DEADLINE,
        core.computation_component("items")?.wait_started(),
    )
    .await??;
    assert_eq!(core.get_query_results("items").await?.len(), 2);
    let topology = core.inspect_computation_graph()?.topology();
    assert!(matches!(
        topology
            .nodes
            .get(&GraphEntityId::Component(ComponentId::try_new("items")?)),
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
            "input",
            ApplicationSourceConfig {
                properties: Default::default(),
                durability: None,
            },
        )?;
        let core = DrasiLib::builder().with_source(source).build().await?;
        core.computation_component("input")?.wait_created().await?;
        for (id, auto_start) in [("manual", false), ("sibling", true)] {
            let query = Query::cypher(id)
                .query("MATCH (n:Item) RETURN n.value AS value")
                .from_source("input")
                .auto_start(auto_start)
                .enable_bootstrap(false)
                .build();
            let batch = core
                .computation_pipeline()?
                .source(
                    core.borrow_computation_source("input").await?,
                    SourceSubscriptionOptions::default(),
                )?
                .query(query)
                .build()?;
            core.add_components(batch).await?;
        }
        core.start().await?;
        assert_eq!(
            core.get_query_status("manual").await?,
            ComponentStatus::Added
        );
        assert_eq!(
            core.get_query_status("sibling").await?,
            ComponentStatus::Running
        );
        let sibling = core.computation_component("sibling")?;
        core.start_query("manual").await?;
        for (sequence, manual_running) in [(1, true), (2, false), (3, true)] {
            if sequence == 2 {
                core.stop_query("manual").await?;
                assert_eq!(
                    core.get_query_status("manual").await?,
                    ComponentStatus::Stopped
                );
                assert!(core.get_query_results("manual").await.is_err());
                assert_eq!(
                    core.get_source_status("input").await?,
                    ComponentStatus::Running
                );
            } else if sequence == 3 {
                core.start_query("manual").await?;
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
                    let rows = core.get_query_results(id).await?;
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
                core.computation_component("sibling")?.generation()
            );
            assert_eq!(
                core.get_query_status("sibling").await?,
                ComponentStatus::Running
            );
        }
        let mut rows = core.get_query_results("manual").await?;
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
            "input",
            ApplicationSourceConfig {
                properties: Default::default(),
                durability: None,
            },
        )?;
        let core = DrasiLib::builder().with_source(source).build().await?;
        core.computation_component("input")?.wait_created().await?;
        let config = Query::cypher("items")
            .query("MATCH (n:Item) RETURN n.value AS value")
            .from_source("input")
            .enable_bootstrap(false)
            .build();
        let batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source("input").await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(config)
            .build()?;
        core.add_components(batch).await?;
        core.start().await?;
        let old = core
            .query_manager()
            .get_query_instance("items")
            .await
            .map_err(anyhow::Error::msg)?;
        let mut old_subscription = old.subscribe("observer".into()).await?;
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
        let control = core.computation_control()?;
        let desired = control.desired_snapshot();
        let replacement = desired
            .select(GraphSelection::Exact(vec![ComponentId::try_new("items")?]))?
            .components
            .remove(0);
        let preview = control
            .preview(
                desired.revision,
                vec![DesiredMutation::ReplaceComponent(replacement)],
            )
            .await?;
        control
            .reconcile(preview, TopologyBindings::default())
            .await?;
        let new = core
            .query_manager()
            .get_query_instance("items")
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
        "input",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?;
    let core = DrasiLib::builder().with_source(source).build().await?;
    core.computation_component("input")?.wait_created().await?;
    let mut config = Query::cypher("invalid")
        .query("MATCH (n:Item) RETURN n.value AS value")
        .from_source("input")
        .auto_start(false)
        .enable_bootstrap(false)
        .build();
    let mut batch = core
        .computation_pipeline()?
        .source(
            core.borrow_computation_source("input").await?,
            SourceSubscriptionOptions::default(),
        )?
        .query(config.clone())
        .build()?;
    config.query = "NOT A QUERY".into();
    let query = batch
        .definition
        .components
        .iter_mut()
        .find(|node| node.descriptor.id().as_str() == "invalid")
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
        ConfigurationValue::Literal(serde_json::to_value(&config)?),
    );
    assert!(core.add_components(batch).await?.committed);
    assert!(core
        .computation_component("invalid")?
        .wait_created()
        .await
        .is_err());
    assert_eq!(
        core.list_queries().await?,
        vec![("invalid".into(), ComponentStatus::Error)]
    );
    assert_eq!(core.get_query_config("invalid").await?.query, "NOT A QUERY");
    assert!(core
        .get_query_info("invalid")
        .await?
        .error_message
        .is_some());
    let (events, _) = core.subscribe_query_events("invalid").await?;
    assert!(events
        .iter()
        .any(|event| event.status == ComponentStatus::Error));
    assert!(core.get_query_results("invalid").await.is_err());
    assert!(core.get_query_config("missing").await.is_err());
    assert!(core.get_query_config("input").await.is_err());
    core.shutdown().await?;
    Ok(())
}
