// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(feature = "computation")]

#[allow(dead_code)]
mod computation_support;

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    result::Result,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};

#[derive(Default)]
struct Calls {
    starts: AtomicUsize,
    stops: AtomicUsize,
    updates: AtomicUsize,
    creates: AtomicUsize,
    fail_stop: AtomicBool,
    fail_create: AtomicBool,
    terminal: AtomicBool,
}

#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
}

struct Source {
    descriptor: ComponentDescriptor,
    calls: Arc<Calls>,
    events: mpsc::Receiver<OutputEnvelope>,
}

#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.calls.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.events.recv().await)
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    calls: Arc<Calls>,
    received: mpsc::Sender<(String, u64)>,
    label: String,
    gate: Option<Arc<Gate>>,
    accepted: bool,
}
#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.calls.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.stops.fetch_add(1, Ordering::SeqCst);
        if self.calls.fail_stop.swap(false, Ordering::SeqCst) {
            anyhow::bail!("cleanup has not completed");
        }
        Ok(())
    }
    async fn reconfigure(&mut self, context: ConstructionContext) -> anyhow::Result<()> {
        self.label = context.configuration()["label"]
            .as_str()
            .expect("validated")
            .into();
        self.calls.updates.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        if self.accepted {
            SinkCompletion::Accepted
        } else {
            SinkCompletion::Handled
        }
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        if let Some(gate) = self.gate.take() {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        self.received
            .send((self.label.clone(), input.envelope.system().sequence()))
            .await?;
        Ok(())
    }
}

fn source(calls: Arc<Calls>) -> (Box<dyn EnvelopeSource>, mpsc::Sender<OutputEnvelope>) {
    let (sender, events) = mpsc::channel(16);
    (
        Box::new(Source {
            descriptor: descriptor("source", &[], &["out"]),
            calls,
            events,
        }),
        sender,
    )
}
fn sink(id: &str, label: &str, calls: Arc<Calls>, received: mpsc::Sender<(String, u64)>) -> Sink {
    Sink {
        descriptor: descriptor(id, &["in"], &[]),
        calls,
        received,
        label: label.into(),
        gate: None,
        accepted: false,
    }
}
fn definition(id: &str) -> DesiredComponent {
    DesiredComponent {
        descriptor: descriptor(id, &["in"], &[]),
        role: ComponentRole::Sink,
        completion: Some(SinkCompletion::Handled),
        streams: BTreeMap::new(),
        lifecycle: LifecyclePolicy::default(),
        input_merge: InputMergePolicy::Arrival,
        construction: ComponentConstruction::External { binding: id.into() },
    }
}
fn relationship(to: &str) -> DesiredRelationship {
    DesiredRelationship {
        definition: edge("source", to),
        policy: RelationshipPolicy::default(),
        pipe: DesiredPipe::Bounded { capacity: 1 },
    }
}
fn bind_sink(sink: Sink) -> TopologyBindings {
    let mut bindings = TopologyBindings::default();
    bindings.components.insert(
        sink.descriptor.id().to_string(),
        ConstructedComponent::sink(Box::new(sink)),
    );
    bindings
}
async fn activate(control: &GraphControl) {
    control.deployment_report().await.expect("deployment");
    let report = control
        .start_components(GraphRevision(1), GraphSelection::All)
        .await
        .expect("startup");
    assert_eq!(report.summary, OperationSummary::Completed);
}
fn cancelled(result: GraphResult<()>) {
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

macro_rules! both_runtimes {
    ($name:ident, $scenario:ident) => {
        mod $name {
            #[tokio::test(flavor = "current_thread")]
            async fn current_thread() {
                tokio::time::timeout(super::Duration::from_secs(15), super::$scenario())
                    .await
                    .expect("reconciliation deadlocked");
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn multi_thread() {
                tokio::time::timeout(super::Duration::from_secs(15), super::$scenario())
                    .await
                    .expect("reconciliation deadlocked");
            }
        }
    };
}

async fn replace_sink_at_safe_boundary() {
    let upstream = Arc::new(Calls::default());
    let old = Arc::new(Calls::default());
    let new = Arc::new(Calls::default());
    let (source, input) = source(upstream.clone());
    let (received, mut output_rx) = mpsc::channel(16);
    let gate = Arc::new(Gate::default());
    let mut original = sink("sink", "old", old.clone(), received.clone());
    original.gate = Some(gate.clone());
    let mut graph = ComputationGraph::builder("replace")
        .source(source)
        .sink(Box::new(original))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        input
            .send(output(root("source", 1, &[1])))
            .await
            .expect("input");
        gate.entered.notified().await;
        let old_observed = control.observed().components[&component("sink")].clone();
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::ReplaceComponent(definition("sink"))],
            )
            .await
            .expect("preview");
        assert_eq!(preview.replaced(), &BTreeSet::from([component("sink")]));
        assert!(preview.restarted().is_empty());
        assert!(!preview.paused().contains(&component("source")));
        let stale = preview.clone();
        let replacement = control.reconcile(
            preview,
            bind_sink(sink("sink", "new", new.clone(), received)),
        );
        tokio::pin!(replacement);
        tokio::select! {
            result = &mut replacement => panic!("replacement interrupted admitted handling: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(old.stops.load(Ordering::SeqCst), 0);
        let (report, ()) = tokio::join!(replacement, async {
            let mut observed = control.subscribe_observed();
            observed
                .wait_for(|state| {
                    state.components[&component("sink")].lifecycle == ComponentLifecycle::Quiescing
                })
                .await
                .expect("controller requested boundary");
            input
                .send(output(root("source", 2, &[2])))
                .await
                .expect("queued input");
            gate.release.notify_one();
        });
        let report = report.expect("replace");
        assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
        assert_eq!(output_rx.recv().await, Some(("old".into(), 1)));
        assert_eq!(output_rx.recv().await, Some(("new".into(), 2)));
        assert_eq!(upstream.starts.load(Ordering::SeqCst), 1);
        assert_eq!(upstream.stops.load(Ordering::SeqCst), 0);
        assert_eq!(old.stops.load(Ordering::SeqCst), 1);
        assert_eq!(new.starts.load(Ordering::SeqCst), 1);
        assert!(matches!(
            control.reconcile(stale, TopologyBindings::default()).await,
            Err(GraphError::StaleRevision { .. })
        ));
        assert!(matches!(
            control
                .report_health(
                    report.revision,
                    HealthObservation {
                        component: component("sink"),
                        generation: old_observed.generation,
                        operation: old_observed.operation,
                        health: ComponentHealth::Healthy,
                    }
                )
                .await,
            Err(GraphError::StaleGeneration)
        ));
        control.cancel();
    });
    cancelled(result);
}
both_runtimes!(sink_replacement, replace_sink_at_safe_boundary);

