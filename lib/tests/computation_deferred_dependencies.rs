// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use drasi_lib::computation::v1::*;

fn component(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("component ID")
}

fn resource(value: &str) -> ResourceId {
    ResourceId::try_new(value).expect("resource ID")
}

struct State(u64);

struct Service {
    descriptor: ComponentDescriptor,
    starts: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
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

struct Factory {
    descriptor: FactoryDescriptor,
    creates: Arc<AtomicUsize>,
    starts: Arc<AtomicUsize>,
    fail: AtomicBool,
}

impl Factory {
    fn new(creates: Arc<AtomicUsize>, starts: Arc<AtomicUsize>) -> Arc<Self> {
        Arc::new(Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test/wait-for-state", "1")
                    .expect("implementation"),
                role: ComponentRole::Service,
                configuration_version: 1,
                configuration: ConfigurationSchema::default(),
                dependencies: BTreeMap::from([(
                    Arc::from("state"),
                    ResourceRequirement::exactly_one::<State>(ResourceRole::StateStore),
                )]),
            },
            creates,
            starts,
            fail: AtomicBool::new(false),
        })
    }

    fn addition(self: &Arc<Self>, auto_start: bool) -> ComponentAddition {
        let mut addition = ComponentAddition::from_specification(
            ComponentSpecification {
                descriptor: ComponentDescriptor::try_new(component("waiting"), Vec::new())
                    .expect("service descriptor"),
                role: ComponentRole::Service,
                completion: None,
                implementation: self.descriptor.implementation.clone(),
                configuration_version: 1,
                configuration: BTreeMap::new(),
                dependencies: BTreeMap::from([(Arc::from("state"), vec![resource("later")])]),
            },
            self.clone(),
        );
        addition.definition.lifecycle.auto_start = auto_start;
        addition
    }
}

#[async_trait]
impl ComponentFactory for Factory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let state = context
            .resources::<State>("state")
            .map_err(ComponentCreationError::terminal)?;
        assert_eq!(state[0].0, 42);
        self.creates.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(ComponentCreationError::retryable(anyhow::anyhow!(
                "injected constructor failure"
            )));
        }
        Ok(ConstructedComponent::service(Box::new(Service {
            descriptor: context.specification.descriptor.clone(),
            starts: self.starts.clone(),
        })))
    }
}

async fn wait_for_creation_state(control: &GraphControl, id: &ComponentId) -> ObservedComponent {
    let mut observed = control.subscribe_observed();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let current = observed.borrow_and_update().components.get(id).cloned();
            if let Some(current) = current {
                if matches!(
                    current.realization,
                    RealizationState::Created
                        | RealizationState::Blocked
                        | RealizationState::CreationFailed
                ) {
                    return current;
                }
            }
            observed.changed().await.expect("graph remains open");
        }
    })
    .await
    .expect("creation state deadline")
}

