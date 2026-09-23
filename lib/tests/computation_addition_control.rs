// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(test)]

use async_trait::async_trait;
use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};
use drasi_lib::computation::v1::*;
use drasi_lib::context::{ComponentResource, ComponentResourceObserver};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).unwrap()
}

fn port(value: &str) -> PortId {
    PortId::try_new(value).unwrap()
}

fn stream(value: &str) -> StreamId {
    StreamId::try_new(value).unwrap()
}

fn descriptor(name: &str, input: bool, output: bool) -> ComponentDescriptor {
    let mut ports = Vec::new();
    for (enabled, name, direction) in
        [(input, "in", PortDirection::Input), (output, "out", PortDirection::Output)]
    {
        if enabled {
            ports.push(PortDescriptor::new(
                port(name),
                direction,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            ));
        }
    }
    ComponentDescriptor::try_new(id(name), ports).unwrap()
}

struct Service {
    descriptor: ComponentDescriptor,
    starts: Arc<AtomicUsize>,
    fail: bool,
}

impl Service {
    fn new(name: &str, starts: Arc<AtomicUsize>) -> Self {
        Self {
            descriptor: descriptor(name, false, false),
            starts,
            fail: false,
        }
    }
}

#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        if self.fail {
            anyhow::bail!("injected startup failure");
        }
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

fn service(name: &str) -> ComponentAddition {
    ComponentAddition::new(ConstructedComponent::service(Box::new(Service::new(
        name,
        Arc::new(AtomicUsize::new(0)),
    ))))
}

#[tokio::test]
async fn addition_reports_only_graph_rejection_and_waits_report_node_failures() {
    let mut graph = ComputationGraph::empty("additions").unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let mut invalid = service("invalid");
        invalid.definition.role = ComponentRole::Source;
        let invalid = control.add_component(invalid).await.unwrap();
        assert!(invalid.wait_created().await.is_err());
        assert_eq!(
            invalid.observed().unwrap().failure.unwrap().phase,
            FailurePhase::Validation
        );
        assert!(control.add_component(service("invalid")).await.is_err());
        assert!(control
            .desired_snapshot()
            .nodes
            .iter()
            .any(|node| node.descriptor.id() == &id("invalid")));

        let mut failing = Service::new("failing", Arc::new(AtomicUsize::new(0)));
        failing.fail = true;
        let failing = control
            .add_component(ComponentAddition::new(ConstructedComponent::service(
                Box::new(failing),
            )))
            .await
            .unwrap();
        assert!(failing.wait_started().await.is_err());
        assert_eq!(
            failing.observed().unwrap().failure.unwrap().phase,
            FailurePhase::Activation
        );
        let healthy = control.add_component(service("healthy")).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), healthy.wait_started())
            .await
            .unwrap()
            .unwrap();
        healthy.stop().await.unwrap();
        healthy.wait_started().await.unwrap();
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

struct PendingFactory {
    descriptor: FactoryDescriptor,
    entered: Arc<Notify>,
}