async fn add_remove_preserves_stable_indices() {
    let upstream = Arc::new(Calls::default());
    let original = Arc::new(Calls::default());
    let (source, input) = source(upstream.clone());
    let (received, mut output_rx) = mpsc::channel(16);
    let mut graph = ComputationGraph::builder("live-topology")
        .source(source)
        .sink(Box::new(sink(
            "original",
            "original",
            original.clone(),
            received.clone(),
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "original"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let mut revision = GraphRevision(1);
        for (sequence, id) in [(1, "second"), (2, "third")] {
            let added = Arc::new(Calls::default());
            let preview = control
                .preview(
                    revision,
                    vec![
                        DesiredMutation::PutComponent(definition(id)),
                        DesiredMutation::Bind(relationship(id)),
                    ],
                )
                .await
                .expect("preview add");
            let report = control
                .reconcile(
                    preview,
                    bind_sink(sink(id, id, added.clone(), received.clone())),
                )
                .await
                .expect("add");
            assert_eq!(report.summary, OperationSummary::Completed);
            revision = report.revision;
            input
                .send(output(root(
                    "source",
                    sequence,
                    &[u16::try_from(sequence).expect("small sequence")],
                )))
                .await
                .expect("fanout input");
            let mut labels = BTreeSet::new();
            labels.insert(output_rx.recv().await.expect("first branch"));
            labels.insert(output_rx.recv().await.expect("second branch"));
            assert_eq!(
                labels,
                BTreeSet::from([("original".into(), sequence), (id.into(), sequence)])
            );
            let preview = control
                .preview(
                    revision,
                    vec![DesiredMutation::RemoveComponents {
                        selection: GraphSelection::Exact(vec![component(id)]),
                        policy: RemovalPolicy::Cascade,
                    }],
                )
                .await
                .expect("preview remove sink only");
            let report = control
                .reconcile(preview, TopologyBindings::default())
                .await
                .expect("remove sink");
            assert_eq!(report.summary, OperationSummary::Completed);
            revision = report.revision;
            assert_eq!(control.desired_snapshot().nodes.len(), 2);
            assert_eq!(control.observed().components.len(), 2);
            assert_eq!(added.stops.load(Ordering::SeqCst), 1);
        }
        assert_eq!(upstream.starts.load(Ordering::SeqCst), 1);
        assert_eq!(upstream.stops.load(Ordering::SeqCst), 0);
        assert_eq!(original.starts.load(Ordering::SeqCst), 1);
        assert_eq!(original.stops.load(Ordering::SeqCst), 0);
        input
            .send(output(root("source", 3, &[3])))
            .await
            .expect("original remains live");
        assert_eq!(output_rx.recv().await, Some(("original".into(), 3)));
        control.cancel();
    });
    cancelled(result);
}
both_runtimes!(live_add_remove, add_remove_preserves_stable_indices);

struct RecordingProvider(Arc<Mutex<Vec<Arc<dyn EnvelopeSender>>>>);
impl PipeProvider for RecordingProvider {
    fn capabilities(&self) -> Result<PipeCapabilities, PipeError> {
        BoundedPipeConfig { capacity: 2 }.capabilities()
    }
    fn create(&self) -> Result<ProvidedPipe, PipeError> {
        let pipe = BoundedPipeConfig { capacity: 2 }.create()?;
        self.0.lock().expect("senders").push(pipe.pipe.sender());
        Ok(pipe)
    }
}

#[tokio::test]
async fn rebind_revokes_old_sender_after_handling_admitted_work_without_restarting_endpoints() {
    let upstream = Arc::new(Calls::default());
    let downstream = Arc::new(Calls::default());
    let (source, input) = source(upstream.clone());
    let (received, mut output_rx) = mpsc::channel(8);
    let senders = Arc::new(Mutex::new(Vec::new()));
    let mut graph = ComputationGraph::builder("rebind")
        .source(source)
        .sink(Box::new(sink("sink", "sink", downstream.clone(), received)))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(RecordingProvider(senders.clone())),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        input
            .send(output(root("source", 1, &[1])))
            .await
            .expect("input");
        assert_eq!(output_rx.recv().await, Some(("sink".into(), 1)));
        let old = senders.lock().expect("senders")[0].clone();
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::Bind(relationship("sink"))],
            )
            .await
            .expect("preview");
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("rebind");
        assert_eq!(report.summary, OperationSummary::Completed);
        assert!(matches!(
            old.send(root("source", 99, &[99]))
                .await
                .expect_err("stale sender")
                .error,
            PipeError::Closed
        ));
        input
            .send(output(root("source", 2, &[2])))
            .await
            .expect("new binding");
        assert_eq!(output_rx.recv().await, Some(("sink".into(), 2)));
        assert_eq!(upstream.starts.load(Ordering::SeqCst), 1);
        assert_eq!(downstream.starts.load(Ordering::SeqCst), 1);
        assert_eq!(upstream.stops.load(Ordering::SeqCst), 0);
        assert_eq!(downstream.stops.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    cancelled(result);
}

