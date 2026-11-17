// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The graph's single sink delivery-failure model: a transient failure is
//! retried in place with backoff and the node shows that it is retrying; a
//! permanent failure follows the sink's declared `PermanentFailurePolicy`.

#![cfg(test)]

#[path = "support/mod.rs"]
mod fixture_support;
use fixture_support::FixtureGraphBuilderExt as _;

mod computation_support;

use anyhow::Result;
use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch, Notify};

const SOURCE: &str = "00000000-0000-0000-0000-0000000005a1";
const SINK: &str = "00000000-0000-0000-0000-0000000005a2";

fn id(value: &str) -> drasi_core_types::ComponentId {
    drasi_core_types::ComponentId::try_new(value).expect("fixture UUID")
}

struct ChannelSource {
    descriptor: ComponentDescriptor,
    input: mpsc::Receiver<u64>,
}

#[async_trait]
impl ComputationComponent for ChannelSource {
    fn description(&self) -> ComponentDescription {
        ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for ChannelSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self
            .input
            .recv()
            .await
            .map(|sequence| output(root(&id(SOURCE), "source", sequence, &[sequence as u16]))))
    }
}

/// What one attempt to deliver an input does.
#[derive(Clone, Copy)]
enum Step {
    Transient(Option<Duration>),
    Permanent,
    /// Wait for the test, then deliver.
    Hold,
}

#[derive(Default)]
struct Record {
    attempts: Vec<u64>,
    delivered: Vec<u64>,
}

struct ScriptedSink {
    descriptor: ComponentDescriptor,
    policy: PermanentFailurePolicy,
    script: BTreeMap<u64, VecDeque<Step>>,
    record: Arc<Mutex<Record>>,
    attempts: watch::Sender<usize>,
    gate: Arc<Notify>,
}

#[async_trait]
impl ComputationComponent for ScriptedSink {
    fn description(&self) -> ComponentDescription {
        ComponentDescription::undeclared().with_permanent_failure(self.policy)
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSink for ScriptedSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let sequence = input.envelope.system().sequence();
        self.record.lock().unwrap().attempts.push(sequence);
        self.attempts.send_modify(|count| *count += 1);
        match self.script.get_mut(&sequence).and_then(VecDeque::pop_front) {
            Some(Step::Transient(None)) => {
                return Err(
                    DeliveryFailure::transient(anyhow::anyhow!("destination outage")).into(),
                )
            }
            Some(Step::Transient(Some(delay))) => {
                return Err(DeliveryFailure::transient_after(
                    anyhow::anyhow!("destination throttled"),
                    delay,
                )
                .into())
            }
            Some(Step::Permanent) => {
                return Err(DeliveryFailure::permanent(anyhow::anyhow!("rejected content")).into())
            }
            Some(Step::Hold) => self.gate.notified().await,
            None => {}
        }
        self.record.lock().unwrap().delivered.push(sequence);
        Ok(())
    }
}

struct Fixture {
    input: mpsc::Sender<u64>,
    record: Arc<Mutex<Record>>,
    attempts: watch::Receiver<usize>,
    gate: Arc<Notify>,
}

