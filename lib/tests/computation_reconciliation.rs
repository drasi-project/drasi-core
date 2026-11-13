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

#![cfg(test)]

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
    initializes: AtomicUsize,
    disposes: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    updates: AtomicUsize,
    creates: AtomicUsize,
    drops: AtomicUsize,
    fail_stop: AtomicBool,
    fail_create: AtomicBool,
    terminal: AtomicBool,
    update_gate: Mutex<Option<Arc<Gate>>>,
    panic_update: AtomicBool,
    panic_update_metadata: AtomicBool,
    panic_preflight: AtomicBool,
    panic_drop: AtomicBool,
    binds_inputs: AtomicBool,
    fail_bind_inputs: AtomicBool,
    bound: Mutex<Vec<Vec<InputBinding>>>,
    validator_tasks: Mutex<Vec<Option<tokio::task::Id>>>,
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
    async fn initialize(&mut self) -> anyhow::Result<()> {
        self.calls.initializes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn dispose(&mut self) -> anyhow::Result<()> {
        self.calls.disposes.fetch_add(1, Ordering::SeqCst);
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
    start_gate: Option<Arc<Gate>>,
    accepted: bool,
}
impl Drop for Sink {
    fn drop(&mut self) {
        self.calls.drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.calls.panic_drop.swap(false, Ordering::SeqCst),
            "retired sink destruction fault"
        );
    }
}
#[async_trait]
impl ComputationComponent for Sink {
    async fn initialize(&mut self) -> anyhow::Result<()> {
        self.calls.initializes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn dispose(&mut self) -> anyhow::Result<()> {
        self.calls.disposes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        if self.calls.panic_update_metadata.load(Ordering::SeqCst)
            && self.calls.updates.load(Ordering::SeqCst) > 0
        {
            panic!("reconfigured metadata fault");
        }
        &self.descriptor
    }
    fn binds_inputs(&self) -> bool {
        self.calls.binds_inputs.load(Ordering::SeqCst)
    }
    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.calls.fail_bind_inputs.load(Ordering::SeqCst),
            "input binding fault"
        );
        self.calls
            .bound
            .lock()
            .expect("bound")
            .push(bindings.to_vec());
        Ok(())
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        if let Some(gate) = self.start_gate.take() {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
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
    async fn apply_update(&mut self, context: ComponentUpdateContext) -> anyhow::Result<()> {
        assert_eq!(
            context.plan,
            ComponentUpdatePlan {
                mode: ComponentUpdateMode::InPlace,
                data: ComponentUpdateData::Preserve,
            }
        );
        assert!(!context.operation_id.is_empty());
        assert_eq!(context.current.descriptor.id(), self.descriptor.id());
        assert_eq!(context.proposed.component_id, *self.descriptor.id());
        let gate = self.calls.update_gate.lock().expect("update gate").take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        assert!(
            !self.calls.panic_update.load(Ordering::SeqCst),
            "reconfiguration fault"
        );
        self.label = context.proposed.configuration()["label"]
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
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &[],
                &["out"],
            ),
            calls,
            events,
        }),
        sender,
    )
}
fn sink(
    id: &drasi_core_types::ComponentId,
    label: &str,
    calls: Arc<Calls>,
    received: mpsc::Sender<(String, u64)>,
) -> Sink {
    Sink {
        descriptor: descriptor(id, &["in"], &[]),
        calls,
        received,
        label: label.into(),
        gate: None,
        start_gate: None,
        accepted: false,
    }
}
fn definition(id: &ComponentId, name: ComponentName) -> DesiredComponent {
    DesiredComponent {
        name,
        descriptor: descriptor(id, &["in"], &[]),
        role: ComponentRole::Sink,
        completion: Some(SinkCompletion::Handled),
        streams: BTreeMap::new(),
        lifecycle: LifecyclePolicy::default(),
        input_merge: InputMergePolicy::Arrival,
        construction: ComponentConstruction::External {
            binding: BindingToken::generate(),
        },
    }
}
fn relationship(to: &ComponentId) -> DesiredRelationship {
    DesiredRelationship {
        definition: edge(&component(77), to),
        policy: RelationshipPolicy::default(),
        pipe: DesiredPipe::Bounded { capacity: 1 },
    }
}
fn bind_sink(definition: &DesiredComponent, sink: Sink) -> TopologyBindings {
    assert_eq!(definition.descriptor.id(), sink.descriptor.id());
    let ComponentConstruction::External { binding } = &definition.construction else {
        panic!("external sink fixture requires an explicit binding token");
    };
    let mut bindings = TopologyBindings::default();
    bindings
        .components
        .insert(binding.clone(), ConstructedComponent::sink(Box::new(sink)));
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
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

macro_rules! both_tokio_flavors {
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
    let mut original = SinkVariant::new(old.clone());
    original.gate = Some(gate.clone());
    let factory = Arc::new(Factory::with_variants(
        component(101),
        ComponentName::try_new("sink").unwrap(),
        "1",
        received,
        [("old", original), ("new", SinkVariant::new(new.clone()))],
    ));
    let mut graph = ComputationGraph::builder("replace")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .component(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            factory.specification("old"),
            factory.clone(),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
            .await
            .expect("input");
        gate.entered.notified().await;
        let old_observed = control.observed().components[&component(101)].clone();
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::ReplaceComponent(factory.desired("new"))],
            )
            .await
            .expect("preview");
        assert_eq!(preview.replaced(), &BTreeSet::from([component(101)]));
        assert!(preview.restarted().is_empty());
        assert!(!preview.paused().contains(&component(77)));
        let stale = preview.clone();
        let replacement = control.reconcile(preview, TopologyBindings::default());
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
                    state.components[&component(101)].lifecycle == ComponentLifecycle::Quiescing
                })
                .await
                .expect("controller requested boundary");
            input
                .send(output(root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "source",
                    2,
                    &[2],
                )))
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
                        component: component(101),
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
both_tokio_flavors!(sink_replacement, replace_sink_at_safe_boundary);