#[tokio::test]
async fn permissible_orphan_is_visible_exportable_and_can_be_rebound() {
    let upstream = Arc::new(Calls::default());
    let (source, input) = source(upstream.clone());
    let (received, mut output_rx) = mpsc::channel(8);
    let policy = RelationshipPolicy {
        required_for_binding: false,
        orphan_permitted: true,
        ..RelationshipPolicy::default()
    };
    let mut graph = ComputationGraph::builder("orphan")
        .source(source)
        .sink(Box::new(sink(
            "sink",
            "sink",
            Arc::new(Calls::default()),
            received,
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .relationship_policy(edge("source", "sink"), policy.clone())
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::Unbind {
                    edge: edge("source", "sink"),
                    policy: RemovalPolicy::Orphan,
                }],
            )
            .await
            .expect("orphan preview");
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("orphan");
        assert_eq!(report.summary, OperationSummary::Completed);
        assert_eq!(
            control.observed().relationships[&edge("source", "sink")].binding,
            BindingState::Declared
        );
        assert_eq!(
            control.observed().components[&component("source")].lifecycle,
            ComponentLifecycle::Quiesced
        );
        let desired = control
            .desired_snapshot()
            .select(GraphSelection::All)
            .expect("export");
        assert!(desired.relationships.is_empty());
        assert_eq!(
            DesiredTopology::from_json(&desired.to_json().expect("json"))
                .expect("import description")
                .boundary_relationships
                .len(),
            1
        );
        let preview = control
            .preview(
                report.revision,
                vec![DesiredMutation::Bind(DesiredRelationship {
                    policy,
                    ..relationship("sink")
                })],
            )
            .await
            .expect("rebind preview");
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("rebind");
        assert_eq!(report.summary, OperationSummary::Completed);
        input
            .send(output(root("source", 1, &[1])))
            .await
            .expect("input");
        assert_eq!(output_rx.recv().await, Some(("sink".into(), 1)));
        assert_eq!(upstream.starts.load(Ordering::SeqCst), 1);
        control.cancel();
    });
    cancelled(result);
}

