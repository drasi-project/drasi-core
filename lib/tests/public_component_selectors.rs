// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use drasi_lib::{
    computation::v1::{ComponentId, FactoryRegistry, ImplementationIdentity},
    config::ComponentReference,
    ComponentStatus, DrasiError, DrasiLib, DrasiLibBuilder, Query,
};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig};

fn application_source(id: ComponentId) -> anyhow::Result<ApplicationSource> {
    Ok(ApplicationSource::new(
        id.clone(),
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )?
    .0)
}

#[derive(Default)]
struct StartGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    starts: AtomicUsize,
}

struct GatedSource {
    inner: ApplicationSource,
    gate: Arc<StartGate>,
}

#[async_trait]
impl drasi_lib::Source for GatedSource {
    fn id(&self) -> &drasi_core_types::ComponentId {
        drasi_lib::Source::id(&self.inner)
    }

    fn type_name(&self) -> &str {
        drasi_lib::Source::type_name(&self.inner)
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        drasi_lib::Source::properties(&self.inner)
    }

    fn auto_start(&self) -> bool {
        false
    }

    fn supports_replay(&self) -> bool {
        drasi_lib::Source::supports_replay(&self.inner)
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.gate.starts.fetch_add(1, Ordering::SeqCst);
        self.gate.entered.notify_one();
        self.gate.release.notified().await;
        drasi_lib::Source::start(&self.inner).await
    }

    async fn stop(&self) -> anyhow::Result<()> {
        drasi_lib::Source::stop(&self.inner).await
    }