async fn sustained_multihop_fanout_churn() {
    let upstream = Arc::new(Calls::default());
    let stable = Arc::new(Calls::default());
    let (source, input) = source(upstream.clone());
    let (received, mut outputs) = mpsc::channel(8);
    let mut graph = ComputationGraph::builder("sustained-fanout")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .transformer(
            drasi_lib::ComponentName::try_new("middle").expect("fixture component name"),
            Box::new(NativeTransform::new(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c0")
                    .expect("fixture UUID"),
                |input| {
                    Ok(vec![output(derived(
                        &input.envelope,
                        &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001c0")
                            .expect("fixture UUID"),
                        "middle",
                        input.envelope.system().sequence(),
                        input.envelope.changes().clone(),
                    ))])
                },
            )),
        )
        .sink(
            drasi_lib::ComponentName::try_new("stable").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f1")
                    .expect("fixture UUID"),
                "stable",
                stable.clone(),
                received.clone(),
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c0")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("middle"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c0")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c0")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f1")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let mut revision = GraphRevision(1);
        for cycle in 0..128 {
            let old = Arc::new(Calls::default());
            let new = Arc::new(Calls::default());
            let gate = Arc::new(Gate::default());
            let mut consumer = SinkVariant::new(old.clone());
            consumer.gate = Some(gate.clone());
            let factory = Arc::new(Factory::with_variants(
                component(1010),
                ComponentName::try_new("transient").unwrap(),
                &cycle.to_string(),
                received.clone(),
                [("old", consumer), ("new", SinkVariant::new(new.clone()))],
            ));
            let preview = control
                .preview(
                    revision,
                    vec![
                        DesiredMutation::PutComponent(factory.desired("old")),
                        DesiredMutation::Bind(DesiredRelationship {
                            definition: edge(
                                &drasi_core_types::ComponentId::try_new(
                                    "00000000-0000-0000-0000-0000000001c0",
                                )
                                .expect("fixture UUID"),
                                &drasi_core_types::ComponentId::try_new(
                                    "00000000-0000-0000-0000-0000000003f2",
                                )
                                .expect("fixture UUID"),
                            ),
                            policy: RelationshipPolicy::default(),
                            pipe: DesiredPipe::Bounded { capacity: 1 },
                        }),
                    ],
                )
                .await
                .expect("add preview");
            let mut bindings = TopologyBindings::default();
            bindings
                .factories
                .register(factory.clone())
                .expect("fixture factory");
            let added = control.reconcile(preview, bindings).await.expect("add");
            assert_eq!(added.summary, OperationSummary::Completed);
            revision = added.revision;
            let first = cycle * 2 + 1;
            input
                .send(output(root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "source",
                    first,
                    &[1],
                )))
                .await
                .expect("input");
            gate.entered.notified().await;
            assert_eq!(outputs.recv().await, Some(("stable".into(), first)));

            let old_observed = control.observed().components[&component(1010)].clone();
            let preview = control
                .preview(
                    revision,
                    vec![DesiredMutation::ReplaceComponent(factory.desired("new"))],
                )
                .await
                .expect("replace preview");
            let replacement = control.reconcile(preview, TopologyBindings::default());
            let (report, ()) = tokio::join!(replacement, async {
                let mut observed = control.subscribe_observed();
                observed
                    .wait_for(|state| {
                        state.components[&component(1010)].lifecycle
                            == ComponentLifecycle::Quiescing
                    })
                    .await
                    .expect("handling boundary");
                assert_eq!(old.stops.load(Ordering::SeqCst), 0);
                assert_eq!(old.drops.load(Ordering::SeqCst), 0);
                input
                    .send(output(root(
                        &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                            .expect("fixture UUID"),
                        "source",
                        first + 1,
                        &[2],
                    )))
                    .await
                    .expect("queued input");
                gate.release.notify_one();
            });
            let report = report.expect("replacement");
            assert_eq!(report.summary, OperationSummary::Completed);
            revision = report.revision;
            let actual = [
                outputs.recv().await.expect("old completion"),
                outputs.recv().await.expect("stable branch"),
                outputs.recv().await.expect("replacement branch"),
            ];
            assert_eq!(
                BTreeSet::from(actual),
                BTreeSet::from([
                    ("old".into(), first),
                    ("stable".into(), first + 1),
                    ("new".into(), first + 1),
                ])
            );
            assert_eq!(old.starts.load(Ordering::SeqCst), 1);
            assert_eq!(old.stops.load(Ordering::SeqCst), 1);
            assert_eq!(old.drops.load(Ordering::SeqCst), 1);
            assert!(matches!(
                control
                    .report_health(
                        revision,
                        HealthObservation {
                            component: component(1010),
                            generation: old_observed.generation,
                            operation: old_observed.operation,
                            health: ComponentHealth::Healthy,
                        }
                    )
                    .await,
                Err(GraphError::StaleGeneration)
            ));

            let preview = control
                .preview(
                    revision,
                    vec![DesiredMutation::RemoveComponents {
                        selection: GraphSelection::Exact(vec![component(1010)]),
                        policy: RemovalPolicy::Drain,
                    }],
                )
                .await
                .expect("remove preview");
            let report = control
                .reconcile(preview, TopologyBindings::default())
                .await
                .expect("remove");
            assert_eq!(report.summary, OperationSummary::Completed);
            revision = report.revision;
            assert_eq!(new.starts.load(Ordering::SeqCst), 1);
            assert_eq!(new.stops.load(Ordering::SeqCst), 1);
            assert_eq!(new.drops.load(Ordering::SeqCst), 1);
            assert_eq!(control.desired_snapshot().nodes.len(), 3);
            assert_eq!(control.observed().components.len(), 3);
            assert!(outputs.try_recv().is_err(), "unexpected duplicate delivery");
        }
        assert_eq!(upstream.starts.load(Ordering::SeqCst), 1);
        assert_eq!(stable.starts.load(Ordering::SeqCst), 1);
        control.cancel();
    });
    cancelled(result);
    graph.dispose().await.expect("release graph");
    drop(graph);
    assert_eq!(upstream.stops.load(Ordering::SeqCst), 1);
    assert_eq!(stable.stops.load(Ordering::SeqCst), 1);
    assert_eq!(stable.drops.load(Ordering::SeqCst), 1);
}

both_tokio_flavors!(sustained_multihop_churn, sustained_multihop_fanout_churn);

async fn add_remove_preserves_stable_indices() {
    let upstream = Arc::new(Calls::default());
    let original = Arc::new(Calls::default());
    let (source, input) = source(upstream.clone());
    let (received, mut output_rx) = mpsc::channel(16);
    let mut graph = ComputationGraph::builder("live-topology")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .sink(
            drasi_lib::ComponentName::try_new("original").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000349")
                    .expect("fixture UUID"),
                "original",
                original.clone(),
                received.clone(),
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000349")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let mut revision = GraphRevision(1);
        for (sequence, id, label) in [(1, component(768), "second"), (2, component(795), "third")] {
            let added = Arc::new(Calls::default());
            let desired = definition(&id, ComponentName::try_new(label).unwrap());
            let preview = control
                .preview(
                    revision,
                    vec![
                        DesiredMutation::PutComponent(desired.clone()),
                        DesiredMutation::Bind(relationship(&id)),
                    ],
                )
                .await
                .expect("preview add");
            let report = control
                .reconcile(
                    preview,
                    bind_sink(&desired, sink(&id, label, added.clone(), received.clone())),
                )
                .await
                .expect("add");
            assert_eq!(report.summary, OperationSummary::Completed);
            revision = report.revision;
            input
                .send(output(root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
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
                BTreeSet::from([("original".into(), sequence), (label.into(), sequence)])
            );
            let preview = control
                .preview(
                    revision,
                    vec![DesiredMutation::RemoveComponents {
                        selection: GraphSelection::Exact(vec![id]),
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
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                3,
                &[3],
            )))
            .await
            .expect("original remains live");
        assert_eq!(output_rx.recv().await, Some(("original".into(), 3)));
        control.cancel();
    });
    cancelled(result);
}
both_tokio_flavors!(live_add_remove, add_remove_preserves_stable_indices);

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
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                downstream.clone(),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(RecordingProvider(senders.clone())),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
            .await
            .expect("input");
        assert_eq!(output_rx.recv().await, Some(("sink".into(), 1)));
        let old = senders.lock().expect("senders")[0].clone();
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::Bind(relationship(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                ))],
            )
            .await
            .expect("preview");
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("rebind");
        assert_eq!(report.summary, OperationSummary::Completed);
        assert!(matches!(
            old.send(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                99,
                &[99]
            ))
            .await
            .expect_err("stale sender")
            .error,
            PipeError::Closed
        ));
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                2,
                &[2],
            )))
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
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                Arc::new(Calls::default()),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .relationship_policy(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            policy.clone(),
        )
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
                    edge: edge(
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-00000000004d",
                        )
                        .expect("fixture UUID"),
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-000000000065",
                        )
                        .expect("fixture UUID"),
                    ),
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
            control.observed().relationships[&edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID")
            )]
                .binding,
            BindingState::Declared
        );
        assert_eq!(
            control.observed().components[&component(77)].lifecycle,
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
                    ..relationship(
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-000000000065",
                        )
                        .expect("fixture UUID"),
                    )
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
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
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
    let mut consumer = sink(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
        "sink",
        downstream.clone(),
        received,
    );
    consumer.gate = Some(gate.clone());
    let mut graph = ComputationGraph::builder("drain")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(consumer),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
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
                    selection: GraphSelection::Exact(vec![component(77)]),
                    policy: RemovalPolicy::Reject,
                }]
            )
            .await
            .is_err());
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
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
            control.observed().components[&component(101)]
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
    let mut consumer = sink(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
        "sink",
        calls.clone(),
        received,
    );
    consumer.accepted = true;
    let mut graph = ComputationGraph::builder("accepted")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(consumer),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
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

#[derive(Clone)]
struct SinkVariant {
    calls: Arc<Calls>,
    gate: Option<Arc<Gate>>,
    start_gate: Option<Arc<Gate>>,
}

impl SinkVariant {
    fn new(calls: Arc<Calls>) -> Self {
        Self {
            calls,
            gate: None,
            start_gate: None,
        }
    }
}

