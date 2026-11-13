// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use drasi_lib::{
    computation::v1::*,
    config::{AssignedQueryConfig, ComponentReference, QueryConfig},
    management::{
        OrdinaryComponent, OrdinaryComponentDefinition, OrdinaryComponentRecord,
        OrdinaryComponentResolver,
    },
    ComponentStatus, DrasiLib, Query,
};
use drasi_reaction_application::{ApplicationReaction, ApplicationReactionHandle};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use serde_json::json;

const DEADLINE: Duration = Duration::from_secs(10);

fn source_config() -> ApplicationSourceConfig {
    ApplicationSourceConfig {
        properties: Default::default(),
        durability: None,
    }
}

type SourceHandleSlot = Arc<Mutex<Option<ApplicationSourceHandle>>>;

fn source_factory(
    slot: SourceHandleSlot,
    config: ApplicationSourceConfig,
) -> impl FnOnce(ComponentId) -> Result<ApplicationSource> + Send {
    move |id| {
        let (source, handle) = ApplicationSource::new(id.clone(), config)?;
        *slot.lock().expect("source handle slot lock") = Some(handle);
        Ok(source)
    }
}

fn take_source_handle(slot: &SourceHandleSlot) -> Result<ApplicationSourceHandle> {
    slot.lock()
        .expect("source handle slot lock")
        .take()
        .context("application source handle was not constructed")
}

fn named_id(core: &DrasiLib, name: &drasi_core::ComponentName) -> Result<ComponentId> {
    Ok(core.component_by_name(name)?.id().clone())
}

fn named_component(core: &DrasiLib, name: &drasi_core::ComponentName) -> Result<ComponentHandle> {
    let id = named_id(core, name)?;
    Ok(core.component_by_id(&id)?)
}

async fn named_query(
    core: &DrasiLib,
    name: &drasi_core::ComponentName,
) -> Result<Arc<dyn drasi_lib::queries::Query>> {
    let id = named_id(core, name)?;
    core.query_manager()
        .get_query_instance_by_id(&id)
        .await
        .map_err(anyhow::Error::msg)
}