#[async_trait]
impl ComponentFactory for PendingFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        _: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn pending_construction_does_not_block_additions_and_can_be_stopped() {
    let mut graph = ComputationGraph::empty("pending").unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let entered = Arc::new(Notify::new());
    let implementation = ImplementationIdentity::try_new("test/pending", "1").unwrap();
    let factory = Arc::new(PendingFactory {
        descriptor: FactoryDescriptor {
            implementation: implementation.clone(),
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: ConfigurationSchema::default(),
            dependencies: BTreeMap::new(),
        },
        entered: entered.clone(),
    });
    let specification = ComponentSpecification {
        descriptor: descriptor("pending", false, false),
        role: ComponentRole::Service,
        completion: None,
        implementation,
        configuration_version: 1,
        configuration: BTreeMap::new(),
        dependencies: BTreeMap::new(),
    };
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let pending = control
            .add_component(ComponentAddition::from_specification(
                specification,
                factory,
            ))
            .await
            .unwrap();
        entered.notified().await;
        let healthy = tokio::time::timeout(
            Duration::from_secs(2),
            control.add_component(service("unrelated")),
        )
        .await
        .unwrap()
        .unwrap();
        healthy.wait_started().await.unwrap();
        assert_eq!(
            pending.observed().unwrap().realization,
            RealizationState::Creating
        );
        tokio::time::timeout(Duration::from_secs(2), pending.stop())
            .await
            .unwrap()
            .unwrap();
        assert!(pending.wait_created().await.is_err());
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn readiness_autostart_waits_for_connected_downstream_not_unrelated_components() {
    let upstream_starts = Arc::new(AtomicUsize::new(0));
    let mut graph = ComputationGraph::empty("readiness").unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let upstream = control
            .add_component(
                ComponentAddition::new(ConstructedComponent::service(Box::new(Service::new(
                    "upstream",
                    upstream_starts.clone(),
                ))))
                .require_downstream_ready(),
            )
            .await
            .unwrap();
        let downstream = control
            .add_component(service("downstream").auto_start(false))
            .await
            .unwrap();
        let unrelated = control
            .add_component(service("unrelated").auto_start(false))
            .await
            .unwrap();
        upstream.wait_created().await.unwrap();
        downstream.wait_created().await.unwrap();
        unrelated.wait_created().await.unwrap();
        control
            .set_control_connections(vec![(id("upstream"), id("downstream"))])
            .await
            .unwrap();
        assert_eq!(upstream_starts.load(Ordering::SeqCst), 0);
        unrelated.control().unwrap().ready().unwrap();
        assert_eq!(upstream_starts.load(Ordering::SeqCst), 0);
        downstream.start().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), upstream.wait_started())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(upstream_starts.load(Ordering::SeqCst), 1);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

struct Recorder(mpsc::UnboundedSender<PeerMessage>);

#[async_trait]
impl ControlHandler for Recorder {
    async fn on_message(&self, message: PeerMessage, _: ComponentControl) -> anyhow::Result<()> {
        self.0.send(message)?;
        Ok(())
    }
}

struct InputSource {
    descriptor: ComponentDescriptor,
    input: mpsc::Receiver<OutputEnvelope>,
    control: Arc<Mutex<Option<ComponentControl>>>,
    produced: Arc<AtomicUsize>,
    progressed: Arc<Notify>,
}

#[async_trait]
impl ComputationComponent for InputSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn bind_control(&mut self, control: ComponentControl) {
        *self.control.lock().unwrap() = Some(control);
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for InputSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let result = self.input.recv().await;
        if result.is_some() {
            self.produced.fetch_add(1, Ordering::SeqCst);
            self.progressed.notify_one();
        }
        Ok(result)
    }
}

struct BlockedSink {
    descriptor: ComponentDescriptor,
    entered: Arc<Notify>,
    handler: Arc<Recorder>,
}

#[async_trait]
impl ComputationComponent for BlockedSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        Some(self.handler.clone())
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for BlockedSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        self.entered.notify_one();
        std::future::pending().await
    }
}

fn event(sequence: u64) -> OutputEnvelope {
    OutputEnvelope {
        port: port("out"),
        envelope: GraphChangeCodec::encode_change(
            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &sequence.to_string()),
                    labels: Arc::from([]),
                    effective_from: sequence,
                },
            },
            stream("source/out"),
            sequence,
            None,
        )
        .unwrap(),
    }
}

