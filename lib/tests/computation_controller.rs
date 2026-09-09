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

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;
use tokio::sync::mpsc;

#[derive(Default)]
struct Calls {
    start: AtomicUsize,
    stop: AtomicUsize,
    failures: AtomicUsize,
}

struct ControlledSource {
    descriptor: ComponentDescriptor,
    calls: Arc<Calls>,
    events: VecDeque<OutputEnvelope>,
}

#[async_trait]
impl ComputationComponent for ControlledSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.calls.start.fetch_add(1, Ordering::SeqCst);
        if self
            .calls
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            anyhow::bail!("upstream is unavailable");
        }
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.stop.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for ControlledSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        if let Some(event) = self.events.pop_front() {
            Ok(Some(event))
        } else {
            std::future::pending().await
        }
    }
}

struct ControlledSink {
    descriptor: ComponentDescriptor,
    calls: Arc<Calls>,
    received: mpsc::Sender<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for ControlledSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.calls.start.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.stop.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for ControlledSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, mut input: InputEnvelope) -> anyhow::Result<()> {
        input
            .envelope
            .append_annotation(annotation(self.descriptor.id().as_str()))?;
        self.received
            .send(input.envelope)
            .await
            .map_err(|_| anyhow::anyhow!("test receiver closed"))
    }
}

fn source(id: &str, calls: Arc<Calls>, events: Vec<OutputEnvelope>) -> Box<dyn EnvelopeSource> {
    Box::new(ControlledSource {
        descriptor: descriptor(id, &[], &["out"]),
        calls,
        events: events.into(),
    })
}

fn sink(
    id: &str,
    calls: Arc<Calls>,
    received: mpsc::Sender<ChangeEnvelope>,
) -> Box<dyn EnvelopeSink> {
    Box::new(ControlledSink {
        descriptor: descriptor(id, &["in"], &[]),
        calls,
        received,
    })
}

macro_rules! both_runtimes {
    ($name:ident, $scenario:ident) => {
        mod $name {
            #[tokio::test(flavor = "current_thread")]
            async fn current_thread() {
                tokio::time::timeout(super::Duration::from_secs(10), super::$scenario())
                    .await
                    .expect("controller deadlocked");
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn multi_thread() {
                tokio::time::timeout(super::Duration::from_secs(10), super::$scenario())
                    .await
                    .expect("controller deadlocked");
            }
        }
    };
}

async fn independent_activation() {
    let source_calls = Arc::new(Calls::default());
    source_calls.failures.store(1, Ordering::SeqCst);
    let sink_calls = Arc::new(Calls::default());
    let (received_tx, mut received) = mpsc::channel(4);
    let transform = NativeTransform::new("transform", |input| {
        Ok(vec![output(derived(
            &input.envelope,
            "transform",
            1,
            input.envelope.changes().clone(),
        ))])
    });
    let mut graph = ComputationGraph::builder("independent-activation")
        .source(source(
            "source",
            source_calls.clone(),
            vec![output(root("source", 1, &[7]))],
        ))
        .transformer(Box::new(transform))
        .sink(sink("sink", sink_calls.clone(), received_tx))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("transform", "out"), stream("transform"))
        .connect(
            edge("source", "transform"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("transform", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("valid graph");
    let run = graph.start().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let report = control.startup_report().await.expect("startup report");
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        assert!(matches!(
            report.components[&component("source")],
            StartOutcome::StartFailed(_)
        ));
        assert!(matches!(
            report.components[&component("transform")],
            StartOutcome::Started
        ));
        assert!(matches!(
            report.components[&component("sink")],
            StartOutcome::Started
        ));
        let observed = control.observed();
        assert_eq!(
            observed.components[&component("source")].realization,
            RealizationState::Created
        );
        assert_eq!(
            observed.components[&component("transform")].lifecycle,
            ComponentLifecycle::Running
        );
        assert_eq!(
            observed.components[&component("sink")].lifecycle,
            ComponentLifecycle::Running
        );
        assert_eq!(
            observed.relationships[&edge("source", "transform")].binding,
            BindingState::Bound
        );
        assert_eq!(
            observed.relationships[&edge("source", "transform")].availability,
            DataAvailability::Unavailable
        );
        assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 0);
        assert!(received.try_recv().is_err(), "failure is not data or EOF");

        let stopped = control
            .stop_components(
                GraphRevision(1),
                GraphSelection::Exact(vec![component("source")]),
            )
            .await
            .expect("stop failed source before explicit retry");
        assert!(matches!(
            stopped.components[&component("source")],
            StopOutcome::Stopped
        ));
        let restarted = control
            .start_components(
                GraphRevision(1),
                GraphSelection::Exact(vec![component("source")]),
            )
            .await
            .expect("explicit retry");
        assert!(matches!(
            restarted.components[&component("source")],
            StartOutcome::Started
        ));
        let result = received
            .recv()
            .await
            .expect("existing consumers receive after source recovers");
        assert_eq!(values(&result), [7]);
        assert_eq!(
            sink_calls.start.load(Ordering::SeqCst),
            1,
            "downstream was not restarted"
        );
        assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 0);
        assert_eq!(source_calls.start.load(Ordering::SeqCst), 2);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(source_calls.stop.load(Ordering::SeqCst), 2);
    assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 1);
}