#[tokio::test]
async fn drain_waits_for_handling_and_failed_cleanup_retains_desired_ownership_for_retry() {
    let upstream = Arc::new(Calls::default());
    let downstream = Arc::new(Calls::default());
    downstream.fail_stop.store(true, Ordering::SeqCst);
    let (source, input) = source(upstream.clone());
    let (received, mut output_rx) = mpsc::channel(8);
    let gate = Arc::new(Gate::default());
    let mut consumer = sink("sink", "sink", downstream.clone(), received);
    consumer.gate = Some(gate.clone());
    let mut graph = ComputationGraph::builder("drain")
        .source(source)
        .sink(Box::new(consumer))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        assert!(control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::RemoveComponents {
                    selection: GraphSelection::Exact(vec![component("source")]),
                    policy: RemovalPolicy::Reject,
                }]
            )
            .await
            .is_err());
        input
            .send(output(root("source", 1, &[1])))
            .await
            .expect("input");
        gate.entered.notified().await;
        let removal = DesiredMutation::RemoveComponents {
            selection: GraphSelection::All,
            policy: RemovalPolicy::Drain,
        };
        let preview = control
            .preview(GraphRevision(1), vec![removal.clone()])
            .await
            .expect("drain preview");
        let drain = control.reconcile(preview, TopologyBindings::default());
        tokio::pin!(drain);
        tokio::select! {
            result = &mut drain => panic!("drain claimed unfinished handling: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        assert_eq!(downstream.stops.load(Ordering::SeqCst), 0);
        gate.release.notify_one();
        let failed = drain.await.expect("cleanup report");
        assert_eq!(output_rx.recv().await, Some(("sink".into(), 1)));
        assert_eq!(failed.summary, OperationSummary::CompletedWithFailures);
        assert!(!failed.committed);
        assert_eq!(control.desired_snapshot().nodes.len(), 2);
        assert_eq!(
            control.observed().components[&component("sink")]
                .failure
                .as_ref()
                .expect("failure")
                .phase,
            FailurePhase::Stop
        );
        let preview = control
            .preview(failed.revision, vec![removal])
            .await
            .expect("explicit retry");
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("cleanup retry");
        assert_eq!(report.summary, OperationSummary::Completed);
        assert!(report.committed);
        assert!(control.desired_snapshot().nodes.is_empty());
        assert!(control.observed().components.is_empty());
        assert_eq!(upstream.stops.load(Ordering::SeqCst), 1);
        assert_eq!(downstream.stops.load(Ordering::SeqCst), 2);
        control.cancel();
    });
    cancelled(result);
}

#[tokio::test]
async fn acceptance_only_sink_rejects_handled_drain_before_any_lifecycle_change() {
    let calls = Arc::new(Calls::default());
    let (source, _input) = source(Arc::new(Calls::default()));
    let (received, _output) = mpsc::channel(8);
    let mut consumer = sink("sink", "sink", calls.clone(), received);
    consumer.accepted = true;
    let mut graph = ComputationGraph::builder("accepted")
        .source(source)
        .sink(Box::new(consumer))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let error = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::RemoveComponents {
                    selection: GraphSelection::All,
                    policy: RemovalPolicy::Drain,
                }],
            )
            .await
            .expect_err("Accepted is not Handled");
        assert!(error.to_string().contains("acceptance-only"));
        assert_eq!(calls.stops.load(Ordering::SeqCst), 0);
        assert_eq!(control.desired_snapshot().revision, GraphRevision(1));
        control.cancel();
    });
    cancelled(result);
}

struct Factory {
    descriptor: FactoryDescriptor,
    calls: Arc<Calls>,
    received: mpsc::Sender<(String, u64)>,
}
impl Factory {
    fn new(calls: Arc<Calls>, received: mpsc::Sender<(String, u64)>) -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test-sink", "1")
                    .expect("implementation"),
                role: ComponentRole::Sink,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        Arc::from("label"),
                        ConfigurationField {
                            value_type: ConfigurationType::String,
                            required: true,
                            secret: false,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::new(),
            },
            calls,
            received,
        }
    }
    fn specification(&self, label: &str) -> ComponentSpecification {
        ComponentSpecification {
            descriptor: descriptor("sink", &["in"], &[]),
            role: ComponentRole::Sink,
            completion: Some(SinkCompletion::Handled),
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([(
                Arc::from("label"),
                ConfigurationValue::Literal(label.into()),
            )]),
            dependencies: BTreeMap::new(),
        }
    }
}
#[async_trait]
impl ComponentFactory for Factory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn supports_reconfiguration(&self) -> bool {
        true
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        self.calls.creates.fetch_add(1, Ordering::SeqCst);
        if self.calls.fail_create.swap(false, Ordering::SeqCst) {
            return Err(if self.calls.terminal.load(Ordering::SeqCst) {
                ComponentCreationError::terminal(anyhow::anyhow!("invalid runtime configuration"))
            } else {
                ComponentCreationError::retryable(anyhow::anyhow!(
                    "temporarily unavailable resource"
                ))
            });
        }
        Ok(ConstructedComponent::sink(Box::new(sink(
            "sink",
            context.configuration()["label"].as_str().expect("label"),
            self.calls.clone(),
            self.received.clone(),
        ))))
    }
}