async fn add_application_reaction(
    core: &DrasiLib,
    name: drasi_core::ComponentName,
    query_names: &[&str],
) -> Result<ApplicationReactionHandle> {
    let output = Arc::new(Mutex::new(None));
    let capture = output.clone();
    let inputs = query_names
        .iter()
        .map(|name| {
            drasi_core::ComponentName::try_new(*name)
                .map(ComponentReference::by_name)
                .map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    let handle = core
        .create_reaction(name, inputs, true, move |id, inputs| {
            let (reaction, output) = ApplicationReaction::new(
                id.clone(),
                inputs
                    .iter()
                    .cloned()
                    .map(ComponentReference::by_id)
                    .collect(),
            );
            *capture.lock().expect("reaction handle slot lock") = Some(output);
            Ok(reaction)
        })
        .await?;
    handle.wait_created().await?;
    let output = output
        .lock()
        .expect("reaction handle slot lock")
        .take()
        .context("application reaction handle was not constructed")?;
    Ok(output)
}

fn assigned(
    mut config: QueryConfig,
    query_id: ComponentId,
    source_id: &ComponentId,
) -> AssignedQueryConfig {
    for input in &mut config.inputs {
        input.component = ComponentReference::by_id(source_id.clone());
    }
    config.with_assigned_id(query_id)
}

#[derive(Default)]
struct ManagedApplicationSources {
    handles: Mutex<HashMap<String, ApplicationSourceHandle>>,
}

impl ManagedApplicationSources {
    fn take(&self, key: &str) -> Result<ApplicationSourceHandle> {
        self.handles
            .lock()
            .expect("managed source handle registry lock")
            .remove(key)
            .with_context(|| format!("managed source handle {key} was not constructed"))
    }

    fn key(definition: &OrdinaryComponentRecord) -> Result<&str> {
        let OrdinaryComponent::Source { configuration, .. } = &definition.component else {
            anyhow::bail!("managed application fixture requires a source definition");
        };
        configuration
            .get("handle")
            .and_then(serde_json::Value::as_str)
            .context("managed application fixture requires a handle key")
    }
}

#[async_trait::async_trait]
impl OrdinaryComponentResolver for ManagedApplicationSources {
    fn plan_update(
        &self,
        _instance_id: &str,
        current: &OrdinaryComponentRecord,
        proposed: &OrdinaryComponentRecord,
    ) -> Result<ComponentUpdatePlan> {
        anyhow::ensure!(current.id == proposed.id, "component UUID changed");
        anyhow::ensure!(current.name == proposed.name, "component name changed");
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }

    async fn create_source(
        &self,
        _instance_id: &str,
        definition: &OrdinaryComponentRecord,
        _update: Option<&ComponentUpdateDirective>,
    ) -> Result<(Box<dyn drasi_lib::Source>, HashMap<String, String>)> {
        let key = Self::key(definition)?.to_string();
        let (source, handle) = ApplicationSource::new(definition.id.clone(), source_config())?;
        self.handles
            .lock()
            .expect("managed source handle registry lock")
            .insert(key, handle);
        Ok((Box::new(source), HashMap::new()))
    }

    async fn create_reaction(
        &self,
        _instance_id: &str,
        _definition: &OrdinaryComponentRecord,
        _inputs: &[ComponentId],
        _update: Option<&ComponentUpdateDirective>,
    ) -> Result<(Box<dyn drasi_lib::Reaction>, HashMap<String, String>)> {
        anyhow::bail!("managed application fixture does not construct reactions")
    }
}

fn managed_source_definition(id: ComponentId, name: &str, handle: &str) -> OrdinaryComponentRecord {
    OrdinaryComponentRecord {
        id,
        name: ComponentName::try_new(name).expect("component name"),
        auto_start: true,
        component: OrdinaryComponent::Source {
            kind: "application".into(),
            configuration: json!({"handle": handle}),
        },
    }
}

async fn settle_managed(core: &DrasiLib) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let status = core.reconcile_desired_state().await?;
            if let Some(error) = status.error {
                anyhow::bail!("managed reconciliation failed: {error}");
            }
            if core.desired_configuration()?.pending_updates.is_empty() {
                return Ok(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("managed reconciliation timed out")?
}

async fn create_managed_application_source(
    core: &DrasiLib,
    resolver: &ManagedApplicationSources,
    name: &str,
    handle: &str,
) -> Result<(ComponentId, ApplicationSourceHandle)> {
    let template = managed_source_definition(ComponentId::generate(), name, handle);
    let receipt = core
        .create_managed_ordinary_component(
            core.desired_configuration()?.revision,
            format!("create-{name}"),
            OrdinaryComponentDefinition {
                id: None,
                name: template.name,
                auto_start: template.auto_start,
                component: template.component,
            },
        )
        .await?;
    let id = receipt
        .creation
        .context("managed source creation receipt")?
        .components[0]
        .id
        .clone();
    settle_managed(core).await?;
    core.component_by_id(&id)?.wait_created().await?;
    Ok((id, resolver.take(handle)?))
}

async fn accept_native_removal(core: &DrasiLib, components: &BTreeSet<ComponentId>) -> Result<()> {
    let saved = core.desired_configuration()?;
    let mut desired = saved.desired;
    let topology = &mut desired.topology;
    topology
        .components
        .retain(|component| !components.contains(component.descriptor.id()));
    topology.relationships.retain(|relationship| {
        !components.contains(&relationship.definition.from.component)
            && !components.contains(&relationship.definition.to.component)
    });
    topology.boundary_relationships.retain(|relationship| {
        !components.contains(&relationship.definition.from.component)
            && !components.contains(&relationship.definition.to.component)
    });
    topology
        .control_connections
        .retain(|(from, to)| !components.contains(from) && !components.contains(to));
    topology
        .subscriptions
        .retain(|(from, to)| !components.contains(from) && !components.contains(to));
    topology
        .recovery_requirements
        .retain(|requirement| !components.contains(&requirement.consumer));
    topology
        .readiness_required
        .retain(|component| !components.contains(component));
    topology
        .component_resources
        .retain(|component, _| !components.contains(component));
    topology
        .component_plugins
        .retain(|component, _| !components.contains(component));
    core.apply_desired_state(
        saved.revision,
        core.new_configuration_request_id().await?,
        desired,
    )
    .await?;
    Ok(())
}

async fn remove_native_components(
    core: &DrasiLib,
    components: &BTreeSet<ComponentId>,
) -> Result<()> {
    accept_native_removal(core, components).await?;
    settle_managed(core).await
}

async fn query_mutations(native: bool) -> Result<()> {
    let left_input = Arc::new(Mutex::new(None));
    let right_input = Arc::new(Mutex::new(None));
    let core = lib_integration_tests::query_builder()
        .with_allow_external_component_ids(true)
        .with_id(format!("mutation-{native}"))
        .with_source_factory(
            drasi_core::ComponentName::try_new("left").expect("fixture component name"),
            true,
            source_factory(left_input.clone(), source_config()),
        )
        .with_source_factory(
            drasi_core::ComponentName::try_new("right").expect("fixture component name"),
            true,
            source_factory(right_input.clone(), source_config()),
        )
        .build()
        .await?;
    let left_input = take_source_handle(&left_input)?;
    let right_input = take_source_handle(&right_input)?;
    for id in ["left", "right"] {
        named_component(&core, &drasi_core::ComponentName::try_new(id)?)?
            .wait_created()
            .await?;
    }
    let config =
        Query::cypher(drasi_core::ComponentName::try_new("items").expect("fixture component name"))
            .query("MATCH (n:Item) RETURN n.value AS value")
            .from_input_by_name(
                drasi_core::ComponentName::try_new("left").expect("fixture component name"),
            )
            .enable_bootstrap(false)
            .build()?;
    let sibling = Query::cypher(
        drasi_core::ComponentName::try_new("sibling").expect("fixture component name"),
    )
    .query("MATCH (n:Item) RETURN n.value AS value")
    .from_input_by_name(drasi_core::ComponentName::try_new("left").expect("fixture component name"))
    .enable_bootstrap(false)
    .build()?;
    let native_items = if native {
        let left_id = named_id(
            &core,
            &drasi_core::ComponentName::try_new("left").expect("fixture component name"),
        )?;
        let batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source_by_id(&left_id).await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(assigned(config, ComponentId::generate(), &left_id))
            .build()?;
        let items = batch
            .definition
            .components
            .iter()
            .map(|component| component.descriptor.id().clone())
            .collect();
        core.add_components(batch).await?;
        let sibling = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source_by_id(&left_id).await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(assigned(sibling, ComponentId::generate(), &left_id))
            .build()?;
        core.add_components(sibling).await?;
        Some(items)
    } else {
        core.add_query(config).await?;
        core.add_query(sibling).await?;
        None
    };
    let output = add_application_reaction(
        &core,
        drasi_core::ComponentName::try_new("capture").expect("fixture component name"),
        &["items"],
    )
    .await?;
    let mut received = output.take_receiver().await.context("capture receiver")?;
    let output = add_application_reaction(
        &core,
        drasi_core::ComponentName::try_new("sibling-capture").expect("fixture component name"),
        &["sibling"],
    )
    .await?;
    let mut sibling_received = output.take_receiver().await.context("sibling receiver")?;
    core.start().await?;
    for id in ["capture", "sibling-capture"] {
        tokio::time::timeout(
            DEADLINE,
            named_component(&core, &drasi_core::ComponentName::try_new(id)?)?.wait_started(),
        )
        .await??;
    }
    let root = core.computation_info().await?.id;
    let old = named_query(
        &core,
        &drasi_core::ComponentName::try_new("items").expect("fixture component name"),
    )
    .await?;
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
        core.get_query_results_by_name(
            &drasi_core::ComponentName::try_new("items").expect("fixture component name")
        )
        .await?,
        vec![json!({"value":7})]
    );

    let replacement =
        Query::cypher(drasi_core::ComponentName::try_new("items").expect("fixture component name"))
            .query("MATCH (n:Item) RETURN n.value * 2 AS value")
            .from_input_by_name(
                drasi_core::ComponentName::try_new("right").expect("fixture component name"),
            )
            .enable_bootstrap(false)
            .build()?;
    let error = core
        .update_query_by_name(
            &drasi_core::ComponentName::try_new("items").expect("fixture component name"),
            replacement,
        )
        .await
        .expect_err("query bindings cannot be changed through QueryConfig");
    assert!(
        error.to_string().contains("validation failed")
            || error
                .to_string()
                .contains("query configuration updates cannot change")
            || error.to_string().contains("cannot change live inputs")
            || error.to_string().contains("stable bindings"),
        "unexpected replacement failure: {error:#}"
    );
    let current = core
        .get_query_config_by_name(
            &drasi_core::ComponentName::try_new("items").expect("fixture component name"),
        )
        .await?;
    assert_eq!(
        current.name,
        drasi_core::ComponentName::try_new("items").expect("fixture component name")
    );
    assert_eq!(current.query, "MATCH (n:Item) RETURN n.value AS value");
    assert_eq!(
        current.inputs[0].component.as_id(),
        Some(&named_id(
            &core,
            &drasi_core::ComponentName::try_new("left").expect("fixture component name")
        )?)
    );
    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core::ComponentName::try_new("items").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core::ComponentName::try_new("sibling").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core::ComponentName::try_new("left").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Running
    );
    assert_eq!(
        core.get_source_status_by_name(
            &drasi_core::ComponentName::try_new("right").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Running
    );
    assert_eq!(
        old.fetch_snapshot().await?.to_vec(),
        vec![json!({"value":7})]
    );
    right_input
        .send_node_insert(
            "after",
            vec!["Item"],
            PropertyMapBuilder::new().with_integer("value", 11).build(),
        )
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), received.recv())
            .await
            .is_err(),
        "the rejected update must not rewire the query to the right source"
    );
    left_input
        .send_node_insert(
            "after-left",
            vec!["Item"],
            PropertyMapBuilder::new().with_integer("value", 13).build(),
        )
        .await?;
    let event = tokio::time::timeout(DEADLINE, received.recv())
        .await?
        .context("result after rejected update")?;
    assert!(matches!(
        &event.results[..],
        [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &json!({"value":13})
    ));
    let rows = core
        .get_query_results_by_name(
            &drasi_core::ComponentName::try_new("items").expect("fixture component name"),
        )
        .await?;
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&json!({"value":7})));
    assert!(rows.contains(&json!({"value":13})));

    if native {
        assert!(
            remove_native_components(
                &core,
                native_items.as_ref().expect("native query component set"),
            )
            .await
            .is_err(),
            "live dependents must protect their query"
        );
    } else {
        assert!(
            core.remove_query_by_name(
                &drasi_core::ComponentName::try_new("items").expect("fixture component name")
            )
            .await
            .is_err(),
            "live dependents must protect their query"
        );
    }
    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core::ComponentName::try_new("items").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Running
    );
    core.remove_reaction_by_name(
        &drasi_core::ComponentName::try_new("capture").expect("fixture component name"),
        false,
    )
    .await?;
    let items_id = named_id(
        &core,
        &drasi_core::ComponentName::try_new("items").expect("fixture component name"),
    )?;
    if native {
        remove_native_components(
            &core,
            native_items.as_ref().expect("native query component set"),
        )
        .await?;
    } else {
        core.remove_query_by_name(
            &drasi_core::ComponentName::try_new("items").expect("fixture component name"),
        )
        .await?;
    }
    assert!(core
        .get_query_config_by_name(
            &drasi_core::ComponentName::try_new("items").expect("fixture component name")
        )
        .await
        .is_err());
    assert!(core.component_by_id(&items_id).is_err());
    assert!(old.fetch_snapshot().await.is_err());
    assert_eq!(core.computation_info().await?.id, root);
    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core::ComponentName::try_new("sibling").expect("fixture component name")
        )
        .await?,
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
async fn ordinary_query_updates_reject_binding_changes_without_disturbing_dependents() -> Result<()>
{
    tokio::time::timeout(Duration::from_secs(30), query_mutations(false)).await?
}

