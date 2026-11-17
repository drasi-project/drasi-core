// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Declared component descriptions are compared when the graph binds an
//! edge or a resource. A mismatch never rejects the graph: both nodes stay,
//! the edge is failed and unbound, and the consumer carries a terminal
//! binding failure until either side changes.

#[path = "support/mod.rs"]
mod fixture_support;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use drasi_lib::computation::v1::*;
use serde_json::json;

fn component(value: u128) -> ComponentId {
    fixture_support::component_id(value)
}

fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("port")
}

fn name(value: &str) -> ComponentName {
    ComponentName::try_new(value).expect("name")
}

fn literal(value: &str) -> ConfigurationValue {
    ConfigurationValue::Literal(json!(value))
}

fn configured(specification: &ComponentSpecification, key: &str) -> String {
    match specification.configuration.get(key) {
        Some(ConfigurationValue::Literal(value)) => value.as_str().unwrap_or_default().to_owned(),
        _ => String::new(),
    }
}

fn output(identity: &str) -> OutputDescription {
    match identity {
        "durable" => OutputDescription::DURABLE,
        "volatile" => OutputDescription::VOLATILE,
        "relay" => OutputDescription::RELAY,
        _ => OutputDescription::UNTRACKED,
    }
}

fn producer_description(descriptor: &ComponentDescriptor, identity: &str) -> ComponentDescription {
    let progress = match identity {
        "durable" => ProgressPersistence::Durable,
        "volatile" => ProgressPersistence::Volatile,
        _ => ProgressPersistence::Stateless,
    };
    ComponentDescription::new(progress).with_outputs(descriptor, output(identity))
}

fn consumer_description(descriptor: &ComponentDescriptor, delivery: &str) -> ComponentDescription {
    let delivery = match delivery {
        "exactlyOnce" => InputDelivery::ExactlyOnce,
        "atLeastOnce" => InputDelivery::AtLeastOnce,
        _ => InputDelivery::Unkeyed,
    };
    ComponentDescription::new(ProgressPersistence::Durable).with_inputs(descriptor, delivery)
}

struct Producer {
    descriptor: ComponentDescriptor,
    description: ComponentDescription,
}

