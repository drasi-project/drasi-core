// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_lib::{
    computation::v1::*, config::ComponentReference, ComponentStatus, DrasiLib, Query, Source,
};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{oneshot, Notify};

fn name(value: &str) -> ComponentName {
    ComponentName::try_new(value).unwrap()
}
fn source(id: ComponentId) -> anyhow::Result<ApplicationSource> {
    Ok(ApplicationSource::new(
        id.to_string(),
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )?
    .0)
}
fn metadata(kind: &str) -> HashMap<String, String> {
    HashMap::from([
        ("pluginId".into(), format!("test/{kind}")),
        ("pluginVersion".into(), "1".into()),
        ("factoryNote".into(), "preserved".into()),
    ])
}

#[tokio::test]
async fn async_source_is_owned_before_await_and_preserves_returned_metadata() -> anyhow::Result<()>
{
    let core = DrasiLib::builder().build().await?;
    let entered = Arc::new(Notify::new());
    let notified = entered.clone();
    let (release, wait) = oneshot::channel();
    let assigned = Arc::new(Mutex::new(None));
    let captured = assigned.clone();
    let handle = core
        .create_source_async("orders", None, false, move |id| async move {
            *captured.lock().unwrap() = Some(id.clone());
            notified.notify_one();
            wait.await?;
            Ok((Box::new(source(id)?) as Box<dyn Source>, metadata("source")))
        })
        .await?;
    entered.notified().await;
    assert_eq!(assigned.lock().unwrap().as_ref(), Some(handle.id()));
    assert_eq!(core.component_by_name("orders")?.id(), handle.id());
    assert_eq!(handle.observed()?.realization, RealizationState::Creating);
    let unavailable = core.snapshot_configuration().await.unwrap_err();
    assert_eq!(
        unavailable
            .downcast_ref::<drasi_lib::config::ComponentConfigurationUnavailable>()
            .unwrap()
            .component,
        *handle.id()
    );
    assert!(core.snapshot_computation_configuration().await.is_err());
    let waiter = tokio::spawn({
        let handle = handle.clone();
        async move { handle.wait_created().await }
    });
    tokio::task::yield_now().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle.wait_created()).await??;
    let graph = core.get_graph().await?;
    let node = graph.get_component(handle.id().as_str()).unwrap();
    assert_eq!(node.metadata["factoryNote"], "preserved");
    assert_eq!(node.metadata["pluginId"], "test/source");
    assert_eq!(
        core.get_source_info(handle.id().as_str())
            .await?
            .source_type,
        "application"
    );
    assert_eq!(
        core.snapshot_configuration().await?.sources[0].source_type,
        "application"
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn async_requested_ids_obey_policy_before_invoking_any_constructor() -> anyhow::Result<()> {
    let core = DrasiLib::builder().build().await?;
    let called = Arc::new(AtomicBool::new(false));
    let seen = called.clone();
    let id = ComponentId::generate();
    let error = core
        .create_source_async("external", Some(id.clone()), false, move |id| async move {
            seen.store(true, Ordering::SeqCst);
            Ok((source(id)?, HashMap::new()))
        })
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error.downcast_ref::<GraphError>(),
        Some(GraphError::ExternalComponentId { .. })
    ));
    let seen = called.clone();
    let error = core
        .create_reaction_async(
            "external",
            Some(id.clone()),
            vec![],
            false,
            move |id, inputs| async move {
                seen.store(true, Ordering::SeqCst);
                Ok((
                    drasi_reaction_application::ApplicationReaction::new(
                        id.to_string(),
                        inputs.iter().map(ToString::to_string).collect(),
                    )
                    .0,
                    HashMap::new(),
                ))
            },
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error.downcast_ref::<GraphError>(),
        Some(GraphError::ExternalComponentId { .. })
    ));
    assert!(!called.load(Ordering::SeqCst));
    assert!(core
        .computation_control()?
        .desired_snapshot()
        .nodes
        .is_empty());
    core.shutdown().await?;

    let core = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .build()
        .await?;
    let handle = core
        .create_source_async("chosen", Some(id.clone()), false, |id| async move {
            Ok((source(id)?, metadata("source")))
        })
        .await?;
    handle.wait_created().await?;
    assert_eq!(handle.id(), &id);
    let seen = called.clone();
    assert!(core
        .create_source_async("duplicate", Some(id.clone()), false, move |id| async move {
            seen.store(true, Ordering::SeqCst);
            Ok((source(id)?, HashMap::new()))
        })
        .await
        .is_err());
    assert!(!called.load(Ordering::SeqCst));
    core.shutdown().await?;
    Ok(())
}