#[tokio::test]
async fn control_notifications_bypass_a_full_data_pipe_and_a_blocked_data_handler() {
    let (input, receiver) = mpsc::channel(3);
    let (received, mut messages) = mpsc::unbounded_channel();
    let source_control = Arc::new(Mutex::new(None));
    let produced = Arc::new(AtomicUsize::new(0));
    let progressed = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let mut graph = ComputationGraph::builder("out-of-band")
        .source(Box::new(InputSource {
            descriptor: descriptor("source", false, true),
            input: receiver,
            control: source_control.clone(),
            produced: produced.clone(),
            progressed: progressed.clone(),
        }))
        .sink(Box::new(BlockedSink {
            descriptor: descriptor("sink", true, false),
            entered: entered.clone(),
            handler: Arc::new(Recorder(received)),
        }))
        .service(Box::new(Service::new(
            "unrelated",
            Arc::new(AtomicUsize::new(0)),
        )))
        .bind_stream(
            Endpoint::new(id("source"), port("out")),
            stream("source/out"),
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(id("source"), port("out")),
                Endpoint::new(id("sink"), port("in")),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap();
        input.send(event(1)).await.unwrap();
        entered.notified().await;
        input.send(event(2)).await.unwrap();
        input.send(event(3)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while produced.load(Ordering::SeqCst) < 3 {
                progressed.notified().await;
            }
        })
        .await
        .unwrap();
        let sender = source_control.lock().unwrap().clone().unwrap();
        assert!(sender
            .notify_neighbor(&id("unrelated"), ControlNotification::Available)
            .is_err());
        sender
            .notify_downstream(ControlNotification::Unavailable {
                reason: "connection lost while data is blocked".into(),
            })
            .unwrap();
        let notification = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let message = messages.recv().await.unwrap();
                if matches!(
                    message.notification,
                    ControlNotification::Unavailable { .. }
                ) {
                    break message;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(notification.from, id("source"));
        assert_eq!(produced.load(Ordering::SeqCst), 3);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn removing_and_readding_an_id_invalidates_old_component_and_control_handles() {
    let mut graph = ComputationGraph::empty("generation").unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let original = control.add_component(service("component")).await.unwrap();
        original.wait_started().await.unwrap();
        let old_sender = original.control().unwrap();
        original.stop().await.unwrap();
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::RemoveComponents {
                    selection: GraphSelection::Exact(vec![id("component")]),
                    policy: RemovalPolicy::Reject,
                }],
            )
            .await
            .unwrap();
        assert!(
            control
                .reconcile(preview, TopologyBindings::default())
                .await
                .unwrap()
                .committed
        );
        let replacement = control.add_component(service("component")).await.unwrap();
        replacement.wait_started().await.unwrap();
        assert_ne!(original.generation(), replacement.generation());
        assert!(matches!(
            original.wait_started().await,
            Err(GraphError::StaleGeneration)
        ));
        assert!(matches!(
            original.stop().await,
            Err(GraphError::StaleGeneration)
        ));
        assert!(old_sender.ready().is_err());
        assert_eq!(
            replacement.observed().unwrap().lifecycle,
            ComponentLifecycle::Running
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

struct Passthrough {
    descriptor: ComponentDescriptor,
}

#[async_trait]
impl ComputationComponent for Passthrough {
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
impl Transformer for Passthrough {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        let sequence = input.envelope.system().sequence();
        let envelope = GraphChangeCodec::encode_change(
            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("transformer", &sequence.to_string()),
                    labels: Arc::from([]),
                    effective_from: sequence,
                },
            },
            stream("transformer/out"),
            sequence,
            None,
        )?;
        Ok(vec![OutputEnvelope {
            port: port("out"),
            envelope,
        }])
    }
}

struct CountingSink {
    descriptor: ComponentDescriptor,
    output: mpsc::UnboundedSender<u64>,
}

#[async_trait]
impl ComputationComponent for CountingSink {
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
impl EnvelopeSink for CountingSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.output.send(input.envelope.system().sequence())?;
        Ok(())
    }
}