fn fixture(
    policy: PermanentFailurePolicy,
    script: &[(u64, &[Step])],
) -> Result<(ComputationGraph, Fixture)> {
    let (input, receiver) = mpsc::channel(8);
    let record = Arc::new(Mutex::new(Record::default()));
    let (attempts, observed_attempts) = watch::channel(0);
    let gate = Arc::new(Notify::new());
    let graph = ComputationGraph::builder("sink-delivery-failures")
        .fixture_source(Box::new(ChannelSource {
            descriptor: descriptor(&id(SOURCE), &[], &["out"]),
            input: receiver,
        }))
        .fixture_sink(Box::new(ScriptedSink {
            descriptor: descriptor(&id(SINK), &["in"], &[]),
            policy,
            script: script
                .iter()
                .map(|(sequence, steps)| (*sequence, steps.iter().copied().collect()))
                .collect(),
            record: record.clone(),
            attempts,
            gate: gate.clone(),
        }))
        .bind_stream(endpoint(&id(SOURCE), "out"), stream("source"))
        .connect(
            edge(&id(SOURCE), &id(SINK)),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .build()?;
    Ok((
        graph,
        Fixture {
            input,
            record,
            attempts: observed_attempts,
            gate,
        },
    ))
}

/// Runs the started graph while `check` drives it, then cancels the run.
async fn running<F, Fut>((mut graph, fixture): (ComputationGraph, Fixture), check: F)
where
    F: FnOnce(GraphControl, Fixture) -> Fut,
    Fut: Future<Output = Fixture>,
{
    let run = graph.start().expect("start");
    let control = run.control();
    let (outcome, fixture) = tokio::join!(run, async {
        control.startup_report().await.expect("startup");
        let fixture =
            tokio::time::timeout(Duration::from_secs(600), check(control.clone(), fixture))
                .await
                .expect("check deadline");
        control.cancel();
        fixture
    });
    assert!(matches!(outcome, Err(GraphError::Cancelled)), "{outcome:?}");
    drop(fixture);
    graph.dispose().await.expect("cleanup");
}

async fn observe(
    control: &GraphControl,
    accept: impl Fn(&ObservedComponent) -> bool,
) -> ObservedComponent {
    let mut observed = control.subscribe_observed();
    let graph = observed
        .wait_for(|graph| graph.components.get(&id(SINK)).is_some_and(&accept))
        .await
        .expect("observed graph")
        .clone();
    graph.components[&id(SINK)].clone()
}

/// The failure's whole cause chain as text.
fn causes(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(error) = cause {
        text.push_str(": ");
        text.push_str(&error.to_string());
        cause = error.source();
    }
    text
}

async fn delivered(fixture: &Fixture, expected: &[u64]) {
    while fixture.record.lock().unwrap().delivered != expected {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn transient_failures_are_retried_in_place_and_the_node_shows_the_retry() -> Result<()> {
    let outage = Step::Transient(None);
    let fixture = fixture(
        PermanentFailurePolicy::Stall,
        &[(1, &[outage, outage, Step::Hold])],
    )?;
    running(fixture, |control, mut fixture| async move {
        fixture.input.send(1).await.unwrap();
        fixture
            .attempts
            .wait_for(|count| *count >= 3)
            .await
            .unwrap();
        // While retrying, the node keeps running and carries the last error.
        let retrying = observe(&control, |node| node.failure.is_some()).await;
        let failure = retrying.failure.as_ref().unwrap();
        assert_eq!(retrying.lifecycle, ComponentLifecycle::Running);
        assert_eq!(retrying.health, ComponentHealth::Degraded);
        assert_eq!(failure.phase, FailurePhase::Processing);
        assert_eq!(failure.disposition, FailureDisposition::Retryable);
        assert!(causes(failure.cause.as_ref()).contains("destination outage"));
        // No manual Retry: the held third attempt delivers the same input.
        fixture.gate.notify_one();
        fixture.input.send(2).await.unwrap();
        delivered(&fixture, &[1, 2]).await;
        let recovered = observe(&control, |node| node.failure.is_none()).await;
        assert_eq!(recovered.health, ComponentHealth::Healthy);
        assert_eq!(recovered.lifecycle, ComponentLifecycle::Running);
        assert_eq!(
            fixture.record.lock().unwrap().attempts,
            [1, 1, 1, 2],
            "the same input is retried, never skipped or duplicated"
        );
        fixture
    })
    .await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_permanent_failure_under_stall_records_a_terminal_failure() -> Result<()> {
    let fixture = fixture(PermanentFailurePolicy::Stall, &[(2, &[Step::Permanent])])?;
    running(fixture, |control, fixture| async move {
        for sequence in 1..=3 {
            fixture.input.send(sequence).await.unwrap();
        }
        let stalled = observe(&control, |node| node.failure.is_some()).await;
        let failure = stalled.failure.as_ref().unwrap();
        assert_eq!(failure.phase, FailurePhase::Processing);
        assert_eq!(failure.disposition, FailureDisposition::Terminal);
        assert!(causes(failure.cause.as_ref()).contains("rejected content"));
        tokio::time::sleep(Duration::from_secs(60)).await;
        let record = fixture.record.lock().unwrap();
        assert_eq!(record.delivered, [1], "nothing after the stalled input");
        assert_eq!(
            record.attempts,
            [1, 2],
            "a permanent failure is not retried"
        );
        drop(record);
        fixture
    })
    .await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_permanent_failure_under_skip_advances_and_is_counted() -> Result<()> {
    let fixture = fixture(PermanentFailurePolicy::Skip, &[(2, &[Step::Permanent])])?;
    running(fixture, |control, fixture| async move {
        for sequence in 1..=3 {
            fixture.input.send(sequence).await.unwrap();
        }
        delivered(&fixture, &[1, 3]).await;
        let node = observe(&control, |node| node.skipped_inputs == 1).await;
        assert!(node.failure.is_none(), "{node:?}");
        assert_eq!(node.lifecycle, ComponentLifecycle::Running);
        assert_eq!(fixture.record.lock().unwrap().attempts, [1, 2, 3]);
        fixture
    })
    .await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stop_during_retry_backoff_exits_without_waiting_for_it() -> Result<()> {
    let throttled = Step::Transient(Some(Duration::from_secs(300)));
    let fixture = fixture(PermanentFailurePolicy::Stall, &[(1, &[throttled; 4])])?;
    running(fixture, |control, mut fixture| async move {
        fixture.input.send(1).await.unwrap();
        fixture
            .attempts
            .wait_for(|count| *count >= 1)
            .await
            .unwrap();
        observe(&control, |node| node.failure.is_some()).await;
        let began = tokio::time::Instant::now();
        let report = control
            .stop_components(
                control.desired_snapshot().revision,
                GraphSelection::Exact(vec![id(SINK)]),
            )
            .await
            .expect("stop");
        assert!(
            began.elapsed() < Duration::from_secs(300),
            "stop waited {:?} for the retry backoff",
            began.elapsed()
        );
        assert!(
            matches!(report.components.get(&id(SINK)), Some(StopOutcome::Stopped)),
            "{report:?}"
        );
        let record = fixture.record.lock().unwrap();
        assert!(record.delivered.is_empty());
        assert_eq!(record.attempts, [1], "no attempt during or after the stop");
        drop(record);
        fixture
    })
    .await;
    Ok(())
}