struct ReturnedSource {
    id: String,
    disposed: Arc<AtomicUsize>,
}

#[async_trait]
impl Source for ReturnedSource {
    fn id(&self) -> &str {
        &self.id
    }
    fn type_name(&self) -> &str {
        "returned-source"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn bind_runtime_context(&self, _: drasi_lib::SourceRuntimeContext) {}
    async fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn dispose(&self) -> anyhow::Result<()> {
        self.disposed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        ComponentStatus::Stopped
    }
    async fn subscribe(
        &self,
        _: drasi_lib::SourceSubscriptionSettings,
    ) -> anyhow::Result<drasi_lib::SubscriptionResponse> {
        anyhow::bail!("fixture has no data")
    }
}

#[tokio::test]
async fn async_failures_reserve_identity_and_returned_bad_objects_are_cleaned_up(
) -> anyhow::Result<()> {
    let core = DrasiLib::builder().build().await?;
    let disposed = Arc::new(AtomicUsize::new(0));
    let cleanup = disposed.clone();
    let foreign = ComponentId::generate();
    let returned = foreign.to_string();
    let handle = core
        .create_source_async("bad", None, false, move |_| async move {
            Ok((
                ReturnedSource {
                    id: returned,
                    disposed: cleanup,
                },
                metadata("source"),
            ))
        })
        .await?;
    assert!(handle.wait_created().await.is_err());
    assert_eq!(
        handle.observed()?.realization,
        RealizationState::CreationFailed
    );
    assert_eq!(core.component_by_name("bad")?.id(), handle.id());
    assert!(core.computation_component(foreign.as_str()).is_err());
    core.remove_source(handle.id().as_str(), false).await?;
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
    let error = core
        .create_source_async("bad", Some(handle.id().clone()), false, |id| async move {
            Ok((source(id)?, HashMap::new()))
        })
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error.downcast_ref::<GraphError>(),
        Some(GraphError::ExternalComponentId { .. })
    ));

    let handle = core
        .create_source_async("failed", None, false, |_| async {
            Err::<(ApplicationSource, HashMap<String, String>), _>(anyhow::anyhow!(
                "factory failed"
            ))
        })
        .await?;
    assert!(format!("{:#}", handle.wait_created().await.unwrap_err()).contains("factory failed"));
    assert_eq!(core.component_by_name("failed")?.id(), handle.id());
    let cleanup = disposed.clone();
    let never_started = core
        .create_source_async("never-started", None, false, move |id| async move {
            Ok((
                ReturnedSource {
                    id: id.to_string(),
                    disposed: cleanup,
                },
                HashMap::new(),
            ))
        })
        .await?;
    never_started.wait_created().await?;
    core.shutdown().await?;
    assert_eq!(disposed.load(Ordering::SeqCst), 2);
    Ok(())
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn cancelled_source_request_keeps_accepted_query_dependency_resolution() -> anyhow::Result<()>
{
    let core = DrasiLib::builder()
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    let query = core
        .add_query_with_handle(
            Query::cypher("view")
                .query("MATCH (n) RETURN n")
                .from_input("late")
                .auto_start(false)
                .enable_bootstrap(false)
                .build()?,
        )
        .await?;
    let entered = Arc::new(Notify::new());
    let signal = entered.clone();
    let (release, wait) = oneshot::channel();
    let creation = tokio::spawn({
        let core = core.clone();
        async move {
            core.create_source_async("late", None, false, move |id| async move {
                signal.notify_one();
                wait.await?;
                Ok((source(id)?, metadata("source")))
            })
            .await
        }
    });
    entered.notified().await;
    let source = core.component_by_name("late")?;
    let mut changes = core.computation_control()?.subscribe_observed();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            changes.borrow_and_update();
            if core.get_query_config(query.id().as_str()).await?.inputs[0]
                .component
                .as_id()
                == Some(source.id())
            {
                return Ok::<_, anyhow::Error>(());
            }
            changes.changed().await?;
        }
    })
    .await??;
    creation.abort();
    let _ = creation.await;
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), source.wait_created()).await??;
    tokio::time::timeout(
        Duration::from_secs(5),
        core.computation_component(query.id().as_str())?
            .wait_created(),
    )
    .await??;
    assert_eq!(
        core.get_query_config(query.id().as_str()).await?.inputs[0]
            .component
            .as_id(),
        Some(source.id())
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shutdown_cancels_construction_without_waiting_for_a_source_requester() -> anyhow::Result<()>
{
    let core = DrasiLib::builder()
        .with_construction_timeout_secs(30)
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    core.add_query(
        Query::cypher("view")
            .query("MATCH (n) RETURN n")
            .from_input("late")
            .auto_start(false)
            .enable_bootstrap(false)
            .build()?,
    )
    .await?;
    let entered = Arc::new(Notify::new());
    let signal = entered.clone();
    let dropped = Arc::new(AtomicBool::new(false));
    let cleanup = dropped.clone();
    let creation = tokio::spawn({
        let core = core.clone();
        async move {
            core.create_source_async("late", None, false, move |id| async move {
                let _cleanup = Dropped(cleanup);
                signal.notify_one();
                std::future::pending::<()>().await;
                Ok((source(id)?, HashMap::new()))
            })
            .await
        }
    });
    entered.notified().await;
    tokio::time::timeout(Duration::from_secs(5), core.shutdown()).await??;
    assert!(dropped.load(Ordering::SeqCst));
    let _ = tokio::time::timeout(Duration::from_secs(5), creation).await?;
    Ok(())
}

#[tokio::test]
async fn stopping_an_async_constructor_cancels_the_owned_future_not_its_identity(
) -> anyhow::Result<()> {
    let core = DrasiLib::builder().build().await?;
    let dropped = Arc::new(AtomicBool::new(false));
    let cleanup = dropped.clone();
    let entered = Arc::new(Notify::new());
    let signal = entered.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let handle = core
        .create_source_async("pending", None, false, move |id| async move {
            let _owned = Dropped(cleanup);
            count.fetch_add(1, Ordering::SeqCst);
            signal.notify_one();
            std::future::pending::<()>().await;
            Ok((source(id)?, HashMap::new()))
        })
        .await?;
    entered.notified().await;
    tokio::time::timeout(Duration::from_secs(5), handle.stop()).await??;
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(core.component_by_name("pending")?.id(), handle.id());
    assert!(handle.start().await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn async_constructor_uses_the_graph_construction_deadline() -> anyhow::Result<()> {
    let core = DrasiLib::builder()
        .with_construction_timeout_secs(1)
        .build()
        .await?;
    let dropped = Arc::new(AtomicBool::new(false));
    let cleanup = dropped.clone();
    let handle = core
        .create_source_async("timeout", None, false, move |id| async move {
            let _owned = Dropped(cleanup);
            std::future::pending::<()>().await;
            Ok((source(id)?, HashMap::new()))
        })
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), handle.wait_created())
            .await?
            .is_err()
    );
    assert!(dropped.load(Ordering::SeqCst));
    assert!(handle.observed()?.failure.is_some());
    assert_eq!(core.component_by_name("timeout")?.id(), handle.id());
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn async_reaction_rejects_wrong_kinds_and_substituted_input_ids() -> anyhow::Result<()> {
    let core = DrasiLib::builder()
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    let source = core.create_source("source", true, source).await?;
    source.wait_created().await?;
    let called = Arc::new(AtomicBool::new(false));
    let captured = called.clone();
    let wrong_kind = core
        .create_reaction_async(
            "wrong-kind",
            None,
            vec![ComponentReference::by_id(source.id().clone())],
            false,
            move |id, inputs| async move {
                captured.store(true, Ordering::SeqCst);
                Ok((
                    drasi_reaction_application::ApplicationReaction::new(
                        id.to_string(),
                        inputs.iter().map(ToString::to_string).collect(),
                    )
                    .0,
                    HashMap::new(),
                ))
            },
        )
        .await
        .err()
        .expect("known non-query input must fail before acceptance");
    assert!(format!("{wrong_kind:#}").contains("is not a query"));
    assert!(core.component_by_name("wrong-kind").is_err());
    assert!(!called.load(Ordering::SeqCst));
    let query = core
        .add_query_with_handle(
            Query::cypher("view")
                .query("MATCH (n) RETURN n")
                .auto_start(false)
                .enable_bootstrap(false)
                .build()?,
        )
        .await?;
    query.wait_created().await?;
    let bad_inputs = core
        .create_reaction_async(
            "wrong-inputs",
            None,
            vec![ComponentReference::by_name("view")?],
            false,
            |id, _| async move {
                Ok((
                    drasi_reaction_application::ApplicationReaction::new(id.to_string(), vec![]).0,
                    metadata("reaction"),
                ))
            },
        )
        .await?;
    let error = bad_inputs.wait_created().await.unwrap_err();
    assert!(format!("{error:#}").contains("substituted its UUID input bindings"));
    assert_eq!(
        core.get_reaction_info(bad_inputs.id().as_str())
            .await?
            .queries[0]
            .as_id(),
        Some(query.id())
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn async_reaction_binds_names_before_constructor_and_never_retargets() -> anyhow::Result<()> {
    let core = DrasiLib::builder()
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    let entered = Arc::new(Notify::new());
    let signal = entered.clone();
    let ids = Arc::new(Mutex::new(Vec::new()));
    let captured = ids.clone();
    let (release, wait) = oneshot::channel();
    let handle = core
        .create_reaction_async(
            "consumer",
            None,
            vec![ComponentReference::by_name("view")?],
            false,
            move |id, inputs| async move {
                *captured.lock().unwrap() = inputs.clone();
                signal.notify_one();
                wait.await?;
                Ok((
                    Box::new(
                        drasi_reaction_application::ApplicationReaction::new(
                            id.to_string(),
                            inputs.iter().map(ToString::to_string).collect(),
                        )
                        .0,
                    ) as Box<dyn drasi_lib::Reaction>,
                    metadata("reaction"),
                ))
            },
        )
        .await?;
    assert!(handle
        .observed()?
        .waiting_for
        .iter()
        .any(|wait| matches!(wait, DependencyWait::ComponentName { .. })));
    assert!(ids.lock().unwrap().is_empty());
    let query = core
        .add_query_with_handle(
            Query::cypher("view")
                .query("MATCH (n) RETURN n")
                .auto_start(false)
                .enable_bootstrap(false)
                .build()?,
        )
        .await?;
    entered.notified().await;
    core.rename_component(query.id().as_str(), "renamed")
        .await?;
    let replacement = core
        .add_query_with_handle(
            Query::cypher("view")
                .query("MATCH (n) RETURN n")
                .auto_start(false)
                .enable_bootstrap(false)
                .build()?,
        )
        .await?;
    release.send(()).unwrap();
    handle.wait_created().await?;
    assert_ne!(replacement.id(), query.id());
    assert_eq!(ids.lock().unwrap().as_slice(), &[query.id().clone()]);
    assert_eq!(
        core.get_reaction_info(handle.id().as_str()).await?.queries[0].as_id(),
        Some(query.id())
    );
    assert_eq!(
        core.get_graph()
            .await?
            .get_component(handle.id().as_str())
            .unwrap()
            .metadata["factoryNote"],
        "preserved"
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn builder_async_factories_deliver_through_an_ordinary_query() -> anyhow::Result<()> {
    let input = Arc::new(Mutex::new(None));
    let source_handle = input.clone();
    let output = Arc::new(Mutex::new(None));
    let reaction_handle = output.clone();
    let entered = Arc::new(Notify::new());
    let signal = entered.clone();
    let (release, wait) = oneshot::channel();
    let builder = DrasiLib::builder()
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .with_source_factory_async("orders", None, true, move |id| async move {
            signal.notify_one();
            wait.await?;
            let (source, handle) = ApplicationSource::new(
                id.to_string(),
                ApplicationSourceConfig {
                    properties: HashMap::new(),
                    durability: None,
                },
            )?;
            *source_handle.lock().unwrap() = Some(handle);
            Ok((source, metadata("source")))
        })
        .with_query(
            Query::cypher("view")
                .query("MATCH (n:Item) RETURN n.value AS value")
                .from_input("orders")
                .enable_bootstrap(false)
                .build()?,
        )
        .with_reaction_inputs_factory_async(
            "consumer",
            None,
            vec![ComponentReference::by_name("view")?],
            true,
            move |id, inputs| async move {
                tokio::task::yield_now().await;
                let (reaction, handle) = drasi_reaction_application::ApplicationReaction::new(
                    id.to_string(),
                    inputs.iter().map(ToString::to_string).collect(),
                );
                *reaction_handle.lock().unwrap() = Some(handle);
                Ok((reaction, metadata("reaction")))
            },
        );
    let build = tokio::spawn(builder.build());
    entered.notified().await;
    tokio::task::yield_now().await;
    assert!(!build.is_finished());
    release.send(()).unwrap();
    let core = tokio::time::timeout(Duration::from_secs(10), build).await???;
    core.start().await?;
    let consumer = core.component_by_name("consumer")?;
    consumer.wait_started().await?;
    let output = output.lock().unwrap().take().unwrap();
    let mut results = output.take_receiver().await.unwrap();
    let input = input.lock().unwrap().take().unwrap();
    input
        .send_node_insert(
            "row",
            vec!["Item"],
            PropertyMapBuilder::new().with_integer("value", 7).build(),
        )
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(5), results.recv())
        .await?
        .unwrap();
    assert_eq!(
        result.query_id,
        core.component_by_name("view")?.id().as_str()
    );
    assert_eq!(
        core.get_graph()
            .await?
            .get_component(consumer.id().as_str())
            .unwrap()
            .metadata["pluginId"],
        "test/reaction"
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn async_reaction_recreation_requires_removal_and_a_new_identity() -> anyhow::Result<()> {
    let core = DrasiLib::builder()
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    let query = core
        .add_query_with_handle(
            Query::cypher("view")
                .query("MATCH (n) RETURN n")
                .auto_start(false)
                .enable_bootstrap(false)
                .build()?,
        )
        .await?;
    query.wait_created().await?;
    let old = core
        .create_reaction_async(
            "consumer",
            None,
            vec![ComponentReference::by_id(query.id().clone())],
            false,
            |id, inputs| async move {
                Ok((
                    drasi_reaction_application::ApplicationReaction::new(
                        id.to_string(),
                        inputs.iter().map(ToString::to_string).collect(),
                    )
                    .0,
                    metadata("reaction"),
                ))
            },
        )
        .await?;
    old.wait_created().await?;
    old.rename(name("kept-name")).await?;
    assert!(
        core.update_reaction(
            old.id().as_str(),
            drasi_lib::management::OrdinaryComponentRecord {
                id: old.id().clone(),
                name: name("kept-name"),
                auto_start: false,
                component: drasi_lib::management::OrdinaryComponent::Reaction {
                    kind: "application".into(),
                    configuration: serde_json::json!({}),
                    inputs: vec![ComponentReference::by_name("later")?],
                },
            }
        )
        .await
        .is_err(),
        "creation callbacks do not provide managed update capability"
    );
    core.remove_reaction(old.id().as_str(), false).await?;
    let called = Arc::new(AtomicBool::new(false));
    let captured = called.clone();
    let replacement = tokio::time::timeout(
        Duration::from_secs(5),
        core.create_reaction_async(
            "kept-name",
            None,
            vec![ComponentReference::by_name("later")?],
            false,
            move |id, inputs| async move {
                captured.store(true, Ordering::SeqCst);
                let mut metadata = metadata("reaction");
                metadata.insert("pluginVersion".into(), "2".into());
                metadata.insert("factoryNote".into(), "replacement".into());
                Ok((
                    drasi_reaction_application::ApplicationReaction::new(
                        id.to_string(),
                        inputs.iter().map(ToString::to_string).collect(),
                    )
                    .0,
                    metadata,
                ))
            },
        ),
    )
    .await??;
    assert_ne!(replacement.id(), old.id());
    assert_ne!(replacement.generation(), old.generation());
    assert!(old.observed().is_err());
    assert_eq!(core.component_by_name("kept-name")?.id(), replacement.id());
    assert!(!called.load(Ordering::SeqCst));
    assert!(replacement
        .observed()?
        .waiting_for
        .iter()
        .any(|wait| matches!(wait,
        DependencyWait::ComponentName { name } if name.as_str() == "later")));
    assert!(core
        .snapshot_configuration()
        .await
        .unwrap_err()
        .downcast_ref::<drasi_lib::config::ComponentConfigurationUnavailable>()
        .is_some());
    let later = core
        .add_query_with_handle(
            Query::cypher("later")
                .query("MATCH (n) RETURN n")
                .auto_start(false)
                .enable_bootstrap(false)
                .build()?,
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(5), replacement.wait_created()).await??;
    assert!(called.load(Ordering::SeqCst));
    let snapshot = core.snapshot_configuration().await?;
    assert_eq!(snapshot.reactions[0].id, replacement.id().as_str());
    assert_eq!(snapshot.reactions[0].name, "kept-name");
    assert_ne!(snapshot.reactions[0].reaction_type, "deferred-reaction");
    assert_eq!(snapshot.reactions[0].queries[0].as_id(), Some(later.id()));
    let graph = core.get_graph().await?;
    let node = graph.get_component(replacement.id().as_str()).unwrap();
    assert_eq!(node.metadata["factoryNote"], "replacement");
    assert_eq!(node.metadata["pluginVersion"], "2");
    assert!(!core.allow_external_component_ids());
    let missing = ComponentId::generate();
    assert!(core
        .update_reaction(
            missing.as_str(),
            drasi_lib::management::OrdinaryComponentRecord {
                id: missing.clone(),
                name: name("missing"),
                auto_start: false,
                component: drasi_lib::management::OrdinaryComponent::Reaction {
                    kind: "application".into(),
                    configuration: serde_json::json!({}),
                    inputs: vec![],
                },
            },
        )
        .await
        .is_err());
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn recreated_async_member_owns_construction_and_later_stop_supersedes_activation(
) -> anyhow::Result<()> {
    let core = DrasiLib::builder()
        .with_construction_timeout_secs(30)
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    let query = core
        .add_query_with_handle(
            Query::cypher("view")
                .query("MATCH (n) RETURN n")
                .enable_bootstrap(false)
                .build()?,
        )
        .await?;
    let old = core
        .create_reaction_async(
            "consumer",
            None,
            vec![ComponentReference::by_id(query.id().clone())],
            true,
            |id, inputs| async move {
                Ok((
                    drasi_reaction_application::ApplicationReaction::new(
                        id.to_string(),
                        inputs.iter().map(ToString::to_string).collect(),
                    )
                    .0,
                    metadata("reaction"),
                ))
            },
        )
        .await?;
    core.start().await?;
    old.wait_started().await?;
    core.remove_reaction(old.id().as_str(), false).await?;
    let entered = Arc::new(Notify::new());
    let signal = entered.clone();
    let (release, wait) = oneshot::channel();
    let update = tokio::spawn({
        let core = core.clone();
        let input = query.id().clone();
        async move {
            let created = core
                .create_reaction_async(
                    "consumer",
                    None,
                    vec![ComponentReference::by_id(input)],
                    true,
                    move |id, inputs| async move {
                        signal.notify_one();
                        wait.await?;
                        Ok((
                            drasi_reaction_application::ApplicationReaction::new(
                                id.to_string(),
                                inputs.iter().map(ToString::to_string).collect(),
                            )
                            .0,
                            metadata("reaction"),
                        ))
                    },
                )
                .await?;
            created.wait_created().await?;
            Ok::<_, anyhow::Error>(created)
        }
    });
    entered.notified().await;
    let current = core.component_by_name("consumer")?;
    assert_ne!(old.id(), current.id());
    assert_ne!(old.generation(), current.generation());
    update.abort();
    let _ = update.await;
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), current.wait_started()).await??;
    core.remove_reaction(current.id().as_str(), false).await?;
    let pending = core
        .create_reaction_async(
            "consumer",
            None,
            vec![ComponentReference::by_name("later")?],
            true,
            |id, inputs| async move {
                Ok((
                    drasi_reaction_application::ApplicationReaction::new(
                        id.to_string(),
                        inputs.iter().map(ToString::to_string).collect(),
                    )
                    .0,
                    metadata("reaction"),
                ))
            },
        )
        .await?;
    core.stop().await?;
    core.add_query(
        Query::cypher("later")
            .query("MATCH (n) RETURN n")
            .enable_bootstrap(false)
            .build()?,
    )
    .await?;
    assert!(!core.is_running().await);
    assert_ne!(pending.observed()?.lifecycle, ComponentLifecycle::Running);
    core.start().await?;
    tokio::time::timeout(Duration::from_secs(5), pending.wait_started()).await??;
    assert_ne!(pending.id(), old.id());
    assert_ne!(pending.id(), current.id());
    core.shutdown().await?;
    Ok(())
}

struct Service(ComponentDescriptor);
#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl ComputationService for Service {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

struct ServiceFactory(FactoryDescriptor);
impl ServiceFactory {
    fn new() -> Arc<Self> {
        Arc::new(Self(FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("fixture/deferred-service", "1")
                .unwrap(),
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: ConfigurationSchema {
                fields: Default::default(),
                allow_additional: false,
            },
            dependencies: Default::default(),
        }))
    }
    fn spec(&self, id: ComponentId) -> GraphResult<ComponentSpecification> {
        Ok(ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id, vec![])?,
            role: ComponentRole::Service,
            completion: None,
            implementation: self.0.implementation.clone(),
            configuration_version: 1,
            configuration: Default::default(),
            dependencies: Default::default(),
        })
    }
}
#[async_trait]
impl ComponentFactory for ServiceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        Ok(ConstructedComponent::service(Box::new(Service(
            context.specification.descriptor.clone(),
        ))))
    }
}