#[tokio::test]
async fn individual_source_transformer_sink_additions_can_be_connected_incrementally() {
    let (input, receiver) = mpsc::channel(2);
    let (output, mut results) = mpsc::unbounded_channel();
    let mut graph = ComputationGraph::empty("incremental").unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let source = control
            .add_component(
                ComponentAddition::new(ConstructedComponent::source(Box::new(InputSource {
                    descriptor: descriptor("source", false, true),
                    input: receiver,
                    control: Arc::new(Mutex::new(None)),
                    produced: Arc::new(AtomicUsize::new(0)),
                    progressed: Arc::new(Notify::new()),
                })))
                .bind_stream(port("out"), stream("source/out"))
                .require_downstream_ready(),
            )
            .await
            .unwrap();
        let transformer = control
            .add_component(
                ComponentAddition::new(ConstructedComponent::transformer(Box::new(Passthrough {
                    descriptor: descriptor("transformer", true, true),
                })))
                .bind_stream(port("out"), stream("transformer/out"))
                .require_downstream_ready(),
            )
            .await
            .unwrap();
        let sink = control
            .add_component(ComponentAddition::new(ConstructedComponent::sink(
                Box::new(CountingSink {
                    descriptor: descriptor("sink", true, false),
                    output,
                }),
            )))
            .await
            .unwrap();
        for (from, to) in [("source", "transformer"), ("transformer", "sink")] {
            let report = control
                .connect(
                    EdgeDefinition::new(
                        Endpoint::new(id(from), port("out")),
                        Endpoint::new(id(to), port("in")),
                    ),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                    RelationshipPolicy::default(),
                )
                .await
                .unwrap();
            assert!(report.committed, "{report:?}");
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            source.wait_started().await.unwrap();
            transformer.wait_started().await.unwrap();
            sink.wait_started().await.unwrap();
        })
        .await
        .unwrap();
        input.send(event(1)).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), results.recv())
                .await
                .unwrap(),
            Some(1),
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn shared_provider_instances_have_one_node_and_live_dependency_links() {
    let mut graph = ComputationGraph::empty("providers").unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let provider = Arc::new(drasi_lib::identity::PasswordIdentityProvider::new(
        "user", "private",
    ));
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let first = control
            .add_component(service("first").auto_start(false))
            .await
            .unwrap();
        let second = control
            .add_component(service("second").auto_start(false))
            .await
            .unwrap();
        let first_observer =
            GraphResourceObserver::new(control.clone(), first.id().clone(), first.generation());
        let second_observer =
            GraphResourceObserver::new(control.clone(), second.id().clone(), second.generation());
        first_observer
            .observe(vec![ComponentResource::Identity(provider.clone())])
            .await
            .unwrap();
        second_observer
            .observe(vec![ComponentResource::Identity(provider.clone())])
            .await
            .unwrap();
        let mut changes = control.subscribe_observed();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                changes.borrow_and_update();
                let desired = control.desired_snapshot();
                if desired
                    .component_resources
                    .get(&id("second"))
                    .is_some_and(|resources| {
                        resources.iter().any(|resource| {
                            desired.resources[resource].role == ResourceRole::Identity
                        })
                    })
                {
                    break;
                }
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let desired = control.desired_snapshot();
        assert_eq!(
            desired
                .resources
                .values()
                .filter(|resource| resource.role == ResourceRole::Identity)
                .count(),
            1
        );
        assert_eq!(
            desired.component_resources[&id("first")],
            desired.component_resources[&id("second")]
        );
        drop(desired);
        let replacement = Arc::new(drasi_lib::identity::PasswordIdentityProvider::new(
            "other", "private",
        ));
        first_observer
            .observe(vec![ComponentResource::Identity(replacement)])
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                changes.borrow_and_update();
                let desired = control.desired_snapshot();
                if desired.component_resources[&id("first")]
                    != desired.component_resources[&id("second")]
                {
                    assert_eq!(
                        desired
                            .resources
                            .values()
                            .filter(|resource| resource.role == ResourceRole::Identity)
                            .count(),
                        2
                    );
                    break;
                }
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        first_observer.observe(Vec::new()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                changes.borrow_and_update();
                let desired = control.desired_snapshot();
                if desired.component_resources[&id("first")].is_empty() {
                    assert_eq!(
                        desired
                            .resources
                            .values()
                            .filter(|resource| resource.role == ResourceRole::Identity)
                            .count(),
                        1
                    );
                    assert!(!desired.component_resources[&id("second")].is_empty());
                    break;
                }
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        for name in ["first", "second"] {
            let preview = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::RemoveComponents {
                        selection: GraphSelection::Exact(vec![id(name)]),
                        policy: RemovalPolicy::Reject,
                    }],
                )
                .await
                .unwrap();
            assert!(
                control
                    .reconcile(preview, TopologyBindings::default())
                    .await
                    .unwrap()
                    .committed
            );
            assert_eq!(
                control
                    .desired_snapshot()
                    .resources
                    .values()
                    .filter(|resource| resource.role == ResourceRole::Identity)
                    .count(),
                usize::from(name == "first"),
            );
        }
        assert!(first_observer
            .observe(vec![ComponentResource::Identity(provider)])
            .await
            .is_err());
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

