// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::time::Duration;

use anyhow::{Context, Result};
use drasi_lib::{computation::v1::*, ComponentStatus, DrasiLib, Query};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use serde_json::json;

const DEADLINE: Duration = Duration::from_secs(10);

fn source_config() -> ApplicationSourceConfig {
    ApplicationSourceConfig {
        properties: Default::default(),
        durability: None,
    }
}

async fn query_mutations(native: bool, strict_consumer: bool) -> Result<()> {
    let (left, left_input) = ApplicationSource::new("left", source_config())?;
    let (right, right_input) = ApplicationSource::new("right", source_config())?;
    let core = DrasiLib::builder()
        .with_id(format!("mutation-{native}-{strict_consumer}"))
        .with_source(left)
        .with_source(right)
        .build()
        .await?;
    for id in ["left", "right"] {
        core.computation_component(id)?.wait_created().await?;
    }
    let config = Query::cypher("items")
        .query("MATCH (n:Item) RETURN n.value AS value")
        .from_source("left")
        .enable_bootstrap(false)
        .build();
    let sibling = Query::cypher("sibling")
        .query("MATCH (n:Item) RETURN n.value AS value")
        .from_source("left")
        .enable_bootstrap(false)
        .build();
    if native {
        let batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source("left").await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(config)
            .query(sibling)
            .build()?;
        let report = core.add_components(batch).await?;
        assert!(report.committed);
        assert_eq!(report.summary, OperationSummary::Completed);
    } else {
        core.add_query(config).await?;
        core.add_query(sibling).await?;
    }
    let (reaction, output) = ApplicationReaction::new("capture", vec!["items".into()]);
    let mut received = output.take_receiver().await.context("capture receiver")?;
    core.add_reaction(reaction).await?;
    let (reaction, output) = ApplicationReaction::new("sibling-capture", vec!["sibling".into()]);
    let mut sibling_received = output.take_receiver().await.context("sibling receiver")?;
    core.add_reaction(reaction).await?;
    core.start().await?;
    for id in ["capture", "sibling-capture"] {
        tokio::time::timeout(DEADLINE, core.computation_component(id)?.wait_started()).await??;
    }
    let root = core.computation_info().await?.id;
    let old = core
        .query_manager()
        .get_query_instance("items")
        .await
        .map_err(anyhow::Error::msg)?;
    left_input
        .send_node_insert(
            "before",
            vec!["Item"],
            PropertyMapBuilder::new().with_integer("value", 7).build(),
        )
        .await?;
    tokio::time::timeout(DEADLINE, received.recv())
        .await
        .context("waiting for initial query result")?
        .context("initial result")?;
    tokio::time::timeout(DEADLINE, sibling_received.recv())
        .await
        .context("waiting for initial sibling result")?
        .context("initial sibling result")?;
    assert_eq!(
        core.get_query_results("items").await?,
        vec![json!({"value":7})]
    );

    let replacement = Query::cypher("items")
        .query("MATCH (n:Item) RETURN n.value * 2 AS value")
        .from_source("right")
        .enable_bootstrap(false)
        .build();
    if !strict_consumer {
        core.stop_reaction("capture").await?;
    }
    let updated = core.update_query("items", replacement.clone()).await;
    if strict_consumer {
        let error =
            updated.expect_err("query replacement must not discard a strict consumer checkpoint");
        assert!(
            error
                .to_string()
                .contains("different query/reset generation"),
            "unexpected replacement failure: {error}"
        );
        assert_eq!(
            core.get_reaction_status("capture").await?,
            ComponentStatus::Error
        );
    } else {
        updated?;
        assert_eq!(
            core.get_reaction_status("capture").await?,
            ComponentStatus::Stopped
        );
    }
    assert_eq!(
        serde_json::to_value(core.get_query_config("items").await?)?,
        serde_json::to_value(replacement)?
    );
    assert_eq!(
        core.get_query_status("items").await?,
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_query_status("sibling").await?,
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_source_status("left").await?,
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_source_status("right").await?,
        ComponentStatus::Running
    );
    assert!(
        old.fetch_snapshot().await.is_err(),
        "old readers must remain generation fenced"
    );
    core.remove_reaction("capture", false).await?;
    let (reaction, output) = ApplicationReaction::new("updated-capture", vec!["items".into()]);
    let mut received = output.take_receiver().await.context("updated receiver")?;
    core.add_reaction(reaction).await?;
    tokio::time::timeout(
        DEADLINE,
        core.computation_component("updated-capture")?
            .wait_started(),
    )
    .await??;
    right_input
        .send_node_insert(
            "after",
            vec!["Item"],
            PropertyMapBuilder::new().with_integer("value", 11).build(),
        )
        .await?;
    let event = tokio::time::timeout(DEADLINE, received.recv())
        .await
        .with_context(|| {
            format!(
                "waiting for replacement query result; graph: {:?}",
                core.inspect_computation_graph()
                    .map(|graph| graph.snapshot())
            )
        })?
        .context("updated result")?;
    assert!(matches!(
        &event.results[..],
        [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &json!({"value":22})
    ));
    assert_eq!(
        core.get_query_results("items").await?,
        vec![json!({"value":22})]
    );

    assert!(
        core.remove_query("items").await.is_err(),
        "live dependents must protect their query"
    );
    assert_eq!(
        core.get_query_status("items").await?,
        ComponentStatus::Running
    );
    core.remove_reaction("updated-capture", false).await?;
    core.remove_query("items").await?;
    assert!(core.get_query_config("items").await.is_err());
    assert!(core.computation_component("items").is_err());
    assert_eq!(core.computation_info().await?.id, root);
    assert_eq!(
        core.get_query_status("sibling").await?,
        ComponentStatus::Running
    );
    left_input
        .send_node_insert(
            "sibling-after-removal",
            vec!["Item"],
            PropertyMapBuilder::new().with_integer("value", 13).build(),
        )
        .await?;
    let event = tokio::time::timeout(DEADLINE, sibling_received.recv())
        .await?
        .context("sibling after removal")?;
    assert!(matches!(
        &event.results[..],
        [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &json!({"value":13})
    ));
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_query_mutations_preserve_dependents_and_rewire_sources() -> Result<()> {
    for strict in [false, true] {
        tokio::time::timeout(Duration::from_secs(30), query_mutations(false, strict)).await??;
    }
    Ok(())
}

#[tokio::test]
async fn native_query_mutations_preserve_dependents_and_rewire_sources() -> Result<()> {
    for strict in [false, true] {
        tokio::time::timeout(Duration::from_secs(30), query_mutations(true, strict)).await??;
    }
    Ok(())
}

#[tokio::test]
async fn reaction_updates_accept_native_queries() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (source, input) = ApplicationSource::new("input", source_config())?;
        let core = DrasiLib::builder().with_source(source).build().await?;
        core.computation_component("input")?.wait_created().await?;
        let ordinary = Query::cypher("ordinary")
            .query("MATCH (n:Item) RETURN n.value AS value")
            .from_source("input")
            .enable_bootstrap(false)
            .build();
        core.add_query(ordinary.clone()).await?;
        let mut native = ordinary;
        native.id = "native".into();
        let batch = core.computation_pipeline()?
            .source(core.borrow_computation_source("input").await?, SourceSubscriptionOptions::default())?
            .query(native).build()?;
        core.add_components(batch).await?;
        let (reaction, output) = ApplicationReaction::new("capture", vec!["ordinary".into()]);
        let _old_receiver = output.take_receiver().await.context("original receiver")?;
        core.add_reaction(reaction).await?;
        core.start().await?;
        tokio::time::timeout(DEADLINE, core.computation_component("capture")?.wait_started()).await??;

        let (replacement, output) = ApplicationReaction::new("capture", vec!["native".into()]);
        let mut received = output.take_receiver().await.context("replacement receiver")?;
        core.update_reaction("capture", replacement).await?;
        input.send_node_insert("one", vec!["Item"], PropertyMapBuilder::new().with_integer("value", 9).build()).await?;
        let event = tokio::time::timeout(DEADLINE, received.recv()).await?.context("native result")?;
        assert_eq!(event.query_id, "native");
        assert!(matches!(&event.results[..], [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &json!({"value":9})));
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn native_query_removal_clears_persistent_state_before_same_id_recreation() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(40), async {
        let directory = tempfile::tempdir()?;
        let provider = std::sync::Arc::new(drasi_index_rocksdb::RocksDbIndexProvider::new(
            directory.path(),
            false,
            false,
        ));
        let (source, input) = ApplicationSource::new("input", source_config())?;
        let core = DrasiLib::builder()
            .with_id("native-removal-storage")
            .with_source(source)
            .with_index_provider("rocks", provider)
            .build()
            .await?;
        core.computation_component("input")?.wait_created().await?;
        core.start().await?;
        for cycle in 0..2 {
            let config = Query::cypher("stored")
                .query("MATCH (n:Item) RETURN n.value AS value")
                .from_source("input")
                .enable_bootstrap(false)
                .with_storage_backend(drasi_lib::StorageBackendRef::Named("rocks".into()))
                .build();
            let batch = core
                .computation_pipeline()?
                .source(
                    core.borrow_computation_source("input").await?,
                    SourceSubscriptionOptions::default(),
                )?
                .query(config)
                .build()?;
            assert!(core.add_components(batch).await?.committed);
            tokio::time::timeout(
                DEADLINE,
                core.computation_component("stored")?.wait_started(),
            )
            .await??;
            assert!(
                core.get_query_results("stored").await?.is_empty(),
                "a removed query must not recover old rows"
            );
            let query = core
                .query_manager()
                .get_query_instance("stored")
                .await
                .map_err(anyhow::Error::msg)?;
            assert_eq!(query.fetch_snapshot().await?.as_of_sequence, 0);
            let mut subscription = query.subscribe(format!("cycle-{cycle}")).await?;
            input
                .send_node_insert(
                    format!("row-{cycle}"),
                    vec!["Item"],
                    PropertyMapBuilder::new()
                        .with_integer("value", cycle)
                        .build(),
                )
                .await?;
            let event = tokio::time::timeout(DEADLINE, subscription.receiver.recv()).await??;
            assert_eq!(event.sequence, 1);
            assert_eq!(
                core.get_query_results("stored").await?,
                vec![json!({"value":cycle})]
            );
            core.stop_query("stored").await?;
            core.remove_query("stored").await?;
            assert!(query.fetch_snapshot().await.is_err());
            assert!(core.computation_component("stored").is_err());
        }
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn source_replacement_rewires_native_query_subscriptions() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (source, input) = ApplicationSource::new("input", source_config())?;
        let core = DrasiLib::builder().with_id("native-source-replacement")
            .with_source(source).build().await?;
        core.computation_component("input")?.wait_created().await?;
        let config = Query::cypher("items").query("MATCH (n:Item) RETURN n.value AS value")
            .from_source("input").enable_bootstrap(false).build();
        let mut sibling_config = config.clone();
        sibling_config.id = "sibling".into();
        let batch = core.computation_pipeline()?
            .source(core.borrow_computation_source("input").await?, SourceSubscriptionOptions::default())?
            .query(config).query(sibling_config).build()?;
        core.add_components(batch).await?;
        core.start().await?;
        let query = core.query_manager().get_query_instance("items").await.map_err(anyhow::Error::msg)?;
        let mut subscription = query.subscribe("original".into()).await?;
        let sibling = core.query_manager().get_query_instance("sibling").await.map_err(anyhow::Error::msg)?;
        let mut sibling_subscription = sibling.subscribe("original-sibling".into()).await?;
        input.send_node_insert("before", vec!["Item"], PropertyMapBuilder::new().with_integer("value", 1).build()).await?;
        tokio::time::timeout(DEADLINE, subscription.receiver.recv()).await??;
        tokio::time::timeout(DEADLINE, sibling_subscription.receiver.recv()).await??;
        let (replacement, replacement_input) = ApplicationSource::new("input", source_config())?;
        core.update_source("input", replacement).await?;
        assert_eq!(core.get_source_status("input").await?, ComponentStatus::Running);
        assert_eq!(core.get_query_status("items").await?, ComponentStatus::Running);
        let updated = core.query_manager().get_query_instance("items").await.map_err(anyhow::Error::msg)?;
        let mut subscription = updated.subscribe("replacement".into()).await?;
        let updated_sibling = core.query_manager().get_query_instance("sibling").await.map_err(anyhow::Error::msg)?;
        let mut sibling_subscription = updated_sibling.subscribe("replacement-sibling".into()).await?;
        replacement_input.send_node_insert("after", vec!["Item"], PropertyMapBuilder::new().with_integer("value", 2).build()).await?;
        let event = tokio::time::timeout(DEADLINE, subscription.receiver.recv()).await.context("updated source must reach its native query")??;
        assert!(matches!(&event.results[..], [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &json!({"value":2})));
        assert!(query.fetch_snapshot().await.is_err(), "source replacement must fence the retired query reader");
        let sibling_event = tokio::time::timeout(DEADLINE, sibling_subscription.receiver.recv()).await??;
        assert!(matches!(&sibling_event.results[..], [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &json!({"value":2})));
        assert!(sibling.fetch_snapshot().await.is_err());
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn native_query_replacement_preserves_scheduled_delivery_identity() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (source, input) = ApplicationSource::new("input", source_config())?;
        let core = DrasiLib::builder().with_id("native-timer-replacement").with_source(source).build().await?;
        core.computation_component("input")?.wait_created().await?;
        let mut config = Query::cypher("items")
            .query("MATCH (n:Item) WHERE drasi.trueFor(n.value > 0, duration({ milliseconds: 50 })) RETURN n.value AS value")
            .from_source("input").enable_bootstrap(false).build();
        let batch = core.computation_pipeline()?
            .source(core.borrow_computation_source("input").await?, SourceSubscriptionOptions::default())?
            .query(config.clone()).build()?;
        core.add_components(batch).await?;
        core.start().await?;
        for cycle in 1..=2 {
            if cycle == 2 {
                config.query = "MATCH (n:Item) WHERE drasi.trueFor(n.value > 0, duration({ milliseconds: 50 })) RETURN n.value * 2 AS value".into();
                core.update_query("items", config.clone()).await?;
            }
            let query = core.query_manager().get_query_instance("items").await.map_err(anyhow::Error::msg)?;
            let mut subscription = query.subscribe(format!("cycle-{cycle}")).await?;
            input.send_node_insert(format!("row-{cycle}"), vec!["Item"], PropertyMapBuilder::new().with_integer("value", cycle).build()).await?;
            let event = tokio::time::timeout(DEADLINE, subscription.receiver.recv()).await??;
            assert!(matches!(&event.results[..], [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &json!({"value":cycle * cycle})));
        }
        assert_eq!(core.get_query_status("items").await?, ComponentStatus::Running);
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn failed_native_query_declaration_can_be_removed_without_activation() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let (source, _) = ApplicationSource::new("input", source_config())?;
        let core = DrasiLib::builder().with_source(source).build().await?;
        core.computation_component("input")?.wait_created().await?;
        let mut config = Query::cypher("invalid")
            .query("MATCH (n:Item) RETURN n.value AS value")
            .from_source("input")
            .enable_bootstrap(false)
            .auto_start(false)
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
        let invalid = batch
            .definition
            .components
            .iter_mut()
            .find(|node| node.descriptor.id().as_str() == "invalid")
            .context("invalid query declaration")?;
        let ComponentConstruction::Factory(specification) = &mut invalid.construction else {
            anyhow::bail!("expected query factory");
        };
        specification.configuration.insert(
            "query".into(),
            ConfigurationValue::Literal(json!(config.query)),
        );
        specification.configuration.insert(
            "query_config".into(),
            ConfigurationValue::Literal(serde_json::to_value(config)?),
        );
        assert!(core.add_components(batch).await?.committed);
        assert!(core
            .computation_component("invalid")?
            .wait_created()
            .await
            .is_err());
        assert_eq!(
            core.get_query_status("invalid").await?,
            ComponentStatus::Error
        );
        core.remove_query("invalid").await?;
        assert!(core.list_queries().await?.is_empty());
        assert!(core.computation_component("input").is_ok());
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn native_persistent_query_replacement_releases_storage_and_retains_new_rows() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(40), async {
        let directory = tempfile::tempdir()?;
        let provider = std::sync::Arc::new(drasi_index_rocksdb::RocksDbIndexProvider::new(
            directory.path(), false, false,
        ));
        let (source, input) = ApplicationSource::new("input", ApplicationSourceConfig {
            properties: Default::default(),
            durability: Some(drasi_lib::DurabilityConfig {
                enabled: true, max_events: 128, capacity_policy: drasi_lib::CapacityPolicy::RejectIncoming,
            }),
        })?;
        let core = DrasiLib::builder().with_id("native-storage-replacement").with_source(source)
            .with_wal_provider(std::sync::Arc::new(drasi_wal_redb::RedbWalProvider::new(directory.path().join("wal"))))
            .with_index_provider("rocks", provider).build().await?;
        core.computation_component("input")?.wait_created().await?;
        let mut config = Query::cypher("stored").query("MATCH (n:Item) RETURN n.value AS value")
            .from_source("input").enable_bootstrap(false)
            .with_storage_backend(drasi_lib::StorageBackendRef::Named("rocks".into())).build();
        let batch = core.computation_pipeline()?
            .source(core.borrow_computation_source("input").await?, SourceSubscriptionOptions { borrowed_recovery: true, ..Default::default() })?
            .query(config.clone()).build()?;
        core.add_components(batch).await?;
        core.start().await?;
        for cycle in 1..=2 {
            if cycle == 2 {
                config.query = "MATCH (n:Item) RETURN n.value * 2 AS value".into();
                core.update_query("stored", config.clone()).await?;
                assert!(core.get_query_results("stored").await?.is_empty());
            }
            let query = core.query_manager().get_query_instance("stored").await.map_err(anyhow::Error::msg)?;
            let mut subscription = query.subscribe(format!("cycle-{cycle}")).await?;
            input.send_node_insert(format!("row-{cycle}"), vec!["Item"], PropertyMapBuilder::new().with_integer("value", cycle).build()).await?;
            let event = tokio::time::timeout(DEADLINE, subscription.receiver.recv()).await??;
            assert!(matches!(&event.results[..], [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &json!({"value":cycle * cycle})));
        }
        core.stop_query("stored").await?;
        core.start_query("stored").await?;
        assert_eq!(core.get_query_results("stored").await?, vec![json!({"value":4})]);
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}

struct RemovalProbe {
    descriptor: ComponentDescriptor,
    allow_cleanup: std::sync::Arc<std::sync::atomic::AtomicBool>,
    attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl ComputationComponent for RemovalProbe {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
    async fn deprovision(&mut self) -> Result<()> {
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        anyhow::ensure!(
            self.allow_cleanup
                .load(std::sync::atomic::Ordering::Acquire),
            "injected state cleanup failure"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl Transformer for RemovalProbe {
    async fn transform(&mut self, _: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        anyhow::bail!("removal probe must not receive data")
    }
}

#[tokio::test]
async fn failed_native_query_cleanup_keeps_its_node_and_retries_explicitly() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let (source, _) = ApplicationSource::new("input", source_config())?;
        let core = DrasiLib::builder().with_source(source).build().await?;
        core.computation_component("input")?.wait_created().await?;
        let allow_cleanup = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = Query::cypher("cleanup")
            .query("MATCH (n) RETURN n")
            .from_source("input")
            .enable_bootstrap(false)
            .auto_start(false)
            .build();
        let mut batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source("input").await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(config)
            .build()?
            .auto_start(false);
        let component = batch
            .definition
            .components
            .iter_mut()
            .find(|node| node.descriptor.id().as_str() == "cleanup")
            .context("cleanup query")?;
        component.construction = ComponentConstruction::External {
            binding: "cleanup-probe".into(),
        };
        batch.bindings.components.insert(
            "cleanup-probe".into(),
            ConstructedComponent::query(Box::new(RemovalProbe {
                descriptor: component.descriptor.clone(),
                allow_cleanup: allow_cleanup.clone(),
                attempts: attempts.clone(),
            })),
        );
        assert!(core.add_components(batch).await?.committed);
        let query = core.computation_component("cleanup")?;
        query.wait_created().await?;
        let error = core
            .remove_query("cleanup")
            .await
            .expect_err("failed cleanup must reject removal");
        assert!(
            error.to_string().contains("injected state cleanup failure"),
            "{error}"
        );
        assert!(core.computation_component("cleanup").is_ok());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Acquire), 1);
        assert!(query.observed()?.failure.is_some());
        allow_cleanup.store(true, std::sync::atomic::Ordering::Release);
        core.remove_query("cleanup").await?;
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Acquire), 2);
        assert!(core.computation_component("cleanup").is_err());
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