both_runtimes!(source_failure_is_not_eof, independent_activation);

async fn revision_and_generation() {
    let source_calls = Arc::new(Calls::default());
    let sink_calls = Arc::new(Calls::default());
    let (received, _receiver) = mpsc::channel(4);
    let mut graph = ComputationGraph::builder("revision-and-generation")
        .source(source("source", source_calls.clone(), vec![]))
        .sink(sink("sink", sink_calls.clone(), received))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("caller-driven controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let deployment = control
            .deployment_report()
            .await
            .expect("create without start");
        assert_eq!(deployment.summary, OperationSummary::Completed);
        assert_eq!(source_calls.start.load(Ordering::SeqCst), 0);
        assert_eq!(sink_calls.start.load(Ordering::SeqCst), 0);
        let revision = control
            .set_lifecycle_policy(
                GraphRevision(1),
                component("sink"),
                LifecyclePolicy { auto_start: false },
            )
            .await
            .expect("CAS policy edit");
        assert_eq!(revision, GraphRevision(2));
        assert!(matches!(
            control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await,
            Err(GraphError::StaleRevision { .. })
        ));
        assert_eq!(source_calls.start.load(Ordering::SeqCst), 0);
        let report = control
            .start_components(revision, GraphSelection::All)
            .await
            .expect("start at current revision");
        assert!(matches!(
            report.components[&component("source")],
            StartOutcome::Started
        ));
        assert!(matches!(
            report.components[&component("sink")],
            StartOutcome::NotRequested
        ));
        let observed = control.observed();
        let node = observed.components[&component("source")].clone();
        assert!(matches!(
            control
                .report_health(
                    revision,
                    HealthObservation {
                        component: component("source"),
                        generation: ComponentGeneration(node.generation.0 + 1),
                        operation: node.operation,
                        health: ComponentHealth::Unavailable,
                    }
                )
                .await,
            Err(GraphError::StaleGeneration)
        ));
        control
            .report_health(
                revision,
                HealthObservation {
                    component: component("source"),
                    generation: node.generation,
                    operation: node.operation,
                    health: ComponentHealth::Degraded,
                },
            )
            .await
            .expect("generation-bound health");
        assert_eq!(
            control.observed().components[&component("source")].health,
            ComponentHealth::Degraded
        );
        let stopped = control
            .stop_components(revision, GraphSelection::All)
            .await
            .expect("stop selection");
        assert!(matches!(
            stopped.components[&component("sink")],
            StopOutcome::AlreadyStopped
        ));
        assert!(matches!(
            control
                .report_health(
                    revision,
                    HealthObservation {
                        component: component("source"),
                        generation: node.generation,
                        operation: node.operation,
                        health: ComponentHealth::Healthy,
                    }
                )
                .await,
            Err(GraphError::StaleGeneration)
        ));
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(source_calls.stop.load(Ordering::SeqCst), 1);
    assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 0);
    assert!(matches!(
        control
            .start_components(GraphRevision(2), GraphSelection::All)
            .await,
        Err(GraphError::ControllerClosed)
    ));
}

both_runtimes!(live_cas_and_health, revision_and_generation);

struct FailingPipe;

impl PipeProvider for FailingPipe {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        BoundedPipeConfig { capacity: 1 }.capabilities()
    }

    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        Err(PipeError::Backend(anyhow::anyhow!(
            "injected pipe construction failure"
        )))
    }
}