struct Factory {
    descriptor: FactoryDescriptor,
    calls: Arc<Calls>,
    received: mpsc::Sender<(String, u64)>,
    binds: bool,
    component_id: ComponentId,
    component_name: ComponentName,
    variants: BTreeMap<String, SinkVariant>,
}
impl Factory {
    fn binding(mut self) -> Self {
        self.binds = true;
        self
    }
    fn new(calls: Arc<Calls>, received: mpsc::Sender<(String, u64)>) -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test-sink", "1")
                    .expect("implementation"),
                role: ComponentRole::Sink,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: ["label", "strategy"]
                        .into_iter()
                        .map(|field| {
                            (
                                Arc::from(field),
                                ConfigurationField {
                                    constraint: None,
                                    value_type: ConfigurationType::String,
                                    required: true,
                                    secret: false,
                                },
                            )
                        })
                        .collect(),
                    allow_additional: false,
                },
                dependencies: BTreeMap::new(),
            },
            calls,
            received,
            binds: false,
            component_id: component(101),
            component_name: ComponentName::try_new("sink").unwrap(),
            variants: BTreeMap::new(),
        }
    }
    fn with_variants(
        component_id: ComponentId,
        component_name: ComponentName,
        version: &str,
        received: mpsc::Sender<(String, u64)>,
        variants: impl IntoIterator<Item = (&'static str, SinkVariant)>,
    ) -> Self {
        let mut factory = Self::new(Arc::default(), received);
        factory.component_id = component_id;
        factory.component_name = component_name;
        factory.descriptor.implementation =
            ImplementationIdentity::try_new(format!("test-sink/{}", factory.component_id), version)
                .expect("fixture implementation");
        factory.variants = variants
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
        factory
    }
    fn specification(&self, label: &str) -> ComponentSpecification {
        ComponentSpecification {
            descriptor: descriptor(&self.component_id, &["in"], &[]),
            role: ComponentRole::Sink,
            completion: Some(SinkCompletion::Handled),
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([
                (
                    Arc::from("label"),
                    ConfigurationValue::Literal(label.into()),
                ),
                (
                    Arc::from("strategy"),
                    ConfigurationValue::Literal("inPlace".into()),
                ),
            ]),
            dependencies: BTreeMap::new(),
        }
    }
    fn replacement_specification(&self, label: &str) -> ComponentSpecification {
        let mut specification = self.specification(label);
        specification.configuration.insert(
            "strategy".into(),
            ConfigurationValue::Literal("recreate".into()),
        );
        specification
    }
    fn desired(&self, label: &str) -> DesiredComponent {
        let mut desired = definition(&self.component_id, self.component_name.clone());
        desired.construction =
            ComponentConstruction::Factory(self.replacement_specification(label));
        desired
    }
}
#[async_trait]
impl ComponentFactory for Factory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn plan_update(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        anyhow::ensure!(
            current.descriptor == proposed.descriptor
                && current.implementation == proposed.implementation
                && current.role == proposed.role,
            "fixture update changed its immutable interface"
        );
        let mode = match proposed.configuration.get("strategy") {
            Some(ConfigurationValue::Literal(value)) if value == "inPlace" => {
                ComponentUpdateMode::InPlace
            }
            Some(ConfigurationValue::Literal(value)) if value == "recreate" => {
                ComponentUpdateMode::Recreate
            }
            _ => anyhow::bail!("unsupported fixture update strategy"),
        };
        if !self.variants.is_empty() {
            for specification in [current, proposed] {
                anyhow::ensure!(
                    matches!(specification.configuration.get("label"),
                    Some(ConfigurationValue::Literal(value))
                        if value.as_str().is_some_and(|label| self.variants.contains_key(label))),
                    "fixture has no constructor for this version"
                );
            }
        }
        Ok(ComponentUpdatePlan {
            mode,
            data: ComponentUpdateData::Preserve,
        })
    }
    fn binds_inputs(&self) -> bool {
        self.binds
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        self.calls
            .validator_tasks
            .lock()
            .expect("validator tasks")
            .push(tokio::task::try_id());
        assert!(
            !self.calls.panic_preflight.load(Ordering::SeqCst),
            "preflight callback fault"
        );
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        let label = context.configuration()["label"].as_str().expect("label");
        let variant = self.variants.get(label);
        let calls = variant.map_or_else(|| self.calls.clone(), |variant| variant.calls.clone());
        calls.creates.fetch_add(1, Ordering::SeqCst);
        calls
            .bound
            .lock()
            .expect("bound")
            .push(context.input_bindings().to_vec());
        if calls.fail_create.swap(false, Ordering::SeqCst) {
            return Err(if calls.terminal.load(Ordering::SeqCst) {
                ComponentCreationError::terminal(anyhow::anyhow!("invalid runtime configuration"))
            } else {
                ComponentCreationError::retryable(anyhow::anyhow!(
                    "temporarily unavailable resource"
                ))
            });
        }
        let mut component = sink(&self.component_id, label, calls, self.received.clone());
        assert_eq!(component.descriptor, context.specification.descriptor);
        if let Some(variant) = variant {
            component.gate = variant.gate.clone();
            component.start_gate = variant.start_gate.clone();
        }
        Ok(ConstructedComponent::sink(Box::new(component)))
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
            .source(
                drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
                source,
            )
            .component(
                drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
                factory.specification("first"),
                factory.clone(),
            )
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            )
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()
            .expect("graph");
        let run = graph.run().expect("scope");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            let deployment = control.deployment_report().await.expect("deploy");
            assert!(matches!(
                deployment.components[&component(101)],
                CreationOutcome::CreationFailed(_)
            ));
            let old_generation = control.observed().components[&component(101)].generation;
            let retry = control
                .preview(
                    GraphRevision(1),
                    vec![DesiredMutation::Retry(GraphSelection::Exact(vec![component(101)]))],
                )
                .await;
            let preview = if terminal {
                assert!(retry.is_err());
                let mut desired = definition(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                    ComponentName::try_new("sink").unwrap(),
                );
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
            assert!(control.observed().components[&component(101)].generation > old_generation);
            input
                .send(output(root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "source",
                    1,
                    &[1],
                )))
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
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .component(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            factory.specification("before"),
            factory.clone(),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let generation = control.observed().components[&component(101)].generation;
        let mut desired = definition(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            ComponentName::try_new("sink").unwrap(),
        );
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
            control.observed().components[&component(101)].generation,
            generation
        );
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
            .await
            .expect("input");
        assert_eq!(output_rx.recv().await, Some(("after".into(), 1)));
        assert_eq!(calls.creates.load(Ordering::SeqCst), 1);
        assert_eq!(calls.starts.load(Ordering::SeqCst), 1);
        assert_eq!(calls.stops.load(Ordering::SeqCst), 0);
        assert_eq!(calls.updates.load(Ordering::SeqCst), 1);
        assert_eq!(calls.initializes.load(Ordering::SeqCst), 1);
        assert_eq!(calls.disposes.load(Ordering::SeqCst), 0);
        assert_eq!(upstream.starts.load(Ordering::SeqCst), 1);
        control.cancel();
    });
    cancelled(result);
}

async fn pending_reconfiguration_keeps_disjoint_lifecycle_and_caller_cancellation_safe() {
    let calls = Arc::new(Calls::default());
    let healthy = Arc::new(Calls::default());
    let gate = Arc::new(Gate::default());
    *calls.update_gate.lock().expect("update gate") = Some(gate.clone());
    let (received, mut output_rx) = mpsc::channel(8);
    let factory = Arc::new(Factory::new(calls.clone(), received.clone()));
    let (source, input) = source(Arc::new(Calls::default()));
    let mut graph = ComputationGraph::builder("owned-reconfiguration")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .component(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            factory.specification("before"),
            factory.clone(),
        )
        .sink(
            drasi_lib::ComponentName::try_new("healthy").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000314")
                    .expect("fixture UUID"),
                "healthy",
                healthy.clone(),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000314")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let generation = control.observed().components[&component(101)].generation;
        let mut desired = definition(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            ComponentName::try_new("sink").unwrap(),
        );
        desired.construction = ComponentConstruction::Factory(factory.specification("after"));
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::UpdateComponent(desired)],
            )
            .await
            .expect("preview");
        {
            let update = control.reconcile(preview, TopologyBindings::default());
            tokio::pin!(update);
            tokio::select! {
                result = &mut update => panic!("update completed without release: {result:?}"),
                _ = gate.entered.notified() => {}
            }
            let revision = control.desired_snapshot().revision;
            let healthy_handle = control
                .component_handle_by_id(&component(788))
                .expect("healthy component");
            let healthy_generation = healthy_handle.generation();
            for notification in 0..3 {
                control
                    .observe_resources(
                        &component(788),
                        healthy_generation,
                        vec![(
                            ResourceSpecification {
                                id: ResourceId::try_new("provider").unwrap(),
                                role: ResourceRole::StateStore,
                                ownership: ResourceOwnership::Borrowed,
                                binding: format!("notification-{notification}").into(),
                            },
                            ResourceHandle::new(ResourceRole::StateStore, Arc::new(notification)),
                        )],
                    )
                    .unwrap();
            }
            for _ in 0..2 {
                tokio::time::timeout(Duration::from_millis(500), healthy_handle.stop())
                    .await
                    .expect("independent stop is serviceable")
                    .expect("stop");
                assert_eq!(
                    healthy_handle.observed().unwrap().lifecycle,
                    ComponentLifecycle::Stopped
                );
                tokio::time::timeout(Duration::from_millis(500), healthy_handle.start())
                    .await
                    .expect("independent start is serviceable")
                    .expect("start");
                assert_eq!(
                    healthy_handle.observed().unwrap().lifecycle,
                    ComponentLifecycle::Running
                );
            }
            assert!(
                control.desired_snapshot().revision.0 > revision.0,
                "committed reconfiguration must not defer independent metadata publication"
            );
            assert!(matches!(
                control
                    .start_requested(
                        control.desired_snapshot().revision,
                        GraphSelection::Exact(vec![component(101)])
                    )
                    .await,
                Err(GraphError::OperationInProgress)
            ));
        }
        assert_eq!(calls.updates.load(Ordering::SeqCst), 0);
        assert_eq!(healthy.stops.load(Ordering::SeqCst), 2);
        gate.release.notify_one();
        let mut observed = control.subscribe_observed();
        tokio::time::timeout(
            Duration::from_secs(2),
            observed.wait_for(|state| {
                state.components[&component(101)].lifecycle == ComponentLifecycle::Running
                    && state.revision.0 > 2
            }),
        )
        .await
        .expect("accepted update survives caller cancellation")
        .expect("observation");
        assert_eq!(calls.updates.load(Ordering::SeqCst), 1);
        assert_eq!(calls.creates.load(Ordering::SeqCst), 1);
        let metadata: Vec<_> = control
            .desired_snapshot()
            .resources
            .values()
            .map(|resource| resource.binding.clone())
            .collect();
        assert_eq!(metadata, [Arc::<str>::from("notification-2")]);
        assert_eq!(
            control.observed().components[&component(101)].generation,
            generation
        );
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
            .await
            .expect("input");
        let mut actual =
            vec![output_rx.recv().await.expect("first"), output_rx.recv().await.expect("second")];
        actual.sort();
        assert_eq!(actual, [("after".into(), 1), ("healthy".into(), 1)]);
        control.cancel();
    });
    cancelled(result);
}
both_tokio_flavors!(
    owned_reconfiguration_and_disjoint_commands,
    pending_reconfiguration_keeps_disjoint_lifecycle_and_caller_cancellation_safe
);