#[tokio::test]
async fn retryable_creation_is_reconstructed_but_terminal_failure_requires_spec_change() {
    for terminal in [false, true] {
        let calls = Arc::new(Calls::default());
        calls.fail_create.store(true, Ordering::SeqCst);
        calls.terminal.store(terminal, Ordering::SeqCst);
        let (received, mut output_rx) = mpsc::channel(8);
        let factory = Arc::new(Factory::new(calls.clone(), received));
        let (source, input) = source(Arc::new(Calls::default()));
        let mut graph = ComputationGraph::builder("creation-retry")
            .source(source)
            .component(factory.specification("first"), factory.clone())
            .bind_stream(endpoint("source", "out"), stream("source"))
            .connect(
                edge("source", "sink"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()
            .expect("graph");
        let run = graph.run().expect("scope");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            let deployment = control.deployment_report().await.expect("deploy");
            assert!(matches!(
                deployment.components[&component("sink")],
                CreationOutcome::CreationFailed(_)
            ));
            let old_generation = control.observed().components[&component("sink")].generation;
            let retry = control
                .preview(
                    GraphRevision(1),
                    vec![DesiredMutation::Retry(GraphSelection::Exact(vec![component("sink")]))],
                )
                .await;
            let preview = if terminal {
                assert!(retry.is_err());
                let mut desired = definition("sink");
                desired.construction =
                    ComponentConstruction::Factory(factory.specification("fixed"));
                control
                    .preview(
                        GraphRevision(1),
                        vec![DesiredMutation::PutComponent(desired)],
                    )
                    .await
                    .expect("changed config")
            } else {
                retry.expect("retryable failure")
            };
            let report = control
                .reconcile(preview, TopologyBindings::default())
                .await
                .expect("realize");
            assert_eq!(report.summary, OperationSummary::Completed);
            assert!(control.observed().components[&component("sink")].generation > old_generation);
            input
                .send(output(root("source", 1, &[1])))
                .await
                .expect("input");
            assert_eq!(output_rx.recv().await.expect("handled").1, 1);
            assert_eq!(calls.creates.load(Ordering::SeqCst), 2);
            control.cancel();
        });
        cancelled(result);
    }
}

#[tokio::test]
async fn supported_in_place_update_changes_behavior_without_reconstruction_or_upstream_restart() {
    let calls = Arc::new(Calls::default());
    let upstream = Arc::new(Calls::default());
    let (received, mut output_rx) = mpsc::channel(8);
    let factory = Arc::new(Factory::new(calls.clone(), received));
    let (source, input) = source(upstream.clone());
    let mut graph = ComputationGraph::builder("in-place")
        .source(source)
        .component(factory.specification("before"), factory.clone())
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let generation = control.observed().components[&component("sink")].generation;
        let mut desired = definition("sink");
        desired.construction = ComponentConstruction::Factory(factory.specification("after"));
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::UpdateComponent(desired)],
            )
            .await
            .expect("preview update");
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("update");
        assert_eq!(report.summary, OperationSummary::Completed);
        assert_eq!(
            control.observed().components[&component("sink")].generation,
            generation
        );
        input
            .send(output(root("source", 1, &[1])))
            .await
            .expect("input");
        assert_eq!(output_rx.recv().await, Some(("after".into(), 1)));
        assert_eq!(calls.creates.load(Ordering::SeqCst), 1);
        assert_eq!(calls.starts.load(Ordering::SeqCst), 1);
        assert_eq!(calls.stops.load(Ordering::SeqCst), 0);
        assert_eq!(calls.updates.load(Ordering::SeqCst), 1);
        assert_eq!(upstream.starts.load(Ordering::SeqCst), 1);
        control.cancel();
    });
    cancelled(result);
}

fn retained_config(id: &str) -> RetainedPipeConfig {
    RetainedPipeConfig {
        resource: ResourceId::try_new(id).expect("resource"),
        capacity: std::num::NonZeroUsize::new(4).expect("capacity"),
        durable: false,
        retention: RetentionPolicy::Backpressure,
        gap_policy: ReplayGapPolicy::Strict,
    }
}