#[async_trait]
impl ComputationComponent for Producer {
    fn description(&self) -> ComponentDescription {
        self.description.clone()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for Producer {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

struct Consumer {
    descriptor: ComponentDescriptor,
    description: ComponentDescription,
}

#[async_trait]
impl ComputationComponent for Consumer {
    fn description(&self) -> ComponentDescription {
        self.description.clone()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSink for Consumer {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A store whose durability the test chooses.
struct Store;

/// Producers declare `identity` (durable, volatile, untracked or relay); the
/// instance reports `instance` when set, to exercise the instance check.
struct ProducerFactory(FactoryDescriptor);

#[async_trait]
impl ComponentFactory for ProducerFactory {
    fn describe(&self, context: &ConstructionContext) -> anyhow::Result<ComponentDescription> {
        Ok(producer_description(
            &context.specification.descriptor,
            &configured(&context.specification, "identity"),
        ))
    }
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
        let specification = &context.specification;
        let identity = match configured(specification, "instance").as_str() {
            "" => configured(specification, "identity"),
            other => other.to_owned(),
        };
        Ok(ConstructedComponent::source(Box::new(Producer {
            description: producer_description(&specification.descriptor, &identity),
            descriptor: specification.descriptor.clone(),
        })))
    }
}

/// Consumers declare their inputs' `delivery`, and optionally require a
/// durable `store`.
struct ConsumerFactory(FactoryDescriptor);

#[async_trait]
impl ComponentFactory for ConsumerFactory {
    fn describe(&self, context: &ConstructionContext) -> anyhow::Result<ComponentDescription> {
        Ok(describe_consumer(&context.specification))
    }
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    fn plan_update(
        &self,
        _: &ComponentSpecification,
        _: &ComponentSpecification,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        Ok(ConstructedComponent::sink(Box::new(Consumer {
            description: describe_consumer(&context.specification),
            descriptor: context.specification.descriptor.clone(),
        })))
    }
}

fn describe_consumer(specification: &ComponentSpecification) -> ComponentDescription {
    let description = consumer_description(
        &specification.descriptor,
        &configured(specification, "delivery"),
    );
    if specification.dependencies.contains_key("store") {
        description.with_dependency(
            "store",
            DependencyRequirement {
                durability: DurabilityRequirement::Durable,
            },
        )
    } else {
        description
    }
}

fn factory_descriptor(name: &str, role: ComponentRole) -> FactoryDescriptor {
    FactoryDescriptor {
        implementation: ImplementationIdentity::try_new(name, "1").expect("implementation"),
        role,
        configuration_version: 1,
        configuration: ConfigurationSchema {
            allow_additional: true,
            ..Default::default()
        },
        dependencies: if role == ComponentRole::Sink {
            BTreeMap::from([(
                Arc::from("store"),
                ResourceRequirement {
                    role: ResourceRole::StateStore,
                    minimum: 0,
                    maximum: Some(1),
                    concrete_type: Some(std::any::TypeId::of::<Store>()),
                },
            )])
        } else {
            BTreeMap::new()
        },
    }
}

fn producer_spec(
    factory: &ProducerFactory,
    id: u128,
    identity: &str,
    instance: Option<&str>,
) -> ComponentSpecification {
    let mut configuration = BTreeMap::from([(Arc::from("identity"), literal(identity))]);
    if let Some(instance) = instance {
        configuration.insert(Arc::from("instance"), literal(instance));
    }
    ComponentSpecification {
        descriptor: ComponentDescriptor::try_new(
            component(id),
            vec![PortDescriptor::new(
                port("out"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )
        .expect("producer descriptor"),
        role: ComponentRole::Source,
        completion: None,
        implementation: factory.0.implementation.clone(),
        configuration_version: 1,
        configuration,
        dependencies: BTreeMap::new(),
    }
}

fn consumer_descriptor(id: u128) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        component(id),
        vec![PortDescriptor::new(
            port("in"),
            PortDirection::Input,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("consumer descriptor")
}

fn consumer_spec(
    factory: &ConsumerFactory,
    id: u128,
    delivery: &str,
    store: Option<&str>,
) -> ComponentSpecification {
    ComponentSpecification {
        descriptor: consumer_descriptor(id),
        role: ComponentRole::Sink,
        completion: Some(SinkCompletion::Handled),
        implementation: factory.0.implementation.clone(),
        configuration_version: 1,
        configuration: BTreeMap::from([(Arc::from("delivery"), literal(delivery))]),
        dependencies: store
            .map(|store| {
                BTreeMap::from([(
                    Arc::from("store"),
                    vec![ResourceId::try_new(store).expect("resource")],
                )])
            })
            .unwrap_or_default(),
    }
}

fn edge(from: u128, to: u128) -> EdgeDefinition {
    EdgeDefinition::new(
        Endpoint::new(component(from), port("out")),
        Endpoint::new(component(to), port("in")),
    )
}

struct Fixture {
    producers: Arc<ProducerFactory>,
    consumers: Arc<ConsumerFactory>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            producers: Arc::new(ProducerFactory(factory_descriptor(
                "test/described-producer",
                ComponentRole::Source,
            ))),
            consumers: Arc::new(ConsumerFactory(factory_descriptor(
                "test/described-consumer",
                ComponentRole::Sink,
            ))),
        }
    }

    fn builder(&self) -> ComputationGraphBuilder {
        ComputationGraph::builder("component-descriptions")
    }

    fn producer(
        &self,
        builder: ComputationGraphBuilder,
        id: u128,
        identity: &str,
        instance: Option<&str>,
    ) -> ComputationGraphBuilder {
        builder
            .component(
                name(&format!("producer-{id}")),
                producer_spec(&self.producers, id, identity, instance),
                self.producers.clone(),
            )
            .bind_stream(
                Endpoint::new(component(id), port("out")),
                StreamId::try_new(format!("producer-{id}/out")).expect("stream"),
            )
    }

    fn consumer(
        &self,
        builder: ComputationGraphBuilder,
        id: u128,
        delivery: &str,
        store: Option<&str>,
    ) -> ComputationGraphBuilder {
        builder.component(
            name(&format!("consumer-{id}")),
            consumer_spec(&self.consumers, id, delivery, store),
            self.consumers.clone(),
        )
    }
}

fn connect(builder: ComputationGraphBuilder, from: u128, to: u128) -> ComputationGraphBuilder {
    builder.connect(edge(from, to), Box::new(BoundedPipeConfig { capacity: 2 }))
}

/// Run the graph through deployment, then let `check` observe and drive it.
async fn deployed<F, Fut>(mut graph: ComputationGraph, check: F)
where
    F: FnOnce(GraphControl) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let run = graph.run().expect("run");
    let control = run.control();
    let (outcome, ()) = tokio::join!(run, async {
        tokio::time::timeout(Duration::from_secs(10), async {
            control.deployment_report().await.expect("deployment");
            check(control.clone()).await;
        })
        .await
        .expect("check deadline");
        control.cancel();
    });
    assert!(matches!(outcome, Err(GraphError::Cancelled)), "{outcome:?}");
    graph.dispose().await.expect("cleanup");
}

fn assert_incompatible_edge(observed: &ObservedGraph, from: u128, to: u128) {
    let relationship = &observed.relationships[&edge(from, to)];
    assert_eq!(
        relationship.binding,
        BindingState::Failed,
        "{relationship:?}"
    );
    let consumer = &observed.components[&component(to)];
    let failure = consumer.failure.as_ref().expect("consumer binding failure");
    assert_eq!(failure.phase, FailurePhase::Binding);
    assert_eq!(failure.disposition, FailureDisposition::Terminal);
    assert!(
        matches!(
            failure.cause.underlying(),
            GraphError::Incompatible {
                source: Incompatibility::ProducerIdentity { .. },
                ..
            }
        ),
        "{failure:?}"
    );
    assert_eq!(consumer.realization, RealizationState::Blocked);
    let producer = &observed.components[&component(from)];
    assert!(producer.failure.is_none(), "{producer:?}");
}

fn assert_bound(observed: &ObservedGraph, from: u128, to: u128) {
    let relationship = &observed.relationships[&edge(from, to)];
    assert_eq!(
        relationship.binding,
        BindingState::Bound,
        "{relationship:?}"
    );
    assert!(observed.components[&component(to)].failure.is_none());
}

async fn incompatible_edge_fails_on_its_consumer_while_unrelated_components_bind() {
    let fixture = Fixture::new();
    let mut builder = fixture.builder();
    builder = fixture.producer(builder, 1, "volatile", None);
    builder = fixture.consumer(builder, 2, "exactlyOnce", None);
    builder = fixture.producer(builder, 3, "durable", None);
    builder = fixture.consumer(builder, 4, "exactlyOnce", None);
    builder = fixture.producer(builder, 5, "volatile", None);
    builder = fixture.consumer(builder, 6, "atLeastOnce", None);
    builder = fixture.producer(builder, 7, "relay", None);
    builder = fixture.consumer(builder, 8, "atLeastOnce", None);
    for (from, to) in [(1, 2), (3, 4), (5, 6), (7, 8)] {
        builder = connect(builder, from, to);
    }
    let graph = builder
        .build()
        .expect("an incompatible edge never rejects the graph");
    deployed(graph, |control| async move {
        let observed = control.observed();
        assert_incompatible_edge(&observed, 1, 2);
        assert_bound(&observed, 3, 4);
        assert_bound(&observed, 5, 6);
        assert_incompatible_edge(&observed, 7, 8);
        assert_eq!(
            observed.components.len(),
            8,
            "every node stays in the graph"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn incompatible_edge_fails_on_its_consumer_current_thread() {
    incompatible_edge_fails_on_its_consumer_while_unrelated_components_bind().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incompatible_edge_fails_on_its_consumer_multi_thread() {
    incompatible_edge_fails_on_its_consumer_while_unrelated_components_bind().await;
}

async fn reconcile(control: &GraphControl, mutations: Vec<DesiredMutation>) {
    let preview = control
        .preview(control.desired_snapshot().revision, mutations)
        .await
        .expect("preview");
    control
        .reconcile(preview, TopologyBindings::default())
        .await
        .expect("reconcile")
        .ensure_committed()
        .expect("committed");
}

fn desired_consumer(control: &GraphControl, fixture: &Fixture, delivery: &str) -> DesiredComponent {
    let mut desired = control
        .desired_snapshot()
        .select(GraphSelection::All)
        .expect("selection")
        .components
        .into_iter()
        .find(|node| node.descriptor.id() == &component(2))
        .expect("consumer");
    desired.construction =
        ComponentConstruction::Factory(consumer_spec(&fixture.consumers, 2, delivery, None));
    desired
}

async fn changing_either_side_clears_the_failure() {
    // Fixing the consumer's declaration through a managed replacement.
    let fixture = Fixture::new();
    let builder = fixture.consumer(
        fixture.producer(fixture.builder(), 1, "volatile", None),
        2,
        "exactlyOnce",
        None,
    );
    let graph = connect(builder, 1, 2).build().expect("graph");
    deployed(graph, |control| async move {
        assert_incompatible_edge(&control.observed(), 1, 2);
        reconcile(
            &control,
            vec![DesiredMutation::ReplaceComponent(desired_consumer(
                &control,
                &fixture,
                "atLeastOnce",
            ))],
        )
        .await;
        let observed = control.observed();
        assert_bound(&observed, 1, 2);
        assert!(observed.components[&component(2)].failure.is_none());
    })
    .await;

    // Removing the edge clears the consumer's failure too.
    let fixture = Fixture::new();
    let builder = fixture.consumer(
        fixture.producer(fixture.builder(), 1, "volatile", None),
        2,
        "exactlyOnce",
        None,
    );
    let graph = connect(builder, 1, 2).build().expect("graph");
    deployed(graph, |control| async move {
        assert_incompatible_edge(&control.observed(), 1, 2);
        reconcile(
            &control,
            vec![DesiredMutation::Unbind {
                edge: edge(1, 2),
                policy: RemovalPolicy::Reject,
            }],
        )
        .await;
        let observed = control.observed();
        assert!(!observed.relationships.contains_key(&edge(1, 2)));
        assert!(
            observed.components[&component(2)].failure.is_none(),
            "{:?}",
            observed.components[&component(2)]
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn changing_either_side_clears_the_failure_current_thread() {
    changing_either_side_clears_the_failure().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_either_side_clears_the_failure_multi_thread() {
    changing_either_side_clears_the_failure().await;
}

#[tokio::test(flavor = "current_thread")]
async fn binding_an_incompatible_edge_later_fails_without_rejecting_the_change() {
    let fixture = Fixture::new();
    let builder = fixture.consumer(
        fixture.producer(fixture.builder(), 1, "volatile", None),
        2,
        "exactlyOnce",
        None,
    );
    let graph = builder.build().expect("graph");
    deployed(graph, |control| async move {
        reconcile(
            &control,
            vec![DesiredMutation::Bind(DesiredRelationship {
                definition: edge(1, 2),
                policy: Default::default(),
                pipe: DesiredPipe::Bounded { capacity: 2 },
            })],
        )
        .await;
        assert_incompatible_edge(&control.observed(), 1, 2);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn constructed_components_declare_through_their_instance() {
    let fixture = Fixture::new();
    let descriptor = ComponentDescriptor::try_new(
        component(1),
        vec![PortDescriptor::new(
            port("out"),
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("descriptor");
    let source = Producer {
        description: producer_description(&descriptor, "volatile"),
        descriptor,
    };
    let builder = fixture
        .builder()
        .source(name("constructed"), Box::new(source))
        .bind_stream(
            Endpoint::new(component(1), port("out")),
            StreamId::try_new("constructed/out").expect("stream"),
        );
    let builder = fixture.consumer(builder, 2, "exactlyOnce", None);
    let graph = connect(builder, 1, 2).build().expect("graph");
    deployed(graph, |control| async move {
        assert_incompatible_edge(&control.observed(), 1, 2);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resource_bindings_are_checked_against_declared_requirements() {
    let fixture = Fixture::new();
    let store = ResourceId::try_new("store").expect("resource");
    let build = |capabilities: ResourceCapabilities| {
        fixture
            .consumer(fixture.builder(), 2, "unkeyed", Some("store"))
            .declare_resource(ResourceSpecification {
                id: store.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "store".into(),
            })
            .expect("declare")
            .provide_resource(
                store.clone(),
                ResourceHandle::new(ResourceRole::StateStore, Arc::new(Store))
                    .with_capabilities(capabilities),
            )
            .expect("provide")
            .build()
            .expect("graph")
    };
    for capabilities in [ResourceCapabilities::VOLATILE, ResourceCapabilities::UNDECLARED] {
        deployed(build(capabilities), |control| async move {
            let node = control.observed().components[&component(2)].clone();
            let failure = node.failure.expect("binding failure");
            assert_eq!(failure.phase, FailurePhase::Binding);
            assert_eq!(failure.disposition, FailureDisposition::Terminal);
            assert!(matches!(
                failure.cause.underlying(),
                GraphError::Incompatible {
                    source: Incompatibility::ResourceDurability { .. },
                    ..
                }
            ));
            assert_eq!(node.realization, RealizationState::Blocked);
        })
        .await;
    }
    deployed(build(ResourceCapabilities::DURABLE), |control| async move {
        let node = control.observed().components[&component(2)].clone();
        // Constructed and waiting only for its (absent) input binding.
        assert!(node.failure.is_none(), "{node:?}");
        assert_ne!(node.realization, RealizationState::CreationFailed);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_instance_that_differs_from_its_factory_declaration_fails_creation() {
    let fixture = Fixture::new();
    let graph = fixture
        .producer(fixture.builder(), 1, "durable", Some("volatile"))
        .build()
        .expect("graph");
    deployed(graph, |control| async move {
        let node = control.observed().components[&component(1)].clone();
        let failure = node.failure.expect("creation failure");
        assert_eq!(failure.phase, FailurePhase::Creation);
        assert_eq!(failure.disposition, FailureDisposition::Terminal);
        assert!(matches!(
            failure.cause.underlying(),
            GraphError::Incompatible {
                source: Incompatibility::InstanceDescription,
                ..
            }
        ));
        assert_eq!(node.realization, RealizationState::CreationFailed);
    })
    .await;
}

/// A query's outputs follow its index provider's declared durability: an
/// in-memory query cannot feed an exactly-once consumer, and an
/// at-least-once consumer accepts it.
#[tokio::test(flavor = "current_thread")]
async fn queries_declare_through_their_index_provider() {
    for (delivery, compatible) in [("exactlyOnce", false), ("atLeastOnce", true)] {
        let fixture = Fixture::new();
        let queries = Arc::new(ContinuousQueryFactory::default());
        let mut query = queries
            .specification_from_config(
                component(10),
                json!({"query": "MATCH (n:items) RETURN n.id AS id", "stream": "query/out"}),
            )
            .expect("query");
        let memory = ResourceId::try_new("memory").expect("resource");
        query
            .dependencies
            .insert("indexes".into(), vec![memory.clone()]);
        let descriptor = ComponentDescriptor::try_new(
            component(1),
            vec![PortDescriptor::new(
                port("out"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )
        .expect("descriptor");
        let feed = Producer {
            description: producer_description(&descriptor, "untracked"),
            descriptor,
        };
        let builder = fixture
            .builder()
            .declare_resource(ResourceSpecification {
                id: memory.clone(),
                role: ResourceRole::IndexBackend,
                ownership: ResourceOwnership::Borrowed,
                binding: "memory".into(),
            })
            .expect("declare")
            .provide_resource(
                memory,
                QueryIndexProviderResource::handle(
                    ResourceRole::IndexBackend,
                    Arc::new(drasi_core::computation::InMemoryComputationProvider),
                ),
            )
            .expect("provide")
            .source(name("feed"), Box::new(feed))
            .bind_stream(
                Endpoint::new(component(1), port("out")),
                StreamId::try_new("feed/out").expect("stream"),
            )
            .connect(
                EdgeDefinition::new(
                    Endpoint::new(component(1), port("out")),
                    Endpoint::new(component(10), port("in")),
                ),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            )
            .component(name("query"), query, queries)
            .bind_stream(
                Endpoint::new(component(10), port("out")),
                StreamId::try_new("query/out").expect("stream"),
            );
        let builder = fixture.consumer(builder, 2, delivery, None);
        let graph = connect(builder, 10, 2).build().expect("graph");
        deployed(graph, move |control| async move {
            let observed = control.observed();
            if compatible {
                assert_bound(&observed, 10, 2);
            } else {
                assert_incompatible_edge(&observed, 10, 2);
            }
        })
        .await;
    }
}