#[tokio::test]
async fn independent_branch_survives_partial_binding_creation_failure() {
    let failed_source = Arc::new(Calls::default());
    let failed_sink = Arc::new(Calls::default());
    let good_source = Arc::new(Calls::default());
    let good_sink = Arc::new(Calls::default());
    let (received_tx, mut received_rx) = mpsc::channel(4);
    let mut graph = ComputationGraph::builder("partial-binding")
        .source(source("bad-source", failed_source.clone(), vec![]))
        .sink(sink("bad-sink", failed_sink.clone(), received_tx.clone()))
        .source(source(
            "good-source",
            good_source.clone(),
            vec![output(root("good-source", 1, &[9]))],
        ))
        .sink(sink("good-sink", good_sink.clone(), received_tx))
        .bind_stream(endpoint("bad-source", "out"), stream("bad-source"))
        .bind_stream(endpoint("good-source", "out"), stream("good-source"))
        .connect(edge("bad-source", "bad-sink"), Box::new(FailingPipe))
        .connect(
            edge("good-source", "good-sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("all declarations valid");
    let run = graph.start().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let deployment = control
            .deployment_report()
            .await
            .expect("per-item creation results");
        assert_eq!(deployment.components.len(), 4);
        assert!(matches!(
            deployment.components[&component("bad-source")],
            CreationOutcome::Blocked { .. }
        ));
        assert!(matches!(
            deployment.components[&component("good-source")],
            CreationOutcome::Created
        ));
        let startup = control
            .startup_report()
            .await
            .expect("all eligible starts attempted");
        assert!(matches!(
            startup.components[&component("bad-source")],
            StartOutcome::NotCreated
        ));
        assert!(matches!(
            startup.components[&component("good-sink")],
            StartOutcome::Started
        ));
        assert_eq!(
            values(&received_rx.recv().await.expect("independent branch event")),
            [9]
        );
        assert_eq!(
            control.desired_snapshot().nodes.len(),
            4,
            "failed desired items remain"
        );
        assert_eq!(failed_source.start.load(Ordering::SeqCst), 0);
        assert_eq!(failed_sink.start.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(good_sink.stop.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_activation_coupling_blocks_only_the_declared_dependency() {
    let source_calls = Arc::new(Calls::default());
    source_calls.failures.store(1, Ordering::SeqCst);
    let sink_calls = Arc::new(Calls::default());
    let (received, _receiver) = mpsc::channel(1);
    let connection = edge("source", "sink");
    let mut graph = ComputationGraph::builder("explicit-coupling")
        .source(source("source", source_calls, vec![]))
        .sink(sink("sink", sink_calls.clone(), received))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            connection.clone(),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .relationship_policy(
            connection,
            RelationshipPolicy {
                activation: ActivationCoupling::RequiresRunning,
                ..Default::default()
            },
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        let report = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .expect("partial report");
        assert!(matches!(
            report.components[&component("sink")],
            StartOutcome::Blocked { .. }
        ));
        assert_eq!(sink_calls.start.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

struct PendingStartSource {
    descriptor: ComponentDescriptor,
    entered: Arc<tokio::sync::Notify>,
    stops: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputationComponent for PendingStartSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.entered.notify_one();
        std::future::pending().await
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for PendingStartSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        anyhow::bail!("processing cannot begin before a successful start")
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_live_stop_command_cancels_pending_start_without_waiting_for_its_component_lock() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let source_stops = Arc::new(AtomicUsize::new(0));
    let sink_calls = Arc::new(Calls::default());
    let (received, _receiver) = mpsc::channel(1);
    let mut graph = ComputationGraph::builder("interrupt-start")
        .source(Box::new(PendingStartSource {
            descriptor: descriptor("source", &[], &["out"]),
            entered: entered.clone(),
            stops: source_stops.clone(),
        }))
        .sink(sink("sink", sink_calls.clone(), received))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        let (started, stopped) = tokio::join!(
            control.start_components(GraphRevision(1), GraphSelection::All),
            async {
                entered.notified().await;
                control
                    .stop_components(
                        GraphRevision(1),
                        GraphSelection::Exact(vec![component("source")]),
                    )
                    .await
            }
        );
        assert!(matches!(
            started.expect("complete all start outcomes").components[&component("source")],
            StartOutcome::StartFailed(_)
        ));
        assert!(matches!(
            stopped.expect("stop interrupted source").components[&component("source")],
            StopOutcome::Stopped
        ));
        assert_eq!(source_stops.load(Ordering::SeqCst), 1);
        assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 0);
        assert_eq!(
            control.observed().components[&component("sink")].lifecycle,
            ComponentLifecycle::Running
        );
        assert_eq!(
            control.observed().relationships[&edge("source", "sink")].binding,
            BindingState::Bound
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(source_stops.load(Ordering::SeqCst), 1);
    assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_failure_propagation_stops_only_its_dependent_closure() {
    let failed = Arc::new(Calls::default());
    failed.failures.store(1, Ordering::SeqCst);
    let dependent = Arc::new(Calls::default());
    let independent = Arc::new(Calls::default());
    let (received, mut receiver) = mpsc::channel(4);
    let mut graph = ComputationGraph::builder("propagation")
        .source(source("failed", failed, vec![]))
        .sink(sink("dependent", dependent.clone(), received.clone()))
        .source(source(
            "independent",
            Arc::new(Calls::default()),
            vec![output(root("independent", 1, &[11]))],
        ))
        .sink(sink("independent-sink", independent.clone(), received))
        .bind_stream(endpoint("failed", "out"), stream("failed"))
        .bind_stream(endpoint("independent", "out"), stream("independent"))
        .connect(
            edge("failed", "dependent"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("independent", "independent-sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .relationship_policy(
            edge("failed", "dependent"),
            RelationshipPolicy {
                propagate_failure: true,
                ..Default::default()
            },
        )
        .build()
        .expect("graph");
    let run = graph.start().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.startup_report().await.expect("all start outcomes");
        let mut observed = control.subscribe_observed();
        observed
            .wait_for(|state| {
                state.components[&component("dependent")].lifecycle == ComponentLifecycle::Failed
            })
            .await
            .expect("dependent cleanup completed");
        assert_eq!(dependent.stop.load(Ordering::SeqCst), 1);
        assert_eq!(independent.stop.load(Ordering::SeqCst), 0);
        assert_eq!(
            values(
                &receiver
                    .recv()
                    .await
                    .expect("independent branch still runs")
            ),
            [11]
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(
        dependent.stop.load(Ordering::SeqCst),
        1,
        "no duplicate stop after propagation"
    );
}

#[tokio::test]
async fn explicit_creation_dependency_blocks_when_its_required_instance_is_unrealized() {
    let (received, _receiver) = mpsc::channel(1);
    let mut graph = ComputationGraph::builder("creation-dependency")
        .source(source("source", Arc::new(Calls::default()), vec![]))
        .sink(sink(
            "failed-binding",
            Arc::new(Calls::default()),
            received.clone(),
        ))
        .sink(sink("dependent", Arc::new(Calls::default()), received))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(edge("source", "failed-binding"), Box::new(FailingPipe))
        .connect(
            edge("source", "dependent"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .relationship_policy(
            edge("source", "dependent"),
            RelationshipPolicy {
                required_for_creation: true,
                ..Default::default()
            },
        )
        .build()
        .expect("valid declarations");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let deployment = control.deployment_report().await.expect("deployment");
        assert!(matches!(
            deployment.components[&component("dependent")],
            CreationOutcome::Blocked { .. }
        ));
        assert_eq!(
            control.observed().relationships[&edge("source", "dependent")].binding,
            BindingState::Bound
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn failed_optional_binding_never_becomes_available_idle_or_exhausted() {
    for finite in [false, true] {
        let (received, _receiver) = mpsc::channel(2);
        let source: Box<dyn EnvelopeSource> = if finite {
            Box::new(FiniteSource::new(
                "source",
                vec![output(root("source", 1, &[1]))],
            ))
        } else {
            source("source", Arc::new(Calls::default()), vec![])
        };
        let mut graph = ComputationGraph::builder("optional-binding")
            .source(source)
            .sink(sink("good", Arc::new(Calls::default()), received.clone()))
            .sink(sink("bad", Arc::new(Calls::default()), received))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .connect(
                edge("source", "good"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(edge("source", "bad"), Box::new(FailingPipe))
            .relationship_policy(
                edge("source", "bad"),
                RelationshipPolicy {
                    required_for_binding: false,
                    ..Default::default()
                },
            )
            .build()
            .expect("valid graph");
        let run = graph.run().expect("scope");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control.deployment_report().await.expect("partial bindings");
            control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await
                .expect("independent startup");
            if finite {
                let mut observed = control.subscribe_observed();
                observed
                    .wait_for(|state| {
                        state.relationships[&edge("source", "good")].availability
                            == DataAvailability::Exhausted
                    })
                    .await
                    .expect("real finite EOF");
            }
            let node = control.observed().components[&component("source")].clone();
            control
                .report_health(
                    GraphRevision(1),
                    HealthObservation {
                        component: component("source"),
                        generation: node.generation,
                        operation: node.operation,
                        health: ComponentHealth::Healthy,
                    },
                )
                .await
                .expect("valid health observation");
            assert_eq!(
                control.observed().relationships[&edge("source", "bad")].binding,
                BindingState::Failed
            );
            assert_eq!(
                control.observed().relationships[&edge("source", "bad")].availability,
                DataAvailability::Unavailable
            );
            control
                .stop_components(
                    GraphRevision(1),
                    GraphSelection::Exact(vec![component("source")]),
                )
                .await
                .expect("stop source");
            assert_eq!(
                control.observed().relationships[&edge("source", "bad")].availability,
                DataAvailability::Unavailable
            );
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
    }
}