async fn retained_backlog_boundaries() {
    for remove in [false, true] {
        let store = Arc::new(MemoryEnvelopeStore::new(
            retained_config("store").capacity,
            RetentionPolicy::Backpressure,
        ));
        let old = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("old binding");
        let count = if remove { 2 } else { 1 };
        for sequence in 1..=count {
            old.sender()
                .send(root("source", sequence, &[1]))
                .await
                .expect("retained admission");
        }
        drop(old);
        let (source, _input) = source(Arc::new(Calls::default()));
        let (received, mut output_rx) = mpsc::channel(8);
        let gate = Arc::new(Gate::default());
        let mut consumer = sink("sink", "sink", Arc::new(Calls::default()), received);
        if remove {
            consumer.gate = Some(gate.clone());
        }
        let mut graph = ComputationGraph::builder("retained-boundary")
            .source(source)
            .sink(Box::new(consumer))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .connect(edge("source", "sink"), Box::new(retained_config("store")))
            .declare_resource(ResourceSpecification {
                id: ResourceId::try_new("store").expect("resource"),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: Arc::from("store"),
            })
            .expect("declare")
            .provide_resource(
                ResourceId::try_new("store").expect("resource"),
                ResourceHandle::new(
                    ResourceRole::StateStore,
                    Arc::new(RetainedStoreResource(store.clone())),
                ),
            )
            .expect("provide")
            .build()
            .expect("graph");
        let run = graph.run().expect("scope");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            activate(&control).await;
            if remove {
                gate.entered.notified().await;
            } else {
                assert_eq!(output_rx.recv().await, Some(("sink".into(), 1)));
            }
            let change = if remove {
                DesiredMutation::RemoveComponents {
                    selection: GraphSelection::All,
                    policy: RemovalPolicy::Drain,
                }
            } else {
                DesiredMutation::Bind(DesiredRelationship {
                    pipe: DesiredPipe::Retained(retained_config("store")),
                    ..relationship("sink")
                })
            };
            let preview = control
                .preview(GraphRevision(1), vec![change])
                .await
                .expect("preview");
            let operation = control.reconcile(preview, TopologyBindings::default());
            let (report, ()) = tokio::join!(operation, async {
                if remove {
                    let mut observed = control.subscribe_observed();
                    observed
                        .wait_for(|state| {
                            state.components[&component("source")].lifecycle
                                == ComponentLifecycle::Quiesced
                        })
                        .await
                        .expect("admission stopped");
                    gate.release.notify_one();
                }
            });
            assert_eq!(
                report.expect("reconcile").summary,
                OperationSummary::Completed
            );
            if remove {
                assert_eq!(
                    output_rx.try_recv().expect("first replay handled"),
                    ("sink".into(), 1)
                );
                assert_eq!(
                    output_rx
                        .try_recv()
                        .expect("second replay handled before removal"),
                    ("sink".into(), 2)
                );
            } else {
                assert_eq!(
                    store
                        .progress(store.generation())
                        .await
                        .expect("retained progress"),
                    1
                );
            }
            control.cancel();
        });
        cancelled(result);
    }
}
both_runtimes!(retained_reconciliation, retained_backlog_boundaries);