#[tokio::test]
async fn native_query_updates_reject_binding_changes_without_disturbing_dependents() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), query_mutations(true)).await?
}

#[tokio::test]
async fn reaction_recreation_accepts_native_queries() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let input = Arc::new(Mutex::new(None));
        let core = lib_integration_tests::query_builder()
            .with_allow_external_component_ids(true)
            .with_source_factory(
                drasi_core::ComponentName::try_new("input").expect("fixture component name"),
                true,
                source_factory(input.clone(), source_config()),
            )
            .build()
            .await?;
        let input = take_source_handle(&input)?;
        named_component(&core, &drasi_core::ComponentName::try_new("input").expect("fixture component name"))?.wait_created().await?;
        let ordinary = Query::cypher(drasi_core::ComponentName::try_new("ordinary").expect("fixture component name"))
            .query("MATCH (n:Item) RETURN n.value AS value")
            .from_input_by_name(drasi_core::ComponentName::try_new("input").expect("fixture component name"))
            .enable_bootstrap(false)
            .build()?;
        core.add_query(ordinary.clone()).await?;
        let mut native = ordinary;
        native.name = drasi_core::ComponentName::try_new("native").expect("fixture component name");
        let native_id = ComponentId::generate();
        let source_id = named_id(&core, &drasi_core::ComponentName::try_new("input").expect("fixture component name"))?;
        let batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source_by_id(&source_id).await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(assigned(native, native_id.clone(), &source_id))
            .build()?;
        core.add_components(batch).await?;
        let output = add_application_reaction(&core, drasi_core::ComponentName::try_new("capture").expect("fixture component name"), &["ordinary"]).await?;
        let _old_receiver = output.take_receiver().await.context("original receiver")?;
        core.start().await?;
        tokio::time::timeout(DEADLINE, named_component(&core, &drasi_core::ComponentName::try_new("capture").expect("fixture component name"))?.wait_started()).await??;

        core.remove_reaction_by_name(&drasi_core::ComponentName::try_new("capture").expect("fixture component name"), false).await?;
        let output = add_application_reaction(&core, drasi_core::ComponentName::try_new("capture").expect("fixture component name"), &["native"]).await?;
        let mut received = output.take_receiver().await.context("replacement receiver")?;
        tokio::time::timeout(DEADLINE, named_component(&core, &drasi_core::ComponentName::try_new("capture").expect("fixture component name"))?.wait_started()).await??;
        input.send_node_insert("one", vec!["Item"], PropertyMapBuilder::new().with_integer("value", 9).build()).await?;
        let event = tokio::time::timeout(DEADLINE, received.recv()).await?.context("native result")?;
        assert_eq!(event.query_id, native_id.clone());
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
        let input = Arc::new(Mutex::new(None));
        let core = lib_integration_tests::query_builder()
            .with_allow_external_component_ids(true)
            .with_id("native-removal-storage")
            .with_source_factory(
                drasi_core::ComponentName::try_new("input").expect("fixture component name"),
                true,
                source_factory(input.clone(), source_config()),
            )
            .with_index_provider("rocks", provider)
            .build()
            .await?;
        let input = take_source_handle(&input)?;
        named_component(
            &core,
            &drasi_core::ComponentName::try_new("input").expect("fixture component name"),
        )?
        .wait_created()
        .await?;
        core.start().await?;
        let source_id = named_id(
            &core,
            &drasi_core::ComponentName::try_new("input").expect("fixture component name"),
        )?;
        let query_id = ComponentId::generate();
        for cycle in 0..2 {
            let config = Query::cypher(
                drasi_core::ComponentName::try_new("stored").expect("fixture component name"),
            )
            .query("MATCH (n:Item) RETURN n.value AS value")
            .from_input_by_name(
                drasi_core::ComponentName::try_new("input").expect("fixture component name"),
            )
            .enable_bootstrap(false)
            .with_storage_backend(drasi_lib::StorageBackendRef::Named("rocks".into()))
            .build()?;
            let batch = core
                .computation_pipeline()?
                .source(
                    core.borrow_computation_source_by_id(&source_id).await?,
                    SourceSubscriptionOptions::default(),
                )?
                .query(assigned(config, query_id.clone(), &source_id))
                .build()?;
            core.add_components(batch).await?;
            tokio::time::timeout(DEADLINE, core.component_by_id(&query_id)?.wait_started())
                .await??;
            assert!(
                core.get_query_results_by_name(
                    &drasi_core::ComponentName::try_new("stored").expect("fixture component name")
                )
                .await?
                .is_empty(),
                "a removed query must not recover old rows"
            );
            let query = core
                .query_manager()
                .get_query_instance_by_id(&query_id)
                .await
                .map_err(anyhow::Error::msg)?;
            assert_eq!(query.fetch_snapshot().await?.as_of_sequence, 0);
            let mut subscription = query.subscribe(query_id.clone()).await?;
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
                core.get_query_results_by_name(
                    &drasi_core::ComponentName::try_new("stored").expect("fixture component name")
                )
                .await?,
                vec![json!({"value":cycle})]
            );
            core.stop_query_by_name(
                &drasi_core::ComponentName::try_new("stored").expect("fixture component name"),
            )
            .await?;
            core.remove_query_by_name(
                &drasi_core::ComponentName::try_new("stored").expect("fixture component name"),
            )
            .await?;
            assert!(query.fetch_snapshot().await.is_err());
            assert!(core.component_by_id(&query_id).is_err());
        }
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn source_replacement_rewires_native_query_subscriptions() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let resolver = Arc::new(ManagedApplicationSources::default());
        let core = lib_integration_tests::query_builder()
            .with_allow_external_component_ids(true)
            .with_id("native-source-replacement")
            .with_ordinary_component_resolver(resolver.clone())
            .build()
            .await?;
        let (source_id, input) =
            create_managed_application_source(&core, resolver.as_ref(), "input", "initial")
                .await?;
        let config = Query::cypher(drasi_core::ComponentName::try_new("items").expect("fixture component name"))
            .query("MATCH (n:Item) RETURN n.value AS value")
            .from_input_by_name(drasi_core::ComponentName::try_new("input").expect("fixture component name"))
            .enable_bootstrap(false)
            .build()?;
        let mut sibling_config = config.clone();
        sibling_config.name = drasi_core::ComponentName::try_new("sibling").expect("fixture component name");
        let items_id = ComponentId::generate();
        let sibling_id = ComponentId::generate();
        let batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source_by_id(&source_id).await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(assigned(config, items_id.clone(), &source_id))
            .query(assigned(
                sibling_config,
                sibling_id.clone(),
                &source_id,
            ))
            .build()?;
        core.add_components(batch).await?;
        core.start().await?;
        let query = core
            .query_manager()
            .get_query_instance_by_id(&items_id)
            .await
            .map_err(anyhow::Error::msg)?;
        let mut subscription = query.subscribe(drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000349").expect("fixture UUID")).await?;
        let sibling = core
            .query_manager()
            .get_query_instance_by_id(&sibling_id)
            .await
            .map_err(anyhow::Error::msg)?;
        let mut sibling_subscription = sibling.subscribe(drasi_core::ComponentId::try_new("00000000-0000-0000-0000-00000000034a").expect("fixture UUID")).await?;
        input.send_node_insert("before", vec!["Item"], PropertyMapBuilder::new().with_integer("value", 1).build()).await?;
        tokio::time::timeout(DEADLINE, subscription.receiver.recv()).await??;
        tokio::time::timeout(DEADLINE, sibling_subscription.receiver.recv()).await??;
        core.update_source_by_name(
            &drasi_core::ComponentName::try_new("input").expect("fixture component name"),
            managed_source_definition(source_id.clone(), "input", "replacement"),
        )
        .await?;
        let replacement_input = resolver.take("replacement")?;
        assert_eq!(core.get_source_status_by_name(&drasi_core::ComponentName::try_new("input").expect("fixture component name")).await?, ComponentStatus::Running);
        tokio::time::timeout(
            DEADLINE,
            core.component_by_id(&items_id)?.wait_started(),
        )
        .await
        .with_context(|| {
            format!(
                "updated query did not restart; observations: {:?}",
                core.computation_control()
                    .map(|control| control.observed().components.clone())
            )
        })?
        .with_context(|| {
            format!(
                "items {items_id} failed after source {source_id} replacement; desired: {:?}",
                core.computation_control()
                    .map(|control| control.desired_snapshot().nodes.clone())
            )
        })?;
        tokio::time::timeout(
            DEADLINE,
            core.component_by_id(&sibling_id)?
                .wait_started(),
        )
        .await??;
        let updated = core
            .query_manager()
            .get_query_instance_by_id(&items_id)
            .await
            .map_err(anyhow::Error::msg)?;
        let mut subscription = updated.subscribe(drasi_core::ComponentId::try_new("00000000-0000-0000-0000-00000000034b").expect("fixture UUID")).await?;
        let updated_sibling = core
            .query_manager()
            .get_query_instance_by_id(&sibling_id)
            .await
            .map_err(anyhow::Error::msg)?;
        let mut sibling_subscription = updated_sibling.subscribe(drasi_core::ComponentId::try_new("00000000-0000-0000-0000-00000000034c").expect("fixture UUID")).await?;
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
async fn native_scheduled_query_update_rebuilds_its_private_helpers() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let input = Arc::new(Mutex::new(None));
        let core = lib_integration_tests::query_builder()
            .with_allow_external_component_ids(true)
            .with_id("native-timer-replacement")
            .with_source_factory(
                drasi_core::ComponentName::try_new("input").expect("fixture component name"),
                true,
                source_factory(input.clone(), source_config()),
            )
            .build()
            .await?;
        let input = take_source_handle(&input)?;
        named_component(&core, &drasi_core::ComponentName::try_new("input").expect("fixture component name"))?.wait_created().await?;
        let mut config = Query::cypher(drasi_core::ComponentName::try_new("items").expect("fixture component name"))
            .query("MATCH (n:Item) WHERE drasi.trueFor(n.value > 0, duration({ milliseconds: 50 })) RETURN n.value AS value")
            .from_input_by_name(drasi_core::ComponentName::try_new("input").expect("fixture component name")).enable_bootstrap(false).build()?;
        let query_id = ComponentId::generate();
        let source_id = named_id(&core, &drasi_core::ComponentName::try_new("input").expect("fixture component name"))?;
        let batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source_by_id(&source_id).await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(assigned(config.clone(), query_id.clone(), &source_id))
            .build()?;
        core.add_components(batch).await?;
        core.start().await?;
        for cycle in 1..=2 {
            if cycle == 2 {
                config.query = "MATCH (n:Item) WHERE drasi.trueFor(n.value > 0, duration({ milliseconds: 50 })) RETURN n.value * 2 AS value".into();
                core
                    .update_query_by_name(&drasi_core::ComponentName::try_new("items").expect("fixture component name"), config.clone())
                    .await?;
            }
            let query = core
                .query_manager()
                .get_query_instance_by_id(&query_id)
                .await
                .map_err(anyhow::Error::msg)?;
            let mut subscription = query.subscribe(query_id.clone()).await?;
            input.send_node_insert(format!("row-{cycle}"), vec!["Item"], PropertyMapBuilder::new().with_integer("value", cycle).build()).await?;
            let event = tokio::time::timeout(DEADLINE, subscription.receiver.recv()).await??;
            let expected = if cycle == 1 { cycle } else { cycle * 2 };
            assert!(matches!(&event.results[..], [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &json!({"value":expected})));
        }
        assert_eq!(core.get_query_status_by_name(&drasi_core::ComponentName::try_new("items").expect("fixture component name")).await?, ComponentStatus::Running);
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }).await?
}