#[tokio::test]
async fn scoped_pipeline_authorizes_only_helpers_of_fresh_owned_components() -> anyhow::Result<()> {
    let core = DrasiLib::builder()
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    let source = core.create_source("orders", true, source).await?;
    source.wait_created().await?;
    let host = core.borrow_computation_source(source.id().as_str()).await?;
    let pipeline = core.computation_pipeline()?.source(
        host,
        SourceSubscriptionOptions {
            borrowed_recovery: true,
            allow_broadcast_loss: true,
            ..Default::default()
        },
    )?;
    let query = Query::cypher("native-view")
        .query("MATCH (n) RETURN n")
        .with_input(drasi_lib::QueryInputConfig::by_id(source.id().clone()))
        .enable_bootstrap(false)
        .auto_start(false)
        .build()?;
    let report = core
        .create_components(move |ids| {
            let id = ids.allocate()?;
            pipeline.query(query.with_assigned_id(id)).build_owned(ids)
        })
        .await?;
    let owner = report
        .creation
        .as_ref()
        .unwrap()
        .components
        .iter()
        .find(|component| component.name.as_str() == "native-view")
        .unwrap()
        .id
        .clone();
    core.reconcile_desired_state().await?;
    core.computation_component(owner.as_str())?
        .wait_created()
        .await?;
    let graph = core.computation_control()?.desired_snapshot();
    for helper in [
        ComponentId::derive_owned(&owner, "query-results")?,
        ComponentId::derive_owned(&owner, "query-scheduled")?,
        drasi_computation_contracts::query_source_component_id(
            owner.as_str(),
            source.id().as_str(),
        )?,
    ] {
        assert!(graph.component(&helper).is_some());
    }
    assert!(!core.allow_external_component_ids());
    let before_update = core.computation_component(owner.as_str())?.generation();
    let extra = core
        .create_source("extra", false, |id| {
            Ok(ApplicationSource::new(
                id.to_string(),
                ApplicationSourceConfig {
                    properties: HashMap::new(),
                    durability: None,
                },
            )?
            .0)
        })
        .await?;
    extra.wait_created().await?;
    let mut update = core
        .get_query_config(owner.as_str())
        .await?
        .into_authoring()?;
    update.query = "MATCH (n) RETURN n AS updated".into();
    update
        .inputs
        .push(drasi_lib::QueryInputConfig::by_id(extra.id().clone()));
    let accepted_before = core.desired_configuration()?;
    assert!(
        core.update_query(owner.as_str(), update).await.is_err(),
        "creation-only factories cannot authorize an implicit query rebuild"
    );
    assert_eq!(core.desired_configuration()?, accepted_before);
    let current = core.computation_component(owner.as_str())?;
    current.wait_created().await?;
    assert_eq!(before_update, current.generation());
    assert_eq!(current.id(), &owner);
    let added_helper = drasi_computation_contracts::query_source_component_id(
        owner.as_str(),
        extra.id().as_str(),
    )?;
    assert!(!core
        .desired_configuration()?
        .desired
        .topology
        .components
        .iter()
        .any(|node| node.descriptor.id() == &added_helper));
    core.remove_query(owner.as_str()).await?;
    assert!(core.computation_component(owner.as_str()).is_err());
    let refused = core
        .create_components(move |ids| {
            let helper = ids.derive_owned(&owner, "reused-helper")?;
            ComponentBatch::builder()
                .service(
                    name("bad"),
                    Box::new(Service(ComponentDescriptor::try_new(helper, vec![])?)),
                )
                .build()
        })
        .await;
    assert!(refused.is_err());
    let external = ComponentId::generate();
    assert!(core
        .create_components(move |ids| {
            ids.allocate()?;
            let helper = ids.derive_owned(&external, "forged-helper")?;
            ComponentBatch::builder()
                .service(
                    name("bad"),
                    Box::new(Service(ComponentDescriptor::try_new(helper, vec![])?)),
                )
                .build()
        })
        .await
        .is_err());
    let factory = ServiceFactory::new();
    core.create_components(move |ids| {
        let owner = ids.allocate()?;
        let helper = ids.derive_owned(&owner, "helper")?;
        ComponentBatch::builder()
            .component(name("owner"), factory.spec(owner)?, factory.clone())
            .component(name("helper"), factory.spec(helper)?, factory.clone())
            .build()
    })
    .await?;
    core.shutdown().await?;
    Ok(())
}