async fn force_cancellation_joins_owned_reconfiguration_and_defers_reactivation() {
    let calls = Arc::new(Calls::default());
    let gate = Arc::new(Gate::default());
    *calls.update_gate.lock().unwrap() = Some(gate.clone());
    let (received, _output) = mpsc::channel(8);
    let factory = Arc::new(Factory::new(calls.clone(), received));
    let (source, _input) = source(Arc::default());
    let mut graph = ComputationGraph::builder("forced-reconfiguration")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .component(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            factory.specification("before"),
            factory.clone(),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let generation = control.observed().components[&component(101)].generation;
        let mut desired = definition(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            ComponentName::try_new("sink").unwrap(),
        );
        desired.construction = ComponentConstruction::Factory(factory.specification("after"));
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::UpdateComponent(desired.clone())],
            )
            .await
            .unwrap();
        let update = control.reconcile(preview, TopologyBindings::default());
        tokio::pin!(update);
        tokio::select! {
            result = &mut update => panic!("update completed before cancellation: {result:?}"),
            _ = gate.entered.notified() => {}
        }
        let forced = control
            .force_cancel_components(
                control.desired_snapshot().revision,
                GraphSelection::Exact(vec![component(101)]),
            )
            .await
            .unwrap();
        assert_eq!(forced.cleanup.summary, OperationSummary::Completed);
        assert_eq!(
            forced.interrupted[&component(101)].phase,
            FailurePhase::Creation
        );
        let report = update.await.unwrap();
        assert!(report.committed);
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        assert_eq!(calls.updates.load(Ordering::SeqCst), 0);
        assert_eq!(calls.starts.load(Ordering::SeqCst), 1);
        assert_eq!(
            control.observed().components[&component(101)].generation,
            generation
        );
        let restart = control
            .start_requested(
                control.desired_snapshot().revision,
                GraphSelection::Exact(vec![component(101)]),
            )
            .await
            .unwrap();
        assert_eq!(restart.summary, OperationSummary::CompletedWithFailures);
        desired.construction =
            ComponentConstruction::Factory(factory.replacement_specification("repaired"));
        let repair = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::ReplaceComponent(desired)],
            )
            .await
            .unwrap();
        let repaired = control
            .reconcile(repair, TopologyBindings::default())
            .await
            .unwrap();
        assert_eq!(
            repaired.summary,
            OperationSummary::Completed,
            "{repaired:?}"
        );
        assert_ne!(
            control.observed().components[&component(101)].generation,
            generation
        );
        assert_eq!(calls.creates.load(Ordering::SeqCst), 2);
        assert_eq!(calls.starts.load(Ordering::SeqCst), 2);
        control.cancel();
    });
    cancelled(result);
}
both_tokio_flavors!(
    force_owned_reconfiguration,
    force_cancellation_joins_owned_reconfiguration_and_defers_reactivation
);

async fn retired_component_faults_fence_only_the_affected_replacement() {
    let calls = Arc::new(Calls::default());
    let (received, mut output_rx) = mpsc::channel(8);
    let factory = Arc::new(Factory::new(calls.clone(), received.clone()));
    let (source, input) = source(Arc::new(Calls::default()));
    let mut graph = ComputationGraph::builder("owned-retirement")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .component(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            factory.specification("before"),
            factory.clone(),
        )
        .sink(
            drasi_lib::ComponentName::try_new("healthy").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000314")
                    .expect("fixture UUID"),
                "healthy",
                Arc::default(),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000314")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        calls.panic_drop.store(true, Ordering::SeqCst);
        let mut desired = definition(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            ComponentName::try_new("sink").unwrap(),
        );
        desired.construction =
            ComponentConstruction::Factory(factory.replacement_specification("after"));
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::ReplaceComponent(desired.clone())],
            )
            .await
            .unwrap();
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .unwrap();
        assert!(report.committed);
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        assert_eq!(
            calls.creates.load(Ordering::SeqCst),
            1,
            "failed retirement prevents construction"
        );
        assert!(control.observed().components[&component(101)]
            .failure
            .is_some());
        let restart = control
            .start_requested(report.revision, GraphSelection::Exact(vec![component(101)]))
            .await
            .unwrap();
        assert_eq!(restart.summary, OperationSummary::CompletedWithFailures);
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
            .await
            .unwrap();
        assert_eq!(output_rx.recv().await, Some(("healthy".into(), 1)));
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::ReplaceComponent(desired)],
            )
            .await
            .unwrap();
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .unwrap();
        assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
        assert_eq!(calls.creates.load(Ordering::SeqCst), 2);
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                2,
                &[2],
            )))
            .await
            .unwrap();
        let mut actual = vec![
            output_rx.recv().await.unwrap(),
            output_rx.recv().await.unwrap(),
            output_rx.recv().await.unwrap(),
        ];
        actual.sort();
        assert_eq!(
            actual,
            [("after".into(), 1), ("after".into(), 2), ("healthy".into(), 2)]
        );
        control.cancel();
    });
    cancelled(result);
}
both_tokio_flavors!(
    owned_retirement_faults,
    retired_component_faults_fence_only_the_affected_replacement
);

struct FaultingMetadata {
    tasks: Arc<Mutex<Vec<Option<tokio::task::Id>>>>,
}
impl Drop for FaultingMetadata {
    fn drop(&mut self) {
        self.tasks.lock().unwrap().push(tokio::task::try_id());
        panic!("retired metadata provider fault");
    }
}