#[tokio::test]
async fn failed_native_query_declaration_can_be_removed_without_activation() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let input = Arc::new(Mutex::new(None));
        let core = lib_integration_tests::query_builder()
            .with_allow_external_component_ids(true)
            .with_source_factory(
                drasi_core::ComponentName::try_new("input").expect("fixture component name"),
                true,
                source_factory(input.clone(), source_config()),
            )
            .build()
            .await?;
        named_component(
            &core,
            &drasi_core::ComponentName::try_new("input").expect("fixture component name"),
        )?
        .wait_created()
        .await?;
        let mut config = Query::cypher(
            drasi_core::ComponentName::try_new("invalid").expect("fixture component name"),
        )
        .query("MATCH (n:Item) RETURN n.value AS value")
        .from_input_by_name(
            drasi_core::ComponentName::try_new("input").expect("fixture component name"),
        )
        .enable_bootstrap(false)
        .auto_start(false)
        .build()?;
        let query_id = ComponentId::generate();
        let source_id = named_id(
            &core,
            &drasi_core::ComponentName::try_new("input").expect("fixture component name"),
        )?;
        let mut batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source_by_id(&source_id).await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(assigned(config.clone(), query_id.clone(), &source_id))
            .build()?;
        config.query = "NOT A QUERY".into();
        let invalid = batch
            .definition
            .components
            .iter_mut()
            .find(|node| node.descriptor.id().as_str() == query_id.as_str())
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
            ConfigurationValue::Literal(serde_json::to_value(assigned(
                config,
                query_id.clone(),
                &source_id,
            ))?),
        );
        let components = batch
            .definition
            .components
            .iter()
            .map(|component| component.descriptor.id().clone())
            .collect();
        core.add_components(batch).await?;
        assert!(core
            .component_by_id(&query_id)?
            .wait_created()
            .await
            .is_err());
        assert_eq!(
            core.get_query_status_by_name(
                &drasi_core::ComponentName::try_new("invalid").expect("fixture component name")
            )
            .await?,
            ComponentStatus::Error
        );
        remove_native_components(&core, &components).await?;
        assert!(core.list_queries().await?.is_empty());
        assert!(named_component(
            &core,
            &drasi_core::ComponentName::try_new("input").expect("fixture component name")
        )
        .is_ok());
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn native_persistent_query_update_resets_state_and_preserves_restart() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(40), async {
        let directory = tempfile::tempdir()?;
        let provider = std::sync::Arc::new(drasi_index_rocksdb::RocksDbIndexProvider::new(
            directory.path().join("indexes"), false, false,
        ));
        let input = Arc::new(Mutex::new(None));
        let durable_source = ApplicationSourceConfig {
            properties: Default::default(),
            durability: Some(drasi_lib::DurabilityConfig {
                enabled: true, max_events: 128, capacity_policy: drasi_lib::CapacityPolicy::RejectIncoming,
            }),
        };
        let core = lib_integration_tests::query_builder()
            .with_allow_external_component_ids(true)
            .with_id("native-storage-replacement")
            .with_source_factory(drasi_core::ComponentName::try_new("input").expect("fixture component name"), true, source_factory(input.clone(), durable_source))
            .with_wal_provider(std::sync::Arc::new(drasi_wal_redb::RedbWalProvider::new(directory.path().join("wal"))))
            .with_index_provider("rocks", provider).build().await?;
        let input = take_source_handle(&input)?;
        named_component(&core, &drasi_core::ComponentName::try_new("input").expect("fixture component name"))?.wait_created().await?;
        let mut config = Query::cypher(drasi_core::ComponentName::try_new("stored").expect("fixture component name")).query("MATCH (n:Item) RETURN n.value AS value")
            .from_input_by_name(drasi_core::ComponentName::try_new("input").expect("fixture component name")).enable_bootstrap(false)
            .with_storage_backend(drasi_lib::StorageBackendRef::Named("rocks".into())).build()?;
        let query_id = ComponentId::generate();
        let source_id = named_id(&core, &drasi_core::ComponentName::try_new("input").expect("fixture component name"))?;
        let batch = core.computation_pipeline()?
            .source(core.borrow_computation_source_by_id(&source_id).await?, SourceSubscriptionOptions { borrowed_recovery: true, ..Default::default() })?
            .query(assigned(config.clone(), query_id.clone(), &source_id)).build()?;
        core.add_components(batch).await?;
        core.start().await?;
        for cycle in 1..=2 {
            if cycle == 2 {
                config.query = "MATCH (n:Item) RETURN n.value * 2 AS value".into();
                core
                    .update_query_by_name(&drasi_core::ComponentName::try_new("stored").expect("fixture component name"), config.clone())
                    .await?;
                assert!(
                    core.get_query_results_by_name(&drasi_core::ComponentName::try_new("stored").expect("fixture component name")).await?.is_empty()
                );
            }
            let query = core
                .query_manager()
                .get_query_instance_by_id(&query_id)
                .await
                .map_err(anyhow::Error::msg)?;
            let mut subscription = query.subscribe(query_id.clone()).await?;
            input.send_node_insert(format!("row-{cycle}"), vec!["Item"], PropertyMapBuilder::new().with_integer("value", cycle).build()).await?;
            let event = tokio::time::timeout(DEADLINE, subscription.receiver.recv()).await??;
            let expected = if cycle == 1 { cycle } else { cycle * 2 };
            assert!(matches!(&event.results[..], [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &json!({"value":expected})));
        }
        core.stop_query_by_name(&drasi_core::ComponentName::try_new("stored").expect("fixture component name")).await?;
        core.start_query_by_name(&drasi_core::ComponentName::try_new("stored").expect("fixture component name")).await?;
        let rows = core.get_query_results_by_name(&drasi_core::ComponentName::try_new("stored").expect("fixture component name")).await?;
        assert_eq!(rows, vec![json!({"value":4})]);
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
    async fn dispose(&mut self) -> Result<()> {
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
        let input = Arc::new(Mutex::new(None));
        let core = lib_integration_tests::query_builder()
            .with_allow_external_component_ids(true)
            .with_source_factory(
                drasi_core::ComponentName::try_new("input").expect("fixture component name"),
                true,
                source_factory(input.clone(), source_config()),
            )
            .build()
            .await?;
        named_component(
            &core,
            &drasi_core::ComponentName::try_new("input").expect("fixture component name"),
        )?
        .wait_created()
        .await?;
        let allow_cleanup = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = Query::cypher(
            drasi_core::ComponentName::try_new("cleanup").expect("fixture component name"),
        )
        .query("MATCH (n) RETURN n")
        .from_input_by_name(
            drasi_core::ComponentName::try_new("input").expect("fixture component name"),
        )
        .enable_bootstrap(false)
        .auto_start(false)
        .build()?;
        let query_id = ComponentId::generate();
        let source_id = named_id(
            &core,
            &drasi_core::ComponentName::try_new("input").expect("fixture component name"),
        )?;
        let mut batch = core
            .computation_pipeline()?
            .source(
                core.borrow_computation_source_by_id(&source_id).await?,
                SourceSubscriptionOptions::default(),
            )?
            .query(assigned(config, query_id.clone(), &source_id))
            .build()?
            .auto_start(false);
        let component = batch
            .definition
            .components
            .iter_mut()
            .find(|node| node.descriptor.id().as_str() == query_id.as_str())
            .context("cleanup query")?;
        let binding = BindingToken::generate();
        component.construction = ComponentConstruction::External {
            binding: binding.clone(),
        };
        batch.bindings.components.insert(
            binding,
            ConstructedComponent::transformer(Box::new(RemovalProbe {
                descriptor: component.descriptor.clone(),
                allow_cleanup: allow_cleanup.clone(),
                attempts: attempts.clone(),
            })),
        );
        let components = batch
            .definition
            .components
            .iter()
            .map(|component| component.descriptor.id().clone())
            .collect();
        core.add_components(batch).await?;
        let query = core.component_by_id(&query_id)?;
        query.wait_created().await?;
        accept_native_removal(&core, &components).await?;
        let failed = core.reconcile_desired_state().await?;
        let error = failed
            .error
            .as_deref()
            .context("failed disposal must report its cause")?;
        assert!(error.contains("injected state cleanup failure"), "{error}");
        assert!(core.component_by_id(&query_id).is_ok());
        let failed_attempts = attempts.load(std::sync::atomic::Ordering::Acquire);
        assert!(failed_attempts >= 1);
        assert!(query.observed()?.failure.is_some());
        allow_cleanup.store(true, std::sync::atomic::Ordering::Release);
        settle_managed(&core).await?;
        assert!(attempts.load(std::sync::atomic::Ordering::Acquire) > failed_attempts);
        assert!(core.component_by_id(&query_id).is_err());
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
