// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use async_trait::async_trait;
use drasi_lib::{
    channels::ResultDiff,
    computation::v1::*,
    config::QueryInputConfig,
    management::{AcceptanceReceipt, DesiredInstance},
    DrasiLib, Query,
};
use drasi_reaction_application::NativeApplicationReaction;
use drasi_source_application::{
    ApplicationSourceHandle, NativeApplicationSource, PropertyMapBuilder,
};
use tokio::sync::mpsc;

const STREAM: &str = "orders-events";

struct ApplicationFactory {
    descriptor: FactoryDescriptor,
    source: Mutex<Option<ApplicationSourceHandle>>,
    reaction: Mutex<Option<mpsc::Receiver<InputEnvelope>>>,
}

impl ApplicationFactory {
    fn new(role: ComponentRole) -> Arc<Self> {
        Arc::new(Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(
                    if role == ComponentRole::Source {
                        "test/managed-native-source"
                    } else {
                        "test/managed-native-reaction"
                    },
                    "1",
                )
                .expect("configuration native wiring fixture"),
                role,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::new(),
                    allow_additional: false,
                },
                dependencies: BTreeMap::new(),
            },
            source: Mutex::new(None),
            reaction: Mutex::new(None),
        })
    }
}

#[async_trait]
impl ComponentFactory for ApplicationFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn specification_from_config(
        &self,
        id: ComponentId,
        _: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        let source = self.descriptor.role == ComponentRole::Source;
        Ok(ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(
                id,
                vec![PortDescriptor::new(
                    PortId::try_new(if source { "out" } else { "in" })?,
                    if source {
                        PortDirection::Output
                    } else {
                        PortDirection::Input
                    },
                    if source {
                        GraphChangeCodec::schema()
                    } else {
                        QueryChangeCodec::schema()
                    }
                    .descriptor()
                    .clone(),
                    PipeRequirements::default(),
                )],
            )?
            .with_semantic_kind(if source {
                ComponentSemanticKind::Source
            } else {
                ComponentSemanticKind::Reaction
            }),
            role: self.descriptor.role,
            completion: (!source).then_some(SinkCompletion::Accepted),
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        })
    }

    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let construct = || -> anyhow::Result<_> {
            let capacity = NonZeroUsize::new(8).expect("configuration native wiring fixture");
            if self.descriptor.role == ComponentRole::Source {
                let (source, handle) = NativeApplicationSource::channel(
                    context.instance_id.to_string(),
                    context.graph_id.to_string(),
                    context.component_id.clone(),
                    StreamId::try_new(STREAM)?,
                    capacity,
                )?;
                assert!(self
                    .source
                    .lock()
                    .expect("configuration native wiring fixture")
                    .replace(handle)
                    .is_none());
                Ok(ConstructedComponent::source(Box::new(source)))
            } else {
                let (reaction, receiver) = NativeApplicationReaction::channel(
                    context.component_id.clone(),
                    QueryChangeCodec::schema().descriptor().clone(),
                    capacity,
                )?;
                assert!(self
                    .reaction
                    .lock()
                    .expect("configuration native wiring fixture")
                    .replace(receiver)
                    .is_none());
                Ok(ConstructedComponent::sink(Box::new(reaction)))
            }
        };
        construct().map_err(ComponentCreationError::terminal)
    }
}

fn edge(from: &ComponentId, to: &ComponentId) -> EdgeDefinition {
    EdgeDefinition::new(
        Endpoint::new(
            from.clone(),
            PortId::try_new("out").expect("configuration native wiring fixture"),
        ),
        Endpoint::new(
            to.clone(),
            PortId::try_new("in").expect("configuration native wiring fixture"),
        ),
    )
}

async fn settle(core: &DrasiLib) -> anyhow::Result<()> {
    let status = core.reconcile_desired_state().await?;
    anyhow::ensure!(
        status.applied && status.error.is_none() && status.resource_errors.is_empty(),
        "{status:?}"
    );
    for (id, observed) in &core.computation_control()?.observed().components {
        anyhow::ensure!(observed.failure.is_none(), "{id}: {observed:?}");
    }
    Ok(())
}

async fn add_application(
    core: &DrasiLib,
    factory: Arc<ApplicationFactory>,
    name: &'static str,
    input: Option<ComponentId>,
) -> anyhow::Result<(ComponentHandle, AcceptanceReceipt)> {
    let revision = core.desired_configuration()?.revision;
    let receipt = core
        .create_managed_components(revision, name, move |ids| {
            let id = ids.allocate()?;
            let specification = factory
                .specification_from_config(id.clone(), serde_json::json!({}))
                .map_err(|source| GraphError::Component {
                    component: id.clone(),
                    operation: "describe application fixture",
                    source,
                })?;
            let source = specification.role == ComponentRole::Source;
            let mut declaration =
                ComponentAddition::declaration(ComponentName::try_new(name)?, specification);
            if source {
                declaration =
                    declaration.bind_stream(PortId::try_new("out")?, StreamId::try_new(STREAM)?);
            }
            let mut topology = DesiredInstance::default().topology;
            topology.components.push(declaration.definition);
            if let Some(input) = input {
                topology.relationships.push(DesiredRelationship {
                    definition: edge(&input, &id),
                    policy: RelationshipPolicy::default(),
                    pipe: DesiredPipe::Bounded { capacity: 8 },
                });
            }
            Ok(topology)
        })
        .await?;
    let id = receipt
        .creation
        .as_ref()
        .expect("configuration native wiring fixture")
        .components[0]
        .id
        .clone();
    settle(core).await?;
    let handle = core.component_by_id(&id)?;
    Ok((handle, receipt))
}