async fn resource_metadata_destruction_is_owned_and_quarantines_its_generation() {
    let calls = Arc::new(Calls::default());
    let (received, mut output_rx) = mpsc::channel(8);
    let factory = Arc::new(Factory::new(calls, received.clone()));
    let (source, input) = source(Arc::default());
    let tasks = Arc::new(Mutex::new(Vec::new()));
    let mut graph = ComputationGraph::builder("owned-metadata-retirement")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .component(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            factory.specification("bad"),
            factory,
        )
        .sink(
            drasi_lib::ComponentName::try_new("healthy").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000314")
                    .expect("fixture UUID"),
                "healthy",
                Arc::default(),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000314")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let generation = control.observed().components[&component(101)].generation;
        control
            .observe_resources(
                &component(101),
                generation,
                vec![(
                    ResourceSpecification {
                        id: ResourceId::try_new("metadata").unwrap(),
                        role: ResourceRole::StateStore,
                        ownership: ResourceOwnership::Borrowed,
                        binding: "metadata".into(),
                    },
                    ResourceHandle::new(
                        ResourceRole::StateStore,
                        Arc::new(FaultingMetadata {
                            tasks: tasks.clone(),
                        }),
                    ),
                )],
            )
            .unwrap();
        let mut observed = control.subscribe_observed();
        observed
            .wait_for(|state| state.resources.len() == 1)
            .await
            .unwrap();
        assert!(tasks.lock().unwrap().is_empty());
        control
            .observe_resources(&component(101), generation, Vec::new())
            .unwrap();
        observed
            .wait_for(|state| {
                state.components[&component(101)]
                    .failure
                    .as_ref()
                    .is_some_and(|failure| {
                        failure.phase == FailurePhase::Removal
                            && failure.disposition == FailureDisposition::Terminal
                    })
            })
            .await
            .unwrap();
        {
            let tasks = tasks.lock().unwrap();
            assert_eq!(tasks.len(), 1);
            assert!(
                tasks[0].is_some(),
                "metadata destruction must run in an owned task"
            );
        }
        assert_eq!(
            control.observed().components[&component(101)].generation,
            generation
        );
        assert_eq!(
            control.observed().components[&component(101)].health,
            ComponentHealth::Unavailable
        );
        let restart = control
            .start_requested(
                control.desired_snapshot().revision,
                GraphSelection::Exact(vec![component(101)]),
            )
            .await
            .unwrap();
        assert_eq!(restart.summary, OperationSummary::CompletedWithFailures);
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
            .await
            .unwrap();
        assert_eq!(output_rx.recv().await, Some(("healthy".into(), 1)));
        control.cancel();
    });
    let Err(GraphError::Cleanup { primary, errors }) = result else {
        panic!("metadata retirement failure must remain visible during cleanup: {result:?}");
    };
    assert!(matches!(primary.as_deref(), Some(GraphError::Cancelled)));
    assert_eq!(errors.len(), 1);
    let GraphError::Component {
        component: failed,
        operation,
        source,
    } = errors[0].underlying()
    else {
        panic!("expected the retained metadata retirement failure: {errors:?}");
    };
    assert_eq!(failed, &component(101));
    assert_eq!(*operation, "retire resource metadata");
    assert!(source.downcast_ref::<ComponentPanic>().is_some());
    assert!(source
        .to_string()
        .contains("retired metadata provider fault"));
}
both_tokio_flavors!(
    owned_metadata_retirement,
    resource_metadata_destruction_is_owned_and_quarantines_its_generation
);

async fn reconfiguration_and_metadata_panics_quarantine_only_the_changed_generation() {
    for fault in 0..3 {
        let calls = Arc::new(Calls::default());
        let (received, mut output_rx) = mpsc::channel(8);
        let factory = Arc::new(Factory::new(calls.clone(), received.clone()));
        let (source, input) = source(Arc::new(Calls::default()));
        let mut graph = ComputationGraph::builder("reconfiguration-fault")
            .source(
                drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
                source,
            )
            .component(
                drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
                factory.specification("before"),
                factory.clone(),
            )
            .sink(
                drasi_lib::ComponentName::try_new("healthy").expect("fixture component name"),
                Box::new(sink(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000314")
                        .expect("fixture UUID"),
                    "healthy",
                    Arc::new(Calls::default()),
                    received,
                )),
            )
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            )
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000314")
                        .expect("fixture UUID"),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .cleanup_timeout(Duration::from_millis(100))
            .build()
            .expect("graph");
        let run = graph.run().expect("scope");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            activate(&control).await;
            calls.panic_update.store(fault == 0, Ordering::SeqCst);
            calls
                .panic_update_metadata
                .store(fault == 1, Ordering::SeqCst);
            if fault == 2 {
                *calls.update_gate.lock().expect("update gate") = Some(Arc::default());
            }
            let mut desired = definition(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                ComponentName::try_new("sink").unwrap(),
            );
            desired.construction = ComponentConstruction::Factory(factory.specification("after"));
            let preview = control
                .preview(
                    GraphRevision(1),
                    vec![DesiredMutation::UpdateComponent(desired)],
                )
                .await
                .expect("preview");
            let report = control
                .reconcile(preview, TopologyBindings::default())
                .await
                .expect("committed report");
            assert!(report.committed);
            assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
            let state = control.observed();
            let failed = state.components[&component(101)]
                .failure
                .as_ref()
                .expect("retained failure");
            assert_eq!(failed.disposition, FailureDisposition::Terminal);
            assert!(failed.cause.to_string().contains(if fault == 2 {
                "interrupted"
            } else {
                "panicked"
            }));
            input
                .send(output(root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "source",
                    1,
                    &[1],
                )))
                .await
                .expect("input");
            assert_eq!(output_rx.recv().await, Some(("healthy".into(), 1)));
            assert_eq!(
                control.observed().components[&component(788)].lifecycle,
                ComponentLifecycle::Running
            );
            let stop = control
                .stop_components(report.revision, GraphSelection::Exact(vec![component(101)]))
                .await
                .expect("guarded cleanup");
            assert_eq!(stop.summary, OperationSummary::Completed);
            let restart = control
                .start_requested(report.revision, GraphSelection::Exact(vec![component(101)]))
                .await
                .expect("restart report");
            assert_eq!(restart.summary, OperationSummary::CompletedWithFailures);
            control.cancel();
        });
        cancelled(result);
    }
}
both_tokio_flavors!(
    owned_reconfiguration_panic_boundaries,
    reconfiguration_and_metadata_panics_quarantine_only_the_changed_generation
);

async fn owned_preflight_panics_leave_the_previous_topology_and_traffic_usable() {
    let calls = Arc::new(Calls::default());
    let (received, mut output_rx) = mpsc::channel(8);
    let factory = Arc::new(Factory::new(calls.clone(), received));
    let (source, input) = source(Arc::new(Calls::default()));
    let mut graph = ComputationGraph::builder("owned-preflight")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .component(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            factory.specification("before"),
            factory.clone(),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        calls
            .validator_tasks
            .lock()
            .expect("validator tasks")
            .clear();
        let driver_task = tokio::task::try_id();
        let mut desired = definition(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            ComponentName::try_new("sink").unwrap(),
        );
        desired.construction = ComponentConstruction::Factory(factory.specification("after"));
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::UpdateComponent(desired)],
            )
            .await
            .unwrap();
        calls.panic_preflight.store(true, Ordering::SeqCst);
        let error = control
            .reconcile(preview.clone(), TopologyBindings::default())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("panicked during Validation"),
            "{error}"
        );
        assert_eq!(control.desired_snapshot().revision, GraphRevision(1));
        assert_eq!(calls.stops.load(Ordering::SeqCst), 0);
        assert_eq!(calls.updates.load(Ordering::SeqCst), 0);
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
            .await
            .unwrap();
        assert_eq!(output_rx.recv().await, Some(("before".into(), 1)));
        calls.panic_preflight.store(false, Ordering::SeqCst);
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .unwrap();
        assert_eq!(report.summary, OperationSummary::Completed);
        {
            let tasks = calls.validator_tasks.lock().expect("validator tasks");
            assert!(!tasks.is_empty());
            assert!(
                tasks
                    .iter()
                    .all(|task| task.is_some() && *task != driver_task),
                "extension validation must run in owned tasks, not the controller"
            );
        }
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                2,
                &[2],
            )))
            .await
            .unwrap();
        assert_eq!(output_rx.recv().await, Some(("after".into(), 2)));
        control.cancel();
    });
    cancelled(result);
}
both_tokio_flavors!(
    owned_preflight_faults,
    owned_preflight_panics_leave_the_previous_topology_and_traffic_usable
);

struct FaultingProvider {
    fault: Arc<AtomicBool>,
}
impl PipeProvider for FaultingProvider {
    fn capabilities(&self) -> Result<PipeCapabilities, PipeError> {
        BoundedPipeConfig { capacity: 2 }.capabilities()
    }
    fn create(&self) -> Result<ProvidedPipe, PipeError> {
        assert!(
            !self.fault.load(Ordering::SeqCst),
            "pipe construction fault"
        );
        BoundedPipeConfig { capacity: 2 }.create()
    }
}