struct GatedTransform {
    descriptor: ComponentDescriptor,
    gate: Option<Arc<Gate>>,
    sequence: u64,
}
#[async_trait]
impl ComputationComponent for GatedTransform {
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
impl Transformer for GatedTransform {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        if let Some(gate) = self.gate.take() {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        self.sequence += 1;
        Ok(vec![output(derived(
            &input.envelope,
            "transform",
            self.sequence,
            input.envelope.changes().clone(),
        ))])
    }
}

async fn drain_multi_hop_backlog() {
    let (source, input) = source(Arc::new(Calls::default()));
    let (received, mut output_rx) = mpsc::channel(8);
    let gate = Arc::new(Gate::default());
    let mut graph = ComputationGraph::builder("multi-hop-drain")
        .source(source)
        .transformer(Box::new(GatedTransform {
            descriptor: descriptor("transform", &["in"], &["out"]),
            gate: Some(gate.clone()),
            sequence: 0,
        }))
        .sink(Box::new(sink(
            "sink",
            "sink",
            Arc::new(Calls::default()),
            received,
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("transform", "out"), stream("transform"))
        .connect(
            edge("source", "transform"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .connect(
            edge("transform", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        input
            .send(output(root("source", 1, &[1])))
            .await
            .expect("first");
        input
            .send(output(root("source", 2, &[2])))
            .await
            .expect("second");
        gate.entered.notified().await;
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::RemoveComponents {
                    selection: GraphSelection::All,
                    policy: RemovalPolicy::Drain,
                }],
            )
            .await
            .expect("preview");
        let (report, ()) = tokio::join!(
            control.reconcile(preview, TopologyBindings::default()),
            async {
                let mut observed = control.subscribe_observed();
                observed
                    .wait_for(|state| {
                        state.components[&component("source")].lifecycle
                            == ComponentLifecycle::Quiesced
                    })
                    .await
                    .expect("producer parked before transformer");
                gate.release.notify_one();
            }
        );
        assert_eq!(report.expect("drain").summary, OperationSummary::Completed);
        assert_eq!(
            output_rx.try_recv().expect("handled first"),
            ("sink".into(), 1)
        );
        assert_eq!(
            output_rx.try_recv().expect("handled second"),
            ("sink".into(), 2)
        );
        control.cancel();
    });
    cancelled(result);
}
both_runtimes!(multi_hop_drain, drain_multi_hop_backlog);

struct StoreCleanup {
    store: Arc<MemoryEnvelopeStore>,
    calls: Arc<AtomicUsize>,
    fail: AtomicBool,
}
#[async_trait]
impl ResourceCleanup for StoreCleanup {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.swap(false, Ordering::SeqCst) {
            anyhow::bail!("second resource cleanup failed");
        }
        self.store.shutdown().await
    }
}
fn store_handle(fail: bool, calls: Arc<AtomicUsize>) -> ResourceHandle {
    let store = Arc::new(MemoryEnvelopeStore::new(
        retained_config("store").capacity,
        RetentionPolicy::Backpressure,
    ));
    ResourceHandle::new(
        ResourceRole::StateStore,
        Arc::new(RetainedStoreResource(store.clone())),
    )
    .with_cleanup(Arc::new(StoreCleanup {
        store,
        calls,
        fail: AtomicBool::new(fail),
    }))
}

#[tokio::test]
async fn partial_resource_cleanup_keeps_revoked_bindings_quiesced_until_explicit_replacement() {
    let source_calls = Arc::new(Calls::default());
    let (source, input) = source(source_calls.clone());
    let (received, mut output_rx) = mpsc::channel(8);
    let clean_a = Arc::new(AtomicUsize::new(0));
    let clean_b = Arc::new(AtomicUsize::new(0));
    let mut builder = ComputationGraph::builder("resource-replacement")
        .source(source)
        .sink(Box::new(sink(
            "a",
            "a",
            Arc::new(Calls::default()),
            received.clone(),
        )))
        .sink(Box::new(sink(
            "b",
            "b",
            Arc::new(Calls::default()),
            received,
        )))
        .bind_stream(endpoint("source", "out"), stream("source"));
    for (id, fail, calls) in [("a", false, clean_a.clone()), ("b", true, clean_b.clone())] {
        builder = builder
            .connect(edge("source", id), Box::new(retained_config(id)))
            .declare_resource(ResourceSpecification {
                id: ResourceId::try_new(id).expect("id"),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Graph,
                binding: Arc::from(id),
            })
            .expect("declare")
            .provide_resource(
                ResourceId::try_new(id).expect("id"),
                store_handle(fail, calls),
            )
            .expect("provide");
    }
    let mut graph = builder.build().expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        for attempt in 0..2 {
            let changes = ["a", "b"]
                .into_iter()
                .map(|id| DesiredMutation::RebindResource(ResourceId::try_new(id).expect("id")))
                .collect();
            let preview = control
                .preview(GraphRevision(1), changes)
                .await
                .expect("preview");
            let mut bindings = TopologyBindings::default();
            for id in ["a", "b"] {
                bindings.resources.insert(
                    ResourceId::try_new(id).expect("id"),
                    store_handle(false, Arc::new(AtomicUsize::new(0))),
                );
            }
            let report = control
                .reconcile(preview, bindings)
                .await
                .expect("resource report");
            if attempt == 0 {
                assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
                assert!(!report.committed);
                assert_eq!(
                    control.observed().components[&component("source")].lifecycle,
                    ComponentLifecycle::Quiesced
                );
                for id in ["a", "b"] {
                    assert_eq!(
                        control.observed().relationships[&edge("source", id)].binding,
                        BindingState::Failed
                    );
                }
                input
                    .send(output(root("source", 1, &[1])))
                    .await
                    .expect("input remains parked");
            } else {
                assert_eq!(report.summary, OperationSummary::Completed);
                let mut received = BTreeSet::new();
                received.insert(output_rx.recv().await.expect("recovered a"));
                received.insert(output_rx.recv().await.expect("recovered b"));
                assert_eq!(received, BTreeSet::from([("a".into(), 1), ("b".into(), 1)]));
            }
        }
        assert_eq!(clean_a.load(Ordering::SeqCst), 1);
        assert_eq!(clean_b.load(Ordering::SeqCst), 2);
        assert_eq!(source_calls.starts.load(Ordering::SeqCst), 1);
        assert_eq!(source_calls.stops.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    cancelled(result);
    graph.dispose().await.expect("release remaining resources");
}

struct FiniteSource {
    descriptor: ComponentDescriptor,
    calls: Arc<Calls>,
    sequence: u64,
    emit: bool,
}
#[async_trait]
impl ComputationComponent for FiniteSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.calls.starts.fetch_add(1, Ordering::SeqCst);
        self.emit = true;
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for FiniteSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        if !std::mem::take(&mut self.emit) {
            return Ok(None);
        }
        self.sequence += 1;
        Ok(Some(output(root("source", self.sequence, &[1]))))
    }
}

#[tokio::test]
async fn exhausted_source_restart_and_replacement_resume_its_consumer_and_closed_pipes() {
    let source_calls = Arc::new(Calls::default());
    let sink_calls = Arc::new(Calls::default());
    let (received, mut output_rx) = mpsc::channel(8);
    let mut graph = ComputationGraph::builder("restart-exhausted")
        .source(Box::new(FiniteSource {
            descriptor: descriptor("source", &[], &["out"]),
            calls: source_calls.clone(),
            sequence: 0,
            emit: false,
        }))
        .sink(Box::new(sink("sink", "sink", sink_calls.clone(), received)))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let mut revision = GraphRevision(1);
        for sequence in 1..=3 {
            assert_eq!(output_rx.recv().await, Some(("sink".into(), sequence)));
            let mut observed = control.subscribe_observed();
            observed
                .wait_for(|state| state.components.values().all(|node| node.exhausted))
                .await
                .expect("finite exhaustion");
            if sequence < 3 {
                let selection = if sequence == 1 {
                    GraphSelection::Exact(vec![component("source")])
                } else {
                    GraphSelection::All
                };
                let preview = control
                    .preview(revision, vec![DesiredMutation::Restart(selection)])
                    .await
                    .expect("preview restart");
                let report = control
                    .reconcile(preview, TopologyBindings::default())
                    .await
                    .expect("restart");
                assert_eq!(report.summary, OperationSummary::Completed);
                revision = report.revision;
            }
        }
        assert_eq!(source_calls.starts.load(Ordering::SeqCst), 3);
        assert_eq!(
            sink_calls.starts.load(Ordering::SeqCst),
            2,
            "implicit consumer resumption does not call start again"
        );
        let (new_source, new_input) = source(Arc::new(Calls::default()));
        let desired = control
            .desired_snapshot()
            .select(GraphSelection::Exact(vec![component("source")]))
            .expect("source spec")
            .components
            .remove(0);
        let preview = control
            .preview(revision, vec![DesiredMutation::ReplaceComponent(desired)])
            .await
            .expect("replacement");
        let mut bindings = TopologyBindings::default();
        bindings
            .components
            .insert("source".into(), ConstructedComponent::source(new_source));
        let report = control
            .reconcile(preview, bindings)
            .await
            .expect("replace exhausted source");
        assert_eq!(report.summary, OperationSummary::Completed);
        new_input
            .send(output(root("source", 4, &[4])))
            .await
            .expect("new producer");
        assert_eq!(output_rx.recv().await, Some(("sink".into(), 4)));
        assert_eq!(sink_calls.starts.load(Ordering::SeqCst), 2);
        control.cancel();
    });
    cancelled(result);
}