async fn verify_late_resource() {
    for (auto_start, explicit_stop) in [(true, false), (false, false), (true, true)] {
        let creates = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(AtomicUsize::new(0));
        let factory = Factory::new(creates.clone(), starts.clone());
        let mut graph = ComputationGraph::empty("late-resource").expect("graph");
        let run = graph.run().expect("run");
        let control = run.control();
        let (outcome, ()) = tokio::join!(run, async {
            control.deployment_report().await.expect("deployment");
            let waiting = control
                .add_component(factory.addition(auto_start))
                .await
                .expect("valid declaration is accepted");
            let observed = wait_for_creation_state(&control, waiting.id()).await;
            assert_eq!(observed.realization, RealizationState::Blocked);
            assert!(observed.failure.is_none(), "{observed:?}");
            assert_eq!(
                observed.waiting_for,
                [DependencyWait::Resource {
                    id: resource("later")
                }]
            );
            assert_eq!(
                observed.waiting_reason().as_deref(),
                Some("Waiting for resource 'later'")
            );
            assert_eq!(creates.load(Ordering::SeqCst), 0);
            let report = control
                .deployment_report()
                .await
                .expect("current deployment");
            assert!(matches!(
                report.components.get(waiting.id()),
                Some(CreationOutcome::Blocked { dependencies, resources })
                    if dependencies.is_empty() && resources == &[resource("later")]
            ));

            let independent_starts = Arc::new(AtomicUsize::new(0));
            let independent = control
                .add_component(ComponentAddition::new(ConstructedComponent::service(
                    Box::new(Service {
                        descriptor: ComponentDescriptor::try_new(
                            component("independent"),
                            Vec::new(),
                        )
                        .expect("descriptor"),
                        starts: independent_starts.clone(),
                    }),
                )))
                .await
                .expect("independent addition");
            tokio::time::timeout(Duration::from_secs(3), independent.wait_started())
                .await
                .expect("independent startup deadline")
                .expect("independent startup");
            assert_eq!(independent_starts.load(Ordering::SeqCst), 1);
            if explicit_stop {
                waiting.stop().await.expect("stop waiting declaration");
            }

            let preview = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::PutResource(ResourceSpecification {
                        id: resource("later"),
                        role: ResourceRole::StateStore,
                        ownership: ResourceOwnership::Borrowed,
                        binding: Arc::from("later"),
                    })],
                )
                .await
                .expect("declare missing resource");
            let mut bindings = TopologyBindings::default();
            bindings.resources.insert(
                resource("later"),
                ResourceHandle::new(ResourceRole::StateStore, Arc::new(State(42))),
            );
            let report = control
                .reconcile(preview, bindings)
                .await
                .expect("supply resource");
            assert!(report.committed, "{report:?}");
            let current = control
                .component_handle(waiting.id())
                .expect("current generation");
            tokio::time::timeout(Duration::from_secs(3), current.wait_created())
                .await
                .expect("deferred creation deadline")
                .expect("deferred creation");
            assert!(current
                .observed()
                .expect("observation")
                .waiting_for
                .is_empty());
            assert_eq!(creates.load(Ordering::SeqCst), 1);
            if auto_start && !explicit_stop {
                tokio::time::timeout(Duration::from_secs(3), current.wait_started())
                    .await
                    .expect("automatic startup deadline")
                    .expect("automatic startup");
                assert_eq!(starts.load(Ordering::SeqCst), 1);
            } else {
                assert_eq!(
                    current.observed().expect("observation").lifecycle,
                    ComponentLifecycle::Stopped,
                    "auto_start={auto_start}, explicit_stop={explicit_stop}"
                );
                assert_eq!(starts.load(Ordering::SeqCst), 0);
            }
            control.cancel();
        });
        assert!(matches!(outcome, Err(GraphError::Cancelled)));
        graph.dispose().await.expect("cleanup");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_resources_wait_and_resume_without_overriding_stop_current_thread() {
    verify_late_resource().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_resources_wait_and_resume_without_overriding_stop_multi_thread() {
    verify_late_resource().await;
}

async fn verify_unresolved_reconstruction() {
    let creates = Arc::new(AtomicUsize::new(0));
    let starts = Arc::new(AtomicUsize::new(0));
    let factory = Factory::new(creates.clone(), starts.clone());
    let addition = factory.addition(true);
    let ComponentConstruction::Factory(specification) = addition.definition.construction else {
        panic!("factory declaration");
    };
    let original = ComputationGraph::builder("deferred-reconstruction")
        .component(specification, factory.clone())
        .build()
        .expect("missing provider does not invalidate a declaration");
    let saved = original
        .snapshot()
        .select(GraphSelection::All)
        .expect("snapshot");
    saved
        .validate_structure()
        .expect("unresolved graph is valid");
    let encoded = serde_json::to_vec(&saved).expect("serialize current definition");
    let saved: DesiredTopology =
        serde_json::from_slice(&encoded).expect("restore current definition");
    let mut graph = saved
        .build(TopologyBindings::default())
        .expect("missing factory is deferred");
    assert_eq!(
        graph
            .snapshot()
            .select(GraphSelection::All)
            .expect("rebuilt snapshot"),
        saved
    );
    let run = graph.run().expect("run");
    let control = run.control();
    let (outcome, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        let waiting = control
            .component_handle(&component("waiting"))
            .expect("node");
        let observed = waiting.observed().expect("observation");
        assert_eq!(observed.realization, RealizationState::Blocked);
        assert_eq!(
            observed.waiting_for,
            [
                DependencyWait::Resource {
                    id: resource("later")
                },
                DependencyWait::Factory {
                    implementation: factory.descriptor.implementation.clone()
                },
            ]
        );
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::PutResource(ResourceSpecification {
                    id: resource("later"),
                    role: ResourceRole::StateStore,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from("later"),
                })],
            )
            .await
            .expect("resource declaration");
        let mut bindings = TopologyBindings::default();
        bindings.resources.insert(
            resource("later"),
            ResourceHandle::new(ResourceRole::StateStore, Arc::new(State(42))),
        );
        control
            .reconcile(preview, bindings)
            .await
            .expect("bind resource");
        let observed = control
            .component_handle(waiting.id())
            .expect("node")
            .observed()
            .expect("state");
        assert_eq!(
            observed.waiting_for,
            [DependencyWait::Factory {
                implementation: factory.descriptor.implementation.clone(),
            }]
        );
        assert_eq!(creates.load(Ordering::SeqCst), 0);
        let preview = control
            .preview(control.desired_snapshot().revision, vec![])
            .await
            .expect("factory registration preview");
        let mut bindings = TopologyBindings::default();
        bindings
            .factories
            .register(factory.clone())
            .expect("factory registration");
        control
            .reconcile(preview, bindings)
            .await
            .expect("register implementation");
        let current = control.component_handle(waiting.id()).expect("node");
        tokio::time::timeout(Duration::from_secs(3), current.wait_created())
            .await
            .expect("creation deadline")
            .expect("created without manual retry");
        assert_eq!(creates.load(Ordering::SeqCst), 1);
        control.cancel();
    });
    assert!(matches!(outcome, Err(GraphError::Cancelled)));
    graph.dispose().await.expect("cleanup");
}