async fn owned_pipe_faults_do_not_terminate_disjoint_lifecycle_work() {
    let (source, _input) = source(Arc::new(Calls::default()));
    let (received, _output) = mpsc::channel(8);
    let healthy = Arc::new(Calls::default());
    let mut graph = ComputationGraph::builder("owned-pipe")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                Arc::new(Calls::default()),
                received.clone(),
            )),
        )
        .sink(
            drasi_lib::ComponentName::try_new("healthy").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000314")
                    .expect("fixture UUID"),
                "healthy",
                healthy.clone(),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000314")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let capabilities = BoundedPipeConfig { capacity: 2 }.capabilities().unwrap();
        let mut replacement = relationship(&component(101));
        let binding = BindingToken::generate();
        replacement.pipe = DesiredPipe::External {
            binding: binding.clone(),
            capabilities: capabilities.supported().iter().copied().collect(),
            capacity: Some(2),
            resources: BTreeMap::new(),
            exclusive_resources: Vec::new(),
        };
        let preview = control
            .preview(GraphRevision(1), vec![DesiredMutation::Bind(replacement)])
            .await
            .unwrap();
        let mut bindings = TopologyBindings::default();
        bindings.pipes.insert(
            binding,
            Box::new(FaultingProvider {
                fault: Arc::new(AtomicBool::new(true)),
            }),
        );
        let report = control.reconcile(preview, bindings).await.unwrap();
        assert!(report.committed);
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        let state = control.observed();
        let failure = state.relationships[&edge(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
        )]
            .failure
            .as_ref()
            .unwrap();
        assert!(failure
            .cause
            .to_string()
            .contains("pipe construction fault"));
        assert_eq!(failure.phase, FailurePhase::Binding);
        let revision = control.desired_snapshot().revision;
        for _ in 0..2 {
            control
                .stop_components(revision, GraphSelection::Exact(vec![component(788)]))
                .await
                .unwrap();
            control
                .start_requested(revision, GraphSelection::Exact(vec![component(788)]))
                .await
                .unwrap();
        }
        assert_eq!(healthy.stops.load(Ordering::SeqCst), 2);
        assert_eq!(healthy.starts.load(Ordering::SeqCst), 3);
        control.cancel();
    });
    cancelled(result);
}
both_tokio_flavors!(
    owned_pipe_faults,
    owned_pipe_faults_do_not_terminate_disjoint_lifecycle_work
);

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
                .send(root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "source",
                    sequence,
                    &[1],
                ))
                .await
                .expect("retained admission");
        }
        drop(old);
        let (source, _input) = source(Arc::new(Calls::default()));
        let (received, mut output_rx) = mpsc::channel(8);
        let gate = Arc::new(Gate::default());
        let mut consumer = sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            "sink",
            Arc::new(Calls::default()),
            received,
        );
        if remove {
            consumer.gate = Some(gate.clone());
        }
        let mut graph = ComputationGraph::builder("retained-boundary")
            .source(
                drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
                source,
            )
            .sink(
                drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
                Box::new(consumer),
            )
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            )
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                ),
                Box::new(retained_config("store")),
            )
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
                    ..relationship(
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-000000000065",
                        )
                        .expect("fixture UUID"),
                    )
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
                            state.components[&component(77)].lifecycle
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
both_tokio_flavors!(retained_reconciliation, retained_backlog_boundaries);

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
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                .expect("fixture UUID"),
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
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .transformer(
            drasi_lib::ComponentName::try_new("transform").expect("fixture component name"),
            Box::new(GatedTransform {
                descriptor: descriptor(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                        .expect("fixture UUID"),
                    &["in"],
                    &["out"],
                ),
                gate: Some(gate.clone()),
                sequence: 0,
            }),
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                Arc::new(Calls::default()),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("transform"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
            .await
            .expect("first");
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                2,
                &[2],
            )))
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
                        state.components[&component(77)].lifecycle == ComponentLifecycle::Quiesced
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
both_tokio_flavors!(multi_hop_drain, drain_multi_hop_backlog);

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
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .sink(
            drasi_lib::ComponentName::try_new("a").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c9")
                    .expect("fixture UUID"),
                "a",
                Arc::new(Calls::default()),
                received.clone(),
            )),
        )
        .sink(
            drasi_lib::ComponentName::try_new("b").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ca")
                    .expect("fixture UUID"),
                "b",
                Arc::new(Calls::default()),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        );
    for (name, id, fail, calls) in [
        ("a", component(457), false, clean_a.clone()),
        ("b", component(458), true, clean_b.clone()),
    ] {
        builder = builder
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &id,
                ),
                Box::new(retained_config(name)),
            )
            .declare_resource(ResourceSpecification {
                id: ResourceId::try_new(name).expect("id"),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Graph,
                binding: Arc::from(name),
            })
            .expect("declare")
            .provide_resource(
                ResourceId::try_new(name).expect("id"),
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
                    control.observed().components[&component(77)].lifecycle,
                    ComponentLifecycle::Quiesced
                );
                for id in [component(457), component(458)] {
                    assert_eq!(
                        control.observed().relationships[&edge(
                            &drasi_core_types::ComponentId::try_new(
                                "00000000-0000-0000-0000-00000000004d"
                            )
                            .expect("fixture UUID"),
                            &id
                        )]
                            .binding,
                        BindingState::Failed
                    );
                }
                input
                    .send(output(root(
                        &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                            .expect("fixture UUID"),
                        "source",
                        1,
                        &[1],
                    )))
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

enum ProducerConstruction {
    Finite,
    Channel(Mutex<Option<mpsc::Receiver<OutputEnvelope>>>),
}

struct ProducerRecipe {
    calls: Arc<Calls>,
    stream: StreamId,
    construction: ProducerConstruction,
}

impl ProducerRecipe {
    fn channel(stream: StreamId) -> (Self, mpsc::Sender<OutputEnvelope>) {
        let (sender, receiver) = mpsc::channel(16);
        (
            Self {
                calls: Arc::default(),
                stream,
                construction: ProducerConstruction::Channel(Mutex::new(Some(receiver))),
            },
            sender,
        )
    }
}

struct ProducerFactory {
    descriptor: FactoryDescriptor,
    recipes: BTreeMap<&'static str, ProducerRecipe>,
}

impl ProducerFactory {
    fn new(recipes: impl IntoIterator<Item = (&'static str, ProducerRecipe)>) -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test/recreating-source", "1")
                    .unwrap(),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        Arc::from("variant"),
                        ConfigurationField {
                            constraint: None,
                            value_type: ConfigurationType::String,
                            required: true,
                            secret: false,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::new(),
            },
            recipes: recipes.into_iter().collect(),
        }
    }

    fn specification(&self, variant: &str) -> ComponentSpecification {
        ComponentSpecification {
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &[],
                &["out"],
            ),
            role: ComponentRole::Source,
            completion: None,
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([(
                Arc::from("variant"),
                ConfigurationValue::Literal(variant.into()),
            )]),
            dependencies: BTreeMap::new(),
        }
    }

    fn desired(&self, variant: &str) -> DesiredComponent {
        let mut desired = source_definition(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            ComponentName::try_new("source").unwrap(),
            stream("source"),
        );
        desired
            .streams
            .insert(port("out"), self.recipes[variant].stream.clone());
        desired.construction = ComponentConstruction::Factory(self.specification(variant));
        desired
    }

    fn recipe(&self, specification: &ComponentSpecification) -> anyhow::Result<&ProducerRecipe> {
        let Some(ConfigurationValue::Literal(variant)) = specification.configuration.get("variant")
        else {
            anyhow::bail!("missing producer variant");
        };
        self.recipes
            .get(
                variant
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid variant"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("unknown producer variant"))
    }
}

#[async_trait]
impl ComponentFactory for ProducerFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        self.recipe(specification)?;
        Ok(())
    }
    fn plan_update(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        anyhow::ensure!(
            current.descriptor == proposed.descriptor
                && current.implementation == proposed.implementation,
            "producer interface changed"
        );
        self.recipe(current)?;
        self.recipe(proposed)?;
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        let recipe = self
            .recipe(&context.specification)
            .map_err(ComponentCreationError::terminal)?;
        let source: Box<dyn EnvelopeSource> = match &recipe.construction {
            ProducerConstruction::Finite => Box::new(FiniteSource {
                descriptor: context.specification.descriptor.clone(),
                calls: recipe.calls.clone(),
                sequence: 0,
                emit: false,
            }),
            ProducerConstruction::Channel(receiver) => Box::new(Source {
                descriptor: context.specification.descriptor.clone(),
                calls: recipe.calls.clone(),
                events: receiver.lock().unwrap().take().ok_or_else(|| {
                    ComponentCreationError::terminal(anyhow::anyhow!(
                        "fixture producer input has already transferred"
                    ))
                })?,
            }),
        };
        recipe.calls.creates.fetch_add(1, Ordering::SeqCst);
        Ok(ConstructedComponent::source(source))
    }
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
        Ok(Some(output(root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            "source",
            self.sequence,
            &[1],
        ))))
    }
}