#[tokio::test]
async fn replacing_a_producer_with_a_new_stream_starts_an_independent_sequence() {
    let (original, input) = source(Arc::new(Calls::default()));
    let (received, mut output_rx) = mpsc::channel(8);
    let mut graph = ComputationGraph::builder("new-stream")
        .source(original)
        .sink(Box::new(sink(
            "sink",
            "sink",
            Arc::new(Calls::default()),
            received,
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        input
            .send(output(root("source", 10, &[1])))
            .await
            .expect("old stream");
        assert_eq!(output_rx.recv().await, Some(("sink".into(), 10)));
        let mut desired = control
            .desired_snapshot()
            .select(GraphSelection::Exact(vec![component("source")]))
            .expect("source")
            .components
            .remove(0);
        desired.streams.insert(port("out"), stream("fresh"));
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::ReplaceComponent(desired)],
            )
            .await
            .expect("new stream preview");
        let (source, new_input) = source(Arc::new(Calls::default()));
        let mut bindings = TopologyBindings::default();
        bindings
            .components
            .insert("source".into(), ConstructedComponent::source(source));
        assert_eq!(
            control
                .reconcile(preview, bindings)
                .await
                .expect("replace")
                .summary,
            OperationSummary::Completed
        );
        new_input
            .send(output(root("fresh", 1, &[1])))
            .await
            .expect("independent new stream");
        assert_eq!(output_rx.recv().await, Some(("sink".into(), 1)));
        control.cancel();
    });
    cancelled(result);
}

#[tokio::test]
async fn binding_generations_do_not_regress_after_live_rebinds_and_a_new_run() {
    let (source, input) = source(Arc::new(Calls::default()));
    let (received, _output) = mpsc::channel(1);
    let mut graph = ComputationGraph::builder("binding-generations")
        .source(source)
        .sink(Box::new(sink(
            "sink",
            "sink",
            Arc::new(Calls::default()),
            received,
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.start().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.startup_report().await.expect("startup");
        let mut revision = GraphRevision(1);
        for _ in 0..3 {
            let preview = control
                .preview(revision, vec![DesiredMutation::Bind(relationship("sink"))])
                .await
                .expect("preview");
            let report = control
                .reconcile(preview, TopologyBindings::default())
                .await
                .expect("rebind");
            assert_eq!(report.summary, OperationSummary::Completed);
            revision = report.revision;
        }
        drop(input);
    });
    result.expect("finite completion");
    let previous = graph.observed().relationships[&edge("source", "sink")].generation;
    graph.start().expect("next run").await.expect("completion");
    assert!(graph.observed().relationships[&edge("source", "sink")].generation > previous);
}