#[tokio::test(flavor = "current_thread")]
async fn unresolved_definition_reopens_and_resumes_when_factory_arrives_current_thread() {
    verify_unresolved_reconstruction().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unresolved_definition_reopens_and_resumes_when_factory_arrives_multi_thread() {
    verify_unresolved_reconstruction().await;
}

async fn verify_no_failure_retry() {
    let creates = Arc::new(AtomicUsize::new(0));
    let factory = Factory::new(creates.clone(), Arc::new(AtomicUsize::new(0)));
    factory.fail.store(true, Ordering::SeqCst);
    let mut graph = ComputationGraph::empty("constructor-failure").expect("graph");
    let run = graph.run().expect("run");
    let control = run.control();
    let (outcome, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        let mut addition = factory.addition(true);
        addition.resources.push(ResourceSpecification {
            id: resource("later"),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "later".into(),
        });
        addition.bindings.resources.insert(
            resource("later"),
            ResourceHandle::new(ResourceRole::StateStore, Arc::new(State(42))),
        );
        let failed = control.add_component(addition).await.expect("declaration");
        timeout_created_failure(&failed).await;
        factory.fail.store(false, Ordering::SeqCst);
        for index in 0..3 {
            let healthy = control
                .add_component(ComponentAddition::new(ConstructedComponent::service(
                    Box::new(Service {
                        descriptor: ComponentDescriptor::try_new(
                            component(&format!("healthy-{index}")),
                            Vec::new(),
                        )
                        .expect("descriptor"),
                        starts: Arc::new(AtomicUsize::new(0)),
                    }),
                )))
                .await
                .expect("unrelated declaration");
            tokio::time::timeout(Duration::from_secs(3), healthy.wait_started())
                .await
                .expect("unrelated startup deadline")
                .expect("unrelated startup");
        }
        assert_eq!(
            creates.load(Ordering::SeqCst),
            1,
            "dependency activity must not retry a real failure"
        );
        assert!(failed.observed().expect("failure state").failure.is_some());
        control.cancel();
    });
    assert!(matches!(outcome, Err(GraphError::Cancelled)));
    graph.dispose().await.expect("cleanup");
}

async fn timeout_created_failure(handle: &ComponentHandle) {
    let error = tokio::time::timeout(Duration::from_secs(3), handle.wait_created())
        .await
        .expect("creation deadline")
        .expect_err("constructor fails");
    assert!(format!("{error:#}").contains("injected constructor failure"));
}