#[tokio::test]
async fn exhausted_source_restart_and_replacement_resume_its_consumer_and_closed_pipes() {
    let source_calls = Arc::new(Calls::default());
    let sink_calls = Arc::new(Calls::default());
    let (received, mut output_rx) = mpsc::channel(8);
    let (live, new_input) = ProducerRecipe::channel(stream("source"));
    let factory = Arc::new(ProducerFactory::new([
        (
            "finite",
            ProducerRecipe {
                calls: source_calls.clone(),
                stream: stream("source"),
                construction: ProducerConstruction::Finite,
            },
        ),
        ("live", live),
    ]));
    let mut graph = ComputationGraph::builder("restart-exhausted")
        .component(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            factory.specification("finite"),
            factory.clone(),
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                sink_calls.clone(),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
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
                    GraphSelection::Exact(vec![component(77)])
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
        let desired = factory.desired("live");
        let preview = control
            .preview(revision, vec![DesiredMutation::ReplaceComponent(desired)])
            .await
            .expect("replacement");
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("replace exhausted source");
        assert_eq!(report.summary, OperationSummary::Completed);
        new_input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                4,
                &[4],
            )))
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
    let (original, input) = ProducerRecipe::channel(stream("source"));
    let (replacement, new_input) = ProducerRecipe::channel(stream("fresh"));
    let factory = Arc::new(ProducerFactory::new([
        ("original", original),
        ("fresh", replacement),
    ]));
    let (received, mut output_rx) = mpsc::channel(8);
    let mut graph = ComputationGraph::builder("new-stream")
        .component(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            factory.specification("original"),
            factory.clone(),
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                Arc::new(Calls::default()),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                10,
                &[1],
            )))
            .await
            .expect("old stream");
        assert_eq!(output_rx.recv().await, Some(("sink".into(), 10)));
        let desired = factory.desired("fresh");
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::ReplaceComponent(desired)],
            )
            .await
            .expect("new stream preview");
        assert_eq!(
            control
                .reconcile(preview, TopologyBindings::default())
                .await
                .expect("replace")
                .summary,
            OperationSummary::Completed
        );
        new_input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000426")
                    .expect("fixture UUID"),
                "fresh",
                1,
                &[1],
            )))
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
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                Arc::new(Calls::default()),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.start().expect("scope");
    let control = run.control();
    let mut phase = "startup";
    let joined = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run, async {
            control.startup_report().await.expect("startup");
            phase = "rebinding";
            let mut revision = GraphRevision(1);
            for _ in 0..3 {
                let preview = control
                    .preview(
                        revision,
                        vec![DesiredMutation::Bind(relationship(
                            &drasi_core_types::ComponentId::try_new(
                                "00000000-0000-0000-0000-000000000065",
                            )
                            .expect("fixture UUID"),
                        ))],
                    )
                    .await
                    .expect("preview");
                let report = control
                    .reconcile(preview, TopologyBindings::default())
                    .await
                    .expect("rebind");
                assert_eq!(report.summary, OperationSummary::Completed);
                revision = report.revision;
            }
            phase = "finite drain";
            drop(input);
        })
    })
    .await;
    let (result, ()) = joined.unwrap_or_else(|_| {
        panic!(
            "binding generation fixture stalled during {phase}: {:?}",
            control.observed()
        )
    });
    result.expect("finite completion");
    let previous = graph.observed().relationships[&edge(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
    )]
        .generation;
    let run = graph.start().expect("next run");
    let control = run.control();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap_or_else(|_| panic!("second finite run stalled: {:?}", control.observed()))
        .expect("completion");
    assert!(
        graph.observed().relationships[&edge(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID")
        )]
            .generation
            > previous
    );
}

/// A committed reconciliation that cannot finish within its deadline must still
/// hand back its report: the topology changed, so the caller has to see an
/// applied-but-incomplete outcome rather than an error it would read as "nothing
/// happened".
fn assert_timed_out_after_commit(report: &ReconciliationReport) {
    assert!(report.committed, "{report:?}");
    assert_eq!(
        report.summary,
        OperationSummary::CompletedWithFailures,
        "{report:?}"
    );
    assert!(report.startup.is_none(), "{report:?}");
    assert!(
        matches!(
            report.failures.as_slice(),
            [failure] if failure.phase == FailurePhase::Activation
                && matches!(failure.cause.as_ref(), GraphError::ReconciliationTimeout)
        ),
        "{report:?}"
    );
}

async fn startup_timeout_after_commit_returns_the_committed_report() {
    let new = Arc::new(Calls::default());
    let (source, input) = source(Arc::new(Calls::default()));
    let (received, mut output_rx) = mpsc::channel(16);
    let start = Arc::new(Gate::default());
    let mut replacement = SinkVariant::new(new.clone());
    replacement.start_gate = Some(start.clone());
    let factory = Arc::new(Factory::with_variants(
        component(101),
        ComponentName::try_new("sink").unwrap(),
        "1",
        received,
        [("old", SinkVariant::new(Arc::default())), ("new", replacement)],
    ));
    let mut graph = ComputationGraph::builder("committed-startup")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .component(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            factory.specification("old"),
            factory.clone(),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .cleanup_timeout(Duration::from_millis(200))
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let preview = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::ReplaceComponent(factory.desired("new"))],
            )
            .await
            .expect("preview");
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("a committed reconciliation returns its report");
        assert_timed_out_after_commit(&report);
        assert_eq!(report.revision, control.desired_snapshot().revision);

        // The start that overran the deadline is still owned by the controller
        // and completes once the component is released.
        start.release.notify_one();
        control
            .subscribe_observed()
            .wait_for(|state| {
                state.components[&component(101)].lifecycle == ComponentLifecycle::Running
            })
            .await
            .expect("replacement starts");
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
            .await
            .expect("input");
        assert_eq!(output_rx.recv().await, Some(("new".into(), 1)));
        assert_eq!(new.starts.load(Ordering::SeqCst), 1);
        control.cancel();
    });
    cancelled(result);
}
both_tokio_flavors!(
    committed_startup_timeout,
    startup_timeout_after_commit_returns_the_committed_report
);

/// Constructs the `late` sink only once the test releases `gate`.
struct GatedFactory {
    descriptor: FactoryDescriptor,
    gate: Arc<Gate>,
    calls: Arc<Calls>,
    received: mpsc::Sender<(String, u64)>,
}
impl GatedFactory {
    fn specification(&self) -> ComponentSpecification {
        ComponentSpecification {
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c2")
                    .expect("fixture UUID"),
                &["in"],
                &[],
            ),
            role: ComponentRole::Sink,
            completion: Some(SinkCompletion::Handled),
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        }
    }
}
#[async_trait]
impl ComponentFactory for GatedFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        _: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        self.gate.entered.notify_one();
        self.gate.release.notified().await;
        Ok(ConstructedComponent::sink(Box::new(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c2")
                .expect("fixture UUID"),
            "late",
            self.calls.clone(),
            self.received.clone(),
        ))))
    }
}

/// A service whose start does not return until the test releases `gate`.
struct StallingService {
    descriptor: ComponentDescriptor,
    gate: Arc<Gate>,
}
#[async_trait]
impl ComputationComponent for StallingService {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.gate.entered.notify_one();
        self.gate.release.notified().await;
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl ComputationService for StallingService {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

async fn committed_reconciliation_ignores_disjoint_pending_start() {
    let late = Arc::new(Calls::default());
    let (source, input) = source(Arc::new(Calls::default()));
    let (received, mut output_rx) = mpsc::channel(16);
    let construction = Arc::new(Gate::default());
    let factory = Arc::new(GatedFactory {
        descriptor: FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("gated-sink", "1")
                .expect("implementation"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: ConfigurationSchema::default(),
            dependencies: BTreeMap::new(),
        },
        gate: construction.clone(),
        calls: late.clone(),
        received: received.clone(),
    });
    let mut graph = ComputationGraph::builder("committed-settle")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                Arc::new(Calls::default()),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .cleanup_timeout(Duration::from_millis(200))
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        activate(&control).await;
        let mut added = definition(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c2")
                .expect("fixture UUID"),
            ComponentName::try_new("late").unwrap(),
        );
        added.construction = ComponentConstruction::Factory(factory.specification());
        let preview = control
            .preview(
                GraphRevision(1),
                vec![
                    DesiredMutation::PutComponent(added),
                    DesiredMutation::Bind(relationship(
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-0000000001c2",
                        )
                        .expect("fixture UUID"),
                    )),
                ],
            )
            .await
            .expect("preview");
        assert!(preview.paused().contains(&component(77)));

        // While `late` is being constructed, a concurrently added service
        // begins a start which remains pending throughout this reconciliation.
        let mut bindings = TopologyBindings::default();
        bindings
            .factories
            .register(factory.clone())
            .expect("factory");
        let start = Arc::new(Gate::default());
        let (report, stalled) = tokio::join!(control.reconcile(preview, bindings), async {
            construction.entered.notified().await;
            let stalled = control
                .add_component(ComponentAddition::new(
                    drasi_lib::ComponentName::try_new("stalled").expect("fixture component name"),
                    ConstructedComponent::service(Box::new(StallingService {
                        descriptor: descriptor(
                            &drasi_core_types::ComponentId::try_new(
                                "00000000-0000-0000-0000-0000000001c1",
                            )
                            .expect("fixture UUID"),
                            &[],
                            &[],
                        ),
                        gate: start.clone(),
                    })),
                ))
                .await
                .expect("addition during construction");
            start.entered.notified().await;
            construction.release.notify_one();
            stalled
        });
        let report = report.expect("a committed reconciliation returns its report");
        assert!(report.committed);
        assert_eq!(report.summary, OperationSummary::Completed);
        assert!(report.failures.is_empty());
        assert!(report.startup.is_some());
        assert_eq!(
            control.observed().components[&component(77)].lifecycle,
            ComponentLifecycle::Running,
            "the producer paused for the commit must be resumed"
        );
        input
            .send(output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            )))
            .await
            .expect("input");
        let mut actual = vec![output_rx.recv().await.unwrap(), output_rx.recv().await.unwrap()];
        actual.sort();
        assert_eq!(actual, [("late".into(), 1), ("sink".into(), 1)]);

        // Only the unrelated service is still starting. Both committed
        // consumers have already received their exact input once.
        assert_eq!(
            control.observed().components[&component(450)].lifecycle,
            ComponentLifecycle::Running
        );
        assert_eq!(
            control.observed().components[&component(449)].lifecycle,
            ComponentLifecycle::Starting
        );
        start.release.notify_one();
        stalled
            .wait_started()
            .await
            .expect("stalled service starts");
        assert!(output_rx.try_recv().is_err());
        assert_eq!(late.starts.load(Ordering::SeqCst), 1);
        control.cancel();
    });
    cancelled(result);
}
both_tokio_flavors!(
    disjoint_start_during_committed_reconciliation,
    committed_reconciliation_ignores_disjoint_pending_start
);