    async fn status(&self) -> ComponentStatus {
        drasi_lib::Source::status(&self.inner).await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> anyhow::Result<drasi_lib::channels::SubscriptionResponse> {
        drasi_lib::Source::subscribe(&self.inner, settings).await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn bind_runtime_context(&self, context: drasi_lib::context::SourceRuntimeContext) {
        drasi_lib::Source::bind_runtime_context(&self.inner, context).await;
    }
}

fn builder() -> DrasiLibBuilder {
    DrasiLib::builder().with_query_factory(
        FactoryRegistry::standard()
            .get(
                &ImplementationIdentity::try_new("drasi/continuous-query", "1")
                    .expect("reference query identity"),
            )
            .expect("reference query factory")
            .clone(),
    )
}

async fn core() -> anyhow::Result<DrasiLib> {
    Ok(builder().build().await?)
}

#[tokio::test]
async fn exact_names_include_uuid_looking_text_and_ids_are_explicit() -> anyhow::Result<()> {
    let core = core().await?;
    core.start().await?;

    let ordinary = core
        .create_source(
            drasi_core_types::ComponentName::try_new("ordinary").expect("fixture component name"),
            false,
            application_source,
        )
        .await?;
    ordinary.wait_created().await?;
    let uuid_looking_name = ordinary.id().to_string();
    let uuid_looking_name =
        drasi_core_types::ComponentName::try_new(uuid_looking_name).expect("UUID-shaped name");
    let uuid_named = core
        .create_source(uuid_looking_name.clone(), false, application_source)
        .await?;
    uuid_named.wait_created().await?;

    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("ordinary").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Added
    );
    core.start_source_by_name(&uuid_looking_name).await?;
    uuid_named.wait_started().await?;
    assert_eq!(
        core.get_source_status_by_id(uuid_named.id()).await?,
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_source_status_by_id(ordinary.id()).await?,
        ComponentStatus::Added
    );

    core.start_source_by_id(ordinary.id()).await?;
    ordinary.wait_started().await?;
    assert_eq!(
        core.get_source_info_by_name(
            &drasi_core_types::ComponentName::try_new("ordinary").expect("fixture component name")
        )
        .await?
        .name,
        drasi_core_types::ComponentName::try_new("ordinary").expect("fixture component name")
    );
    assert_eq!(
        core.get_source_info_by_id(uuid_named.id()).await?.name,
        uuid_looking_name
    );
    assert!(core
        .get_source_schema_by_name(
            &drasi_core_types::ComponentName::try_new("ordinary").expect("fixture component name")
        )
        .await?
        .is_none());
    assert!(core
        .get_source_schema_by_id(uuid_named.id())
        .await?
        .is_none());
    drop(
        core.get_source_events_by_name(
            &drasi_core_types::ComponentName::try_new("ordinary").expect("fixture component name"),
        )
        .await?,
    );
    drop(core.get_source_events_by_id(uuid_named.id()).await?);
    drop(
        core.subscribe_source_logs_by_name(
            &drasi_core_types::ComponentName::try_new("ordinary").expect("fixture component name"),
        )
        .await?,
    );
    drop(core.subscribe_source_events_by_id(uuid_named.id()).await?);

    let query = core
        .add_query_with_handle(
            Query::cypher(
                drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
            )
            .query("MATCH (n) RETURN n")
            .from_input_by_name(
                drasi_core_types::ComponentName::try_new("ordinary")
                    .expect("fixture component name"),
            )
            .enable_bootstrap(false)
            .auto_start(false)
            .build()?,
        )
        .await?;
    query.wait_created().await?;
    let error = core
        .get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        DrasiError::ComponentNotFound {
            ref component_type,
            subject: drasi_lib::error::OperationSubject::Name(ref name),
        } if component_type == "source" && name == &drasi_lib::ComponentName::try_new("view").unwrap()
    ));

    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn query_reaction_and_graph_selectors_support_names_and_ids() -> anyhow::Result<()> {
    let query_config = Query::cypher(
        drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
    )
    .query("MATCH (n) RETURN n")
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
    )
    .enable_bootstrap(false)
    .auto_start(false)
    .build()?;
    let core = builder()
        .with_source_factory(
            drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
            false,
            application_source,
        )
        .with_query(query_config)
        .build()
        .await?;
    let source = core.component_by_name(
        &drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
    )?;
    source.wait_created().await?;
    let query = core.component_by_name(
        &drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
    )?;
    query.wait_created().await?;
    let reaction = core
        .create_reaction(
            drasi_core_types::ComponentName::try_new("output").expect("fixture component name"),
            vec![ComponentReference::by_name(
                drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
            )],
            false,
            |id, inputs| {
                Ok(ApplicationReaction::new(
                    id.clone(),
                    inputs.into_iter().map(ComponentReference::by_id).collect(),
                )
                .0)
            },
        )
        .await?;
    reaction.wait_created().await?;

    core.start().await?;
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
    )
    .await?;
    source.wait_started().await?;
    core.start_query_by_id(query.id()).await?;
    query.wait_started().await?;
    core.start_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("output").expect("fixture component name"),
    )
    .await?;
    reaction.wait_started().await?;

    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_query_status_by_id(query.id()).await?,
        ComponentStatus::Running
    );
    assert!(core
        .get_query_results_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name")
        )
        .await?
        .is_empty());
    assert!(core.get_query_results_by_id(query.id()).await?.is_empty());
    assert!(core
        .subscribe_query_results_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name")
        )
        .await?
        .snapshot
        .is_empty());
    assert_eq!(
        core.get_query_info_by_id(query.id()).await?.name,
        drasi_core_types::ComponentName::try_new("view").expect("fixture component name")
    );
    assert_eq!(
        core.get_query_config_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name")
        )
        .await?
        .name,
        drasi_core_types::ComponentName::try_new("view").expect("fixture component name")
    );
    drop(core.get_query_events_by_id(query.id()).await?);
    drop(
        core.subscribe_query_logs_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
        )
        .await?,
    );
    drop(core.subscribe_query_events_by_id(query.id()).await?);
    let _ = core
        .get_query_output_metrics_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
        )
        .await?;
    let _ = core.get_query_output_metrics_by_id(query.id()).await?;

    assert_eq!(
        core.get_reaction_status_by_id(reaction.id()).await?,
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_reaction_info_by_name(
            &drasi_core_types::ComponentName::try_new("output").expect("fixture component name")
        )
        .await?
        .name,
        drasi_core_types::ComponentName::try_new("output").expect("fixture component name")
    );
    drop(
        core.get_reaction_events_by_name(
            &drasi_core_types::ComponentName::try_new("output").expect("fixture component name"),
        )
        .await?,
    );
    drop(core.subscribe_reaction_logs_by_id(reaction.id()).await?);
    drop(
        core.subscribe_reaction_events_by_name(
            &drasi_core_types::ComponentName::try_new("output").expect("fixture component name"),
        )
        .await?,
    );
    let _ = core
        .get_reaction_metrics_by_name(
            &drasi_core_types::ComponentName::try_new("output").expect("fixture component name"),
        )
        .await?;
    let _ = core.get_reaction_metrics_by_id(reaction.id()).await?;

    assert_eq!(
        core.get_dependencies_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name")
        )
        .await?
        .len(),
        1
    );
    assert_eq!(core.get_dependencies_by_id(query.id()).await?.len(), 1);
    assert_eq!(
        core.get_dependents_by_name(
            &drasi_core_types::ComponentName::try_new("input").expect("fixture component name")
        )
        .await?
        .len(),
        1
    );
    assert_eq!(core.get_dependents_by_id(source.id()).await?.len(), 1);
    assert!(core
        .can_remove_component_by_name(
            &drasi_core_types::ComponentName::try_new("input").expect("fixture component name")
        )
        .await
        .is_err());
    assert!(core.can_remove_component_by_id(source.id()).await.is_err());

    core.stop_reaction_by_id(reaction.id()).await?;
    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
    )
    .await?;

    let mut update = core
        .get_query_config_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
        )
        .await?
        .into_authoring();
    update.query = "MATCH (n) RETURN n AS first_update".into();
    let error = core
        .update_query_by_name(
            &drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
            update,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error.classification(),
        DrasiError::OperationFailed {
            component_type,
            subject: drasi_lib::error::OperationSubject::Id(component_id),
            operation,
            ..
        } if component_type == "query"
            && component_id == query.id()
            && operation == "update"
    ));

    let mut update = core
        .get_query_config_by_id(query.id())
        .await?
        .into_authoring();
    update.query = "MATCH (n) RETURN n AS second_update".into();
    let error = core
        .update_query_by_id(query.id(), update)
        .await
        .unwrap_err();
    assert!(matches!(
        error.classification(),
        DrasiError::OperationFailed {
            component_type,
            subject: drasi_lib::error::OperationSubject::Id(component_id),
            operation,
            ..
        } if component_type == "query"
            && component_id == query.id()
            && operation == "update"
    ));

    core.remove_reaction_by_name(
        &drasi_core_types::ComponentName::try_new("output").expect("fixture component name"),
        false,
    )
    .await?;
    core.remove_query_by_id(query.id()).await?;
    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
        false,
    )
    .await?;
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn preconstructed_reaction_query_names_bind_to_stable_ids() -> anyhow::Result<()> {
    let source = application_source(ComponentId::generate())?;
    let reaction = ApplicationReaction::new(
        ComponentId::generate().clone(),
        vec![drasi_computation_contracts::ComponentReference::by_name(
            drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
        )],
    )
    .0;
    let core = builder()
        .with_allow_external_component_ids(true)
        .with_source(
            drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
            source,
        )
        .with_query(
            Query::cypher(
                drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
            )
            .query("MATCH (n) RETURN n")
            .from_input_by_name(
                drasi_core_types::ComponentName::try_new("input").expect("fixture component name"),
            )
            .enable_bootstrap(false)
            .build()?,
        )
        .with_reaction(
            drasi_core_types::ComponentName::try_new("output").expect("fixture component name"),
            reaction,
        )
        .build()
        .await?;

    tokio::time::timeout(std::time::Duration::from_secs(5), core.start()).await??;
    assert_eq!(
        core.get_reaction_status_by_name(
            &drasi_core_types::ComponentName::try_new("output").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Running
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolved_identity_survives_rename_and_name_reuse() -> anyhow::Result<()> {
    let core = core().await?;
    core.start().await?;

    let gate = Arc::new(StartGate::default());
    let original_gate = gate.clone();
    let original = core
        .create_source(
            drasi_core_types::ComponentName::try_new("reused").expect("fixture component name"),
            false,
            move |id| {
                Ok(GatedSource {
                    inner: application_source(id)?,
                    gate: original_gate,
                })
            },
        )
        .await?;
    original.wait_created().await?;
    let resolved = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("reused").expect("fixture component name"),
        )?
        .id()
        .clone();

    let admitted_core = core.clone();
    let admitted = tokio::spawn(async move {
        admitted_core
            .start_source_by_name(
                &drasi_core_types::ComponentName::try_new("reused")
                    .expect("fixture component name"),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified()).await?;
    assert_eq!(gate.starts.load(Ordering::SeqCst), 1);

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        core.rename_component_by_id(
            original.id(),
            drasi_core_types::ComponentName::try_new("renamed").expect("fixture component name"),
        ),
    )
    .await??;
    let replacement = core
        .create_source(
            drasi_core_types::ComponentName::try_new("reused").expect("fixture component name"),
            false,
            application_source,
        )
        .await?;
    replacement.wait_created().await?;
    assert_ne!(replacement.id(), &resolved);
    assert_eq!(
        core.get_source_status_by_id(replacement.id()).await?,
        ComponentStatus::Added
    );

    gate.release.notify_one();
    admitted.await??;
    original.wait_started().await?;
    assert_eq!(gate.starts.load(Ordering::SeqCst), 1);
    assert_eq!(
        core.get_source_status_by_id(&resolved).await?,
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_source_status_by_id(replacement.id()).await?,
        ComponentStatus::Added
    );

    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("reused").expect("fixture component name"),
    )
    .await?;
    replacement.wait_started().await?;
    assert_eq!(
        core.get_source_status_by_id(replacement.id()).await?,
        ComponentStatus::Running
    );

    core.remove_source_by_id(&resolved, false).await?;
    assert!(core.stop_source_by_id(&resolved).await.is_err());
    assert_eq!(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("reused").expect("fixture component name")
        )?
        .id(),
        replacement.id()
    );
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core_types::ComponentName::try_new("reused").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Running
    );

    core.remove_source_by_name(
        &drasi_core_types::ComponentName::try_new("reused").expect("fixture component name"),
        false,
    )
    .await?;
    core.shutdown().await?;
    Ok(())
}