#[tokio::test(flavor = "current_thread")]
async fn dependency_events_do_not_retry_genuine_failures_current_thread() {
    verify_no_failure_retry().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dependency_events_do_not_retry_genuine_failures_multi_thread() {
    verify_no_failure_retry().await;
}

struct Producer {
    descriptor: ComponentDescriptor,
    output: Option<OutputEnvelope>,
}

#[async_trait]
impl ComputationComponent for Producer {
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

#[async_trait]
impl EnvelopeSource for Producer {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        match self.output.take() {
            Some(output) => Ok(Some(output)),
            None => std::future::pending().await,
        }
    }
}

struct Consumer {
    descriptor: ComponentDescriptor,
    output: tokio::sync::mpsc::Sender<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for Consumer {
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

#[async_trait]
impl EnvelopeSink for Consumer {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.output.send(input.envelope).await?;
        Ok(())
    }
}

async fn verify_late_connection() {
    use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};
    let output = PortId::try_new("out").expect("port");
    let input = PortId::try_new("in").expect("port");
    let stream = StreamId::try_new("producer/out").expect("stream");
    let change = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("producer", "exact-row"),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: 17,
        },
    };
    let mut graph = ComputationGraph::builder("late-connection")
        .source(Box::new(Producer {
            descriptor: ComponentDescriptor::try_new(
                component("producer"),
                vec![PortDescriptor::new(
                    output.clone(),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("source descriptor"),
            output: Some(OutputEnvelope {
                port: output.clone(),
                envelope: GraphChangeCodec::encode_change(change.clone(), stream.clone(), 1, None)
                    .expect("change"),
            }),
        }))
        .bind_stream(Endpoint::new(component("producer"), output.clone()), stream)
        .build()
        .expect("an unconnected component is a valid declaration");
    let definition = graph
        .snapshot()
        .select(GraphSelection::All)
        .expect("definition");
    definition
        .validate_structure()
        .expect("unconnected definition");
    let value = serde_json::to_value(&definition).expect("serialization");
    assert!(value.get("allow_incomplete").is_none());
    let mut obsolete = value.clone();
    obsolete["allow_incomplete"] = true.into();
    assert!(serde_json::from_value::<DesiredTopology>(obsolete).is_err());
    let restored: DesiredTopology = serde_json::from_value(value).expect("current definition");
    assert_eq!(restored, definition);
    let run = graph.start().expect("run");
    let control = run.control();
    let (outcome, ()) = tokio::join!(run, async {
        control.startup_report().await.expect("startup status");
        let producer = control
            .component_handle(&component("producer"))
            .expect("producer");
        assert!(producer.observed().expect("state").waiting_for.contains(
            &DependencyWait::Connection {
                endpoint: Endpoint::new(component("producer"), output.clone()),
            }
        ));
        let (send, mut receive) = tokio::sync::mpsc::channel(1);
        control
            .add_component(ComponentAddition::new(ConstructedComponent::sink(
                Box::new(Consumer {
                    descriptor: ComponentDescriptor::try_new(
                        component("consumer"),
                        vec![PortDescriptor::new(
                            input.clone(),
                            PortDirection::Input,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )
                    .expect("sink descriptor"),
                    output: send,
                }),
            )))
            .await
            .expect("sink declaration");
        control
            .connect(
                EdgeDefinition::new(
                    Endpoint::new(component("producer"), output),
                    Endpoint::new(component("consumer"), input),
                ),
                Box::new(BoundedPipeConfig { capacity: 2 }),
                RelationshipPolicy::default(),
            )
            .await
            .expect("connect declared components");
        let envelope = tokio::time::timeout(Duration::from_secs(3), receive.recv())
            .await
            .expect("automatic processing deadline")
            .expect("delivered change");
        assert_eq!(
            GraphChangeCodec::decode_changes(&envelope).expect("decode"),
            vec![change]
        );
        assert!(producer.observed().expect("state").waiting_for.is_empty());
        control.cancel();
    });
    assert!(matches!(outcome, Err(GraphError::Cancelled)));
    graph.dispose().await.expect("cleanup");
}

#[tokio::test(flavor = "current_thread")]
async fn unconnected_nodes_wait_and_start_when_connected_current_thread() {
    verify_late_connection().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unconnected_nodes_wait_and_start_when_connected_multi_thread() {
    verify_late_connection().await;
}