fn assert_connected(core: &DrasiLib, expected: &[EdgeDefinition]) -> anyhow::Result<()> {
    let control = core.computation_control()?;
    let topology = control.desired_snapshot();
    for edge in expected {
        assert_eq!(
            topology
                .edges
                .iter()
                .filter(|actual| &actual.definition == edge)
                .count(),
            1,
            "missing or duplicated connection {edge:?}; actual: {:?}; pending: {:?}",
            topology.edges,
            topology.unbound_relationships
        );
        assert_eq!(
            control.observed().relationships[edge].binding,
            BindingState::Bound
        );
    }
    assert!(topology.unbound_relationships.is_empty());
    Ok(())
}

async fn managed_native_wiring() -> anyhow::Result<()> {
    let source_factory = ApplicationFactory::new(ComponentRole::Source);
    let reaction_factory = ApplicationFactory::new(ComponentRole::Sink);
    let mut factories = FactoryRegistry::standard();
    factories.register(source_factory.clone())?;
    factories.register(reaction_factory.clone())?;
    let core = DrasiLib::builder()
        .with_component_factories(factories)
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    core.start().await?;
    let (source, _) = add_application(&core, source_factory.clone(), "orders", None).await?;
    let query = core
        .add_query_with_handle(
            Query::cypher(
                drasi_core_types::ComponentName::try_new("view").expect("fixture component name"),
            )
            .query("MATCH (n:Item) RETURN n.value AS value")
            .with_input(QueryInputConfig::by_id(source.id().clone()))
            .enable_bootstrap(false)
            .build()?,
        )
        .await?;
    let (reaction, receipt) = add_application(
        &core,
        reaction_factory.clone(),
        "capture",
        Some(query.id().clone()),
    )
    .await?;
    let source_edge = edge(source.id(), query.id());
    let reaction_edge = edge(query.id(), reaction.id());
    let saved = core.desired_configuration()?;
    assert_eq!(saved.revision, receipt.revision);
    assert_eq!(
        saved
            .desired
            .topology
            .relationships
            .iter()
            .chain(&saved.desired.topology.boundary_relationships)
            .filter(|edge| edge.definition == reaction_edge)
            .count(),
        1,
        "acceptance must retain the explicit UUID connection"
    );
    let stored_query = saved
        .desired
        .ordinary
        .iter()
        .find(|record| &record.id == query.id())
        .context("accepted query record is missing")?
        .query_configuration()?;
    assert_eq!(stored_query.inputs[0].component.as_id(), Some(source.id()));
    assert_eq!(
        core.get_query_config_by_id(query.id()).await?.inputs[0]
            .component
            .as_id(),
        Some(source.id())
    );
    for handle in [&source, &query, &reaction] {
        handle.wait_started().await?;
    }
    assert_connected(&core, &[source_edge.clone(), reaction_edge.clone()])?;
    let input = source_factory
        .source
        .lock()
        .expect("configuration native wiring fixture")
        .take()
        .expect("configuration native wiring fixture");
    let mut output = reaction_factory
        .reaction
        .lock()
        .expect("configuration native wiring fixture")
        .take()
        .expect("configuration native wiring fixture");
    let generations = [source.generation(), query.generation(), reaction.generation()];

    for value in [7, 8] {
        input
            .send_node_insert(
                format!("row-{value}"),
                vec!["Item"],
                PropertyMapBuilder::new()
                    .with_integer("value", value)
                    .build(),
            )
            .await?;
        let result = output.recv().await.context("application reaction closed")?;
        let result = QueryChangeCodec::to_legacy_result(&result.envelope)?;
        assert_eq!(result.query_id, query.id().clone());
        assert!(
            matches!(result.results.as_slice(), [ResultDiff::Add { data, .. }]
                if data == &serde_json::json!({"value": value}))
        );
        if value == 7 {
            core.rename_component_by_id(
                query.id(),
                drasi_core_types::ComponentName::try_new("renamed-view")
                    .expect("fixture component name"),
            )
            .await?;
        }
        settle(&core).await?;
        assert_connected(&core, &[source_edge.clone(), reaction_edge.clone()])?;
        for (handle, generation) in [&source, &query, &reaction].iter().zip(generations) {
            assert_eq!(core.component_by_id(handle.id())?.generation(), generation);
        }
        assert!(core.management_status().await?.converged());
    }

    let mut target = core.desired_configuration()?;
    target
        .desired
        .topology
        .components
        .retain(|node| node.descriptor.id() != reaction.id());
    target
        .desired
        .topology
        .relationships
        .retain(|edge| edge.definition != reaction_edge);
    target
        .desired
        .topology
        .boundary_relationships
        .retain(|edge| edge.definition != reaction_edge);
    core.apply_desired_state(target.revision, "remove-capture", target.desired)
        .await?;
    settle(&core).await?;
    assert!(reaction.observed().is_err());
    let control = core.computation_control()?;
    assert!(control
        .desired_snapshot()
        .edges
        .iter()
        .all(|edge| edge.definition != reaction_edge));
    assert_connected(&core, &[source_edge])?;
    assert!(!core.allow_external_component_ids());
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn managed_native_wiring_current_thread() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(30), managed_native_wiring())
        .await
        .context("managed native wiring timed out on current-thread runtime")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_native_wiring_multi_thread() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(30), managed_native_wiring())
        .await
        .context("managed native wiring timed out on multi-thread runtime")?
}