struct ConfirmedService(Service);

#[async_trait]
impl ComputationComponent for ConfirmedService {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.0.descriptor()
    }
    fn requires_readiness_confirmation(&self) -> bool {
        true
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.0.start().await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.0.stop().await
    }
}

#[async_trait]
impl ComputationService for ConfirmedService {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn startup_wait_requires_actual_readiness_and_resets_on_a_new_start() {
    let mut graph = ComputationGraph::empty("confirmed-start").unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let component = control
            .add_component(ComponentAddition::new(ConstructedComponent::service(
                Box::new(ConfirmedService(Service::new(
                    "component",
                    Arc::new(AtomicUsize::new(0)),
                ))),
            )))
            .await
            .unwrap();
        let mut changes = control.subscribe_observed();
        changes
            .wait_for(|state| {
                state.components[&id("component")].lifecycle == ComponentLifecycle::Starting
            })
            .await
            .unwrap();
        assert!(!component.observed().unwrap().started);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), component.wait_started())
                .await
                .is_err()
        );
        component.control().unwrap().ready().unwrap();
        component.wait_started().await.unwrap();
        component.stop().await.unwrap();
        component.wait_started().await.unwrap();
        let (started, ()) = tokio::join!(component.start(), async {
            changes
                .wait_for(|state| {
                    let node = &state.components[&id("component")];
                    node.lifecycle == ComponentLifecycle::Starting && !node.started
                })
                .await
                .unwrap();
            component.control().unwrap().ready().unwrap();
        });
        started.unwrap();
        assert_eq!(
            component.observed().unwrap().lifecycle,
            ComponentLifecycle::Running
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

struct CountCleanup(Arc<AtomicUsize>);

#[async_trait]
impl ResourceCleanup for CountCleanup {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn resource_spec(name: &str, ownership: ResourceOwnership) -> ResourceSpecification {
    ResourceSpecification {
        id: ResourceId::try_new(name).unwrap(),
        role: ResourceRole::StateStore,
        ownership,
        binding: Arc::from(name),
    }
}

#[tokio::test]
async fn rejected_addition_retains_cleanup_and_never_shuts_down_existing_or_borrowed_resources() {
    let existing_cleanup = Arc::new(AtomicUsize::new(0));
    let incoming_cleanup = Arc::new(AtomicUsize::new(0));
    let borrowed_cleanup = Arc::new(AtomicUsize::new(0));
    let existing = ResourceHandle::new(ResourceRole::StateStore, Arc::new(1usize))
        .with_cleanup(Arc::new(CountCleanup(existing_cleanup.clone())));
    let mut graph = ComputationGraph::builder("rejection")
        .service(Box::new(Service::new(
            "existing",
            Arc::new(AtomicUsize::new(0)),
        )))
        .declare_resource(resource_spec("existing-store", ResourceOwnership::Graph))
        .unwrap()
        .provide_resource(
            ResourceId::try_new("existing-store").unwrap(),
            existing.clone(),
        )
        .unwrap()
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let rejected = service("existing")
            .with_resource(
                resource_spec("new", ResourceOwnership::Graph),
                ResourceHandle::new(ResourceRole::StateStore, Arc::new(2usize))
                    .with_cleanup(Arc::new(CountCleanup(incoming_cleanup.clone()))),
            )
            .with_resource(
                resource_spec("same-existing-instance", ResourceOwnership::Graph),
                existing,
            )
            .with_resource(
                resource_spec("borrowed", ResourceOwnership::Borrowed),
                ResourceHandle::new(ResourceRole::StateStore, Arc::new(3usize))
                    .with_cleanup(Arc::new(CountCleanup(borrowed_cleanup.clone()))),
            );
        assert!(matches!(
            control.add_component(rejected).await,
            Err(GraphError::AdditionRejected { .. })
        ));
        assert_eq!(existing_cleanup.load(Ordering::SeqCst), 0);
        assert_eq!(incoming_cleanup.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.dispose().await.unwrap();
    assert_eq!(existing_cleanup.load(Ordering::SeqCst), 1);
    assert_eq!(incoming_cleanup.load(Ordering::SeqCst), 1);
    assert_eq!(borrowed_cleanup.load(Ordering::SeqCst), 0);
    graph.dispose().await.unwrap();
    assert_eq!(incoming_cleanup.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn connecting_deferred_components_does_not_start_them_before_explicit_activation() {
    let (_input, receiver) = mpsc::channel(1);
    let (output, _) = mpsc::unbounded_channel();
    let mut graph = ComputationGraph::empty("deferred").unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let source = control
            .add_component(
                ComponentAddition::new(ConstructedComponent::source(Box::new(InputSource {
                    descriptor: descriptor("source", false, true),
                    input: receiver,
                    control: Arc::new(Mutex::new(None)),
                    produced: Arc::new(AtomicUsize::new(0)),
                    progressed: Arc::new(Notify::new()),
                })))
                .bind_stream(port("out"), stream("source/out"))
                .defer_activation(),
            )
            .await
            .unwrap();
        let sink = control
            .add_component(
                ComponentAddition::new(ConstructedComponent::sink(Box::new(CountingSink {
                    descriptor: descriptor("sink", true, false),
                    output,
                })))
                .defer_activation(),
            )
            .await
            .unwrap();
        control
            .connect(
                EdgeDefinition::new(
                    Endpoint::new(id("source"), port("out")),
                    Endpoint::new(id("sink"), port("in")),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
                RelationshipPolicy::default(),
            )
            .await
            .unwrap();
        assert!(!source.observed().unwrap().started);
        assert!(!sink.observed().unwrap().started);
        assert_eq!(
            source.observed().unwrap().lifecycle,
            ComponentLifecycle::Stopped
        );
        control
            .start_components(control.desired_snapshot().revision, GraphSelection::All)
            .await
            .unwrap();
        source.wait_started().await.unwrap();
        sink.wait_started().await.unwrap();
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn an_unresolved_factory_does_not_block_unrelated_changes() {
    let mut graph = ComputationGraph::empty("missing-factory").unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let broken = ComponentAddition {
            definition: DesiredComponent {
                descriptor: descriptor("broken", false, false),
                role: ComponentRole::Service,
                completion: None,
                streams: BTreeMap::new(),
                lifecycle: LifecyclePolicy::default(),
                input_merge: InputMergePolicy::default(),
                construction: ComponentConstruction::Factory(ComponentSpecification {
                    descriptor: descriptor("broken", false, false),
                    role: ComponentRole::Service,
                    completion: None,
                    implementation: ImplementationIdentity::try_new("missing", "1").unwrap(),
                    configuration_version: 1,
                    configuration: BTreeMap::new(),
                    dependencies: BTreeMap::new(),
                }),
            },
            resources: Vec::new(),
            bindings: TopologyBindings::default(),
        };
        let broken = control.add_component(broken).await.unwrap();
        assert!(broken.wait_created().await.is_err());
        let healthy = control
            .add_component(service("healthy").auto_start(false))
            .await
            .unwrap();
        healthy.wait_created().await.unwrap();
        for component in ["healthy", "broken"] {
            let preview = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::RemoveComponents {
                        selection: GraphSelection::Exact(vec![id(component)]),
                        policy: RemovalPolicy::Reject,
                    }],
                )
                .await
                .unwrap();
            assert!(
                control
                    .reconcile(preview, TopologyBindings::default())
                    .await
                    .unwrap()
                    .committed
            );
        }
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

struct SlowReadySource {
    base: Arc<drasi_lib::sources::SourceBase>,
    fail_initialize: bool,
}

#[async_trait]
impl drasi_lib::Source for SlowReadySource {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn type_name(&self) -> &str {
        "slow-ready"
    }
    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        std::collections::HashMap::new()
    }
    async fn initialize(&self, context: drasi_lib::SourceRuntimeContext) {
        self.base.initialize(context).await;
        if self.fail_initialize {
            self.base
                .set_status(
                    drasi_lib::ComponentStatus::Error,
                    Some("initialization reported failure and returned".into()),
                )
                .await;
        }
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.base
            .set_status(drasi_lib::ComponentStatus::Starting, None)
            .await;
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.base
            .set_status(drasi_lib::ComponentStatus::Stopped, None)
            .await;
        Ok(())
    }
    async fn status(&self) -> drasi_lib::ComponentStatus {
        self.base.get_status().await
    }
    async fn subscribe(
        &self,
        _: drasi_lib::config::SourceSubscriptionSettings,
    ) -> anyhow::Result<drasi_lib::SubscriptionResponse> {
        anyhow::bail!("this lifecycle fixture has no data subscriptions")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[tokio::test]
async fn ordinary_source_handle_waits_for_running_not_only_successful_start_hook() {
    let core = drasi_lib::DrasiLib::builder().build().await.unwrap();
    core.start().await.unwrap();
    let base = Arc::new(
        drasi_lib::sources::SourceBase::new(drasi_lib::sources::SourceBaseParams::new(
            "slow-source",
        ))
        .unwrap(),
    );
    let handle = core
        .add_source_with_handle(SlowReadySource {
            base: base.clone(),
            fail_initialize: false,
        })
        .await
        .unwrap();
    handle.wait_created().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), handle.wait_started())
            .await
            .is_err()
    );
    assert_eq!(
        base.get_status().await,
        drasi_lib::ComponentStatus::Starting
    );
    base.set_status(drasi_lib::ComponentStatus::Running, None)
        .await;
    tokio::time::timeout(Duration::from_secs(2), handle.wait_started())
        .await
        .unwrap()
        .unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_component_subscriptions_are_pipe_nodes_and_control_neighbors() {
    let core = drasi_lib::DrasiLib::builder().build().await.unwrap();
    let base = Arc::new(
        drasi_lib::sources::SourceBase::new(drasi_lib::sources::SourceBaseParams::new("source"))
            .unwrap(),
    );
    core.add_source_with_handle(SlowReadySource {
        base,
        fail_initialize: false,
    })
    .await
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    core.add_query_with_handle(
        drasi_lib::Query::cypher("query")
            .query("MATCH (n) RETURN n")
            .from_source("source")
            .build(),
    )
    .await
    .unwrap()
    .wait_created()
    .await
    .unwrap();
    let (reaction, _results) = drasi_reaction_application::ApplicationReaction::new(
        "reaction".to_owned(),
        vec!["query".to_owned()],
    );
    core.add_reaction_with_handle(reaction)
        .await
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    let control = core.computation_control().unwrap();
    let mut changes = control.subscribe_observed();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            changes.borrow_and_update();
            if control.desired_snapshot().subscriptions.len() == 2 {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let topology = control.inspector().topology();
    assert_eq!(
        topology
            .nodes
            .values()
            .filter(|node| matches!(node, GraphEntity::SubscriptionPipe(_)))
            .count(),
        2
    );
    let source = core
        .computation_component("source")
        .unwrap()
        .control()
        .unwrap();
    source
        .notify_neighbor(&id("query"), ControlNotification::Available)
        .unwrap();
    assert!(source
        .notify_neighbor(&id("reaction"), ControlNotification::Available)
        .is_err());
    assert!(control.desired_snapshot().control_connections.is_empty());
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_void_initializer_error_is_not_lost_when_the_initializer_returns() {
    let core = drasi_lib::DrasiLib::builder().build().await.unwrap();
    core.start().await.unwrap();
    let base = Arc::new(
        drasi_lib::sources::SourceBase::new(drasi_lib::sources::SourceBaseParams::new(
            "failed-source",
        ))
        .unwrap(),
    );
    let handle = core
        .add_source_with_handle(SlowReadySource {
            base,
            fail_initialize: true,
        })
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), handle.wait_started())
        .await
        .unwrap();
    assert!(result.is_err());
    assert_eq!(
        handle.observed().unwrap().realization,
        RealizationState::CreationFailed
    );
    assert!(!handle.observed().unwrap().started);
    core.shutdown().await.unwrap();
}