fn declared_input(producer: &ComponentId, namespace: &str) -> InputBinding {
    InputBinding {
        input_port: port("in"),
        output: OutputEndpoint::new(producer.clone(), port("out")),
        stream: stream(namespace),
    }
}

fn source_definition(id: &ComponentId, name: ComponentName, output: StreamId) -> DesiredComponent {
    DesiredComponent {
        name,
        descriptor: descriptor(id, &[], &["out"]),
        role: ComponentRole::Source,
        completion: None,
        streams: BTreeMap::from([(port("out"), output)]),
        lifecycle: LifecyclePolicy::default(),
        input_merge: InputMergePolicy::Arrival,
        construction: ComponentConstruction::External {
            binding: BindingToken::generate(),
        },
    }
}

#[tokio::test]
async fn supplied_component_receives_declared_bindings_only_when_opted_in() {
    for opted_in in [true, false] {
        let calls = Arc::new(Calls::default());
        calls.binds_inputs.store(opted_in, Ordering::SeqCst);
        let (source, input) = source(Arc::new(Calls::default()));
        let (received, mut output_rx) = mpsc::channel(8);
        let mut graph = ComputationGraph::builder("supplied-bindings")
            .source(
                drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
                source,
            )
            .sink(
                drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
                Box::new(sink(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                    "sink",
                    calls.clone(),
                    received,
                )),
            )
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            )
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()
            .expect("graph");
        let run = graph.run().expect("scope");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            activate(&control).await;
            input
                .send(output(root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "source",
                    1,
                    &[1],
                )))
                .await
                .expect("input");
            assert_eq!(output_rx.recv().await, Some(("sink".into(), 1)));
            let bound = calls.bound.lock().expect("bound").clone();
            if opted_in {
                assert!(!bound.is_empty());
                assert!(bound
                    .iter()
                    .all(|bindings| bindings == &[declared_input(&component(77), "source")]));
            } else {
                assert!(bound.is_empty());
            }
            control.cancel();
        });
        cancelled(result);
    }
}

#[tokio::test]
async fn failed_input_binding_fails_the_operation_instead_of_being_ignored() {
    let calls = Arc::new(Calls::default());
    calls.binds_inputs.store(true, Ordering::SeqCst);
    calls.fail_bind_inputs.store(true, Ordering::SeqCst);
    let (source, _input) = source(Arc::new(Calls::default()));
    let (received, _output_rx) = mpsc::channel(8);
    let mut graph = ComputationGraph::builder("failed-bindings")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .sink(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            Box::new(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                calls.clone(),
                received,
            )),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        let started = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await;
        assert!(started
            .map(|report| report.summary != OperationSummary::Completed)
            .unwrap_or(true));
        assert!(calls.bound.lock().expect("bound").is_empty());
        assert_eq!(calls.starts.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    cancelled(result);
}

#[tokio::test]
async fn binding_factory_gets_declared_inputs_and_is_replaced_when_they_change() {
    for binds in [true, false] {
        let calls = Arc::new(Calls::default());
        let (received, _output_rx) = mpsc::channel(8);
        let factory = Factory::new(calls.clone(), received);
        let factory = Arc::new(if binds { factory.binding() } else { factory });
        let (source, _input) = source(Arc::new(Calls::default()));
        let mut graph = ComputationGraph::builder("factory-bindings")
            .source(
                drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
                source,
            )
            .component(
                drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
                factory.specification("sink"),
                factory.clone(),
            )
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            )
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()
            .expect("graph");
        let run = graph.run().expect("scope");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            activate(&control).await;
            assert_eq!(
                *calls.bound.lock().expect("bound"),
                vec![if binds {
                    vec![declared_input(&component(77), "source")]
                } else {
                    Vec::new()
                }]
            );
            let preview = control
                .preview(
                    GraphRevision(1),
                    vec![
                        DesiredMutation::PutComponent(source_definition(
                            &drasi_core_types::ComponentId::try_new(
                                "00000000-0000-0000-0000-00000000029a",
                            )
                            .expect("fixture UUID"),
                            ComponentName::try_new("other").unwrap(),
                            stream("other"),
                        )),
                        DesiredMutation::Bind(DesiredRelationship {
                            definition: edge(
                                &drasi_core_types::ComponentId::try_new(
                                    "00000000-0000-0000-0000-00000000029a",
                                )
                                .expect("fixture UUID"),
                                &drasi_core_types::ComponentId::try_new(
                                    "00000000-0000-0000-0000-000000000065",
                                )
                                .expect("fixture UUID"),
                            ),
                            policy: RelationshipPolicy::default(),
                            pipe: DesiredPipe::Bounded { capacity: 1 },
                        }),
                    ],
                )
                .await
                .expect("a second incoming edge is a valid preview");
            assert_eq!(preview.replaced().contains(&component(101)), binds);
            assert!(preview.paused().contains(&component(101)));
            control.cancel();
        });
        cancelled(result);
    }
}

async fn binding_factory_waits_for_its_input_declaration() {
    let calls = Arc::new(Calls::default());
    let (received, _output_rx) = mpsc::channel(8);
    let factory = Arc::new(Factory::new(calls.clone(), received).binding());
    let (source, _input) = source(Arc::new(Calls::default()));
    let mut graph = ComputationGraph::builder("pending-factory-input")
        .source(
            drasi_lib::ComponentName::try_new("source").expect("fixture component name"),
            source,
        )
        .component(
            drasi_lib::ComponentName::try_new("sink").expect("fixture component name"),
            factory.specification("sink"),
            factory,
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .build()
        .expect("unconnected declarations");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control
            .deployment_report()
            .await
            .expect("deployment observation");
        let observed = control.observed();
        let sink = &observed.components[&component(101)];
        assert_eq!(calls.creates.load(Ordering::SeqCst), 0);
        assert!(sink.failure.is_none());
        assert!(sink.waiting_for.contains(&DependencyWait::Connection {
            endpoint: endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "in"
            ),
        }));
        control
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
                RelationshipPolicy::default(),
            )
            .await
            .expect("declare the input")
            .ensure_committed()
            .expect("desired topology committed");
        let report = control
            .start_components(control.desired_snapshot().revision, GraphSelection::All)
            .await
            .expect("activate the connected graph");
        assert_eq!(report.summary, OperationSummary::Completed);
        assert_eq!(calls.creates.load(Ordering::SeqCst), 1);
        assert_eq!(
            *calls.bound.lock().unwrap(),
            vec![vec![declared_input(&component(77), "source")]]
        );
        control.cancel();
    });
    cancelled(result);
}

both_tokio_flavors!(
    binding_factory_missing_input,
    binding_factory_waits_for_its_input_declaration
);
