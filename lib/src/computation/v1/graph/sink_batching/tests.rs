// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;

#[derive(Default)]
struct Probe {
    attempts: AtomicUsize,
    release: tokio::sync::Notify,
}
struct Ack {
    probe: Arc<Probe>,
    fail: bool,
}
#[async_trait]
impl Acknowledgement for Ack {
    async fn complete(self: Box<Self>, _: HandlingOutcome) -> Result<(), PipeError> {
        self.probe.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(PipeError::Backend(anyhow::anyhow!(
                "acknowledgement failed"
            )));
        }
        self.probe.release.notified().await;
        Ok(())
    }
}
fn completed(fail: bool) -> (SinkBatchState, Arc<FlowProgress>, Arc<Probe>) {
    let progress = Arc::new(FlowProgress::default());
    let probe = Arc::new(Probe::default());
    let input = InputEnvelope {
        port: PortId::try_new("in").unwrap(),
        envelope: crate::computation::v1::GraphChangeCodec::encode_changes(
            &[],
            StreamId::try_new("source/out").unwrap(),
            1,
            None,
        )
        .unwrap(),
    };
    let mut state = SinkBatchState::default();
    state.inputs.push_back(RetainedInput {
        edge: 0,
        input,
        acknowledgement: Some(Box::new(Ack {
            probe: probe.clone(),
            fail,
        })),
        _work: DeliveryWork::new(progress.clone()).unwrap(),
        bytes: 100,
    });
    state.completed = 1;
    (state, progress, probe)
}

#[tokio::test]
async fn cancellation_retains_the_exact_acknowledgement_future_without_reexecuting_effects() {
    let (mut state, progress, probe) = completed(false);
    {
        let call = state.acknowledge();
        tokio::pin!(call);
        assert!(futures::poll!(&mut call).is_pending());
    }
    assert_eq!(probe.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(state.completed, 1);
    assert!(state.acknowledging.is_some());
    assert_eq!(progress.in_flight.load(Ordering::SeqCst), 1);
    probe.release.notify_one();
    state.acknowledge().await.unwrap();
    assert_eq!(
        probe.attempts.load(Ordering::SeqCst),
        1,
        "resume polls the owned future, not a second ACK"
    );
    assert!(state.inputs.is_empty());
    assert_eq!(progress.in_flight.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_acknowledgement_retains_input_and_fences_retry_without_skipping_progress() {
    let (mut state, progress, probe) = completed(true);
    assert!(state.acknowledge().await.is_err());
    assert!(state.acknowledge().await.is_err());
    assert_eq!(probe.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(state.inputs.len(), 1);
    assert_eq!(state.completed, 1);
    assert_eq!(progress.in_flight.load(Ordering::SeqCst), 1);
}

struct ClosedReceiver;
#[async_trait]
impl EnvelopeReceiver for ClosedReceiver {
    async fn receive(&mut self) -> Result<Option<Delivery>, PipeError> {
        Ok(None)
    }
}

fn incoming(edge: usize, seconds: Option<i64>) -> Incoming {
    let progress = Arc::new(FlowProgress::default());
    let envelope = crate::computation::v1::GraphChangeCodec::encode_changes(
        &[],
        StreamId::try_new(format!("source-{edge}/out")).unwrap(),
        1,
        seconds.map(|seconds| chrono::DateTime::from_timestamp(seconds, 0).unwrap()),
    )
    .unwrap();
    Incoming {
        edge,
        port: PortId::try_new("in").unwrap(),
        receiver: Box::new(ClosedReceiver),
        acknowledgement_required: false,
        exhausted: false,
        pending: Some(PendingInput {
            delivery: Delivery::new(envelope, None),
            work: DeliveryWork::new(progress.clone()).unwrap(),
        }),
        progress,
    }
}

#[tokio::test]
async fn timed_merge_preserves_untimed_arrival_barriers_across_batch_boundaries() {
    let mut inputs = [incoming(0, Some(10)), incoming(1, None), incoming(2, Some(30))];
    let mut ready = VecDeque::from([2, 1, 0]);
    for expected in [2, 1, 0] {
        let index = next_input(
            &mut inputs,
            InputMergePolicy::EventTimeAcrossStreams,
            &mut ready,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(inputs[index].edge, expected);
        assert_eq!(
            ready.front(),
            Some(&expected),
            "selection remains retained until the input is consumed"
        );
        inputs[index].pending.take();
        ready.retain(|edge| *edge != expected);
    }
    let mut inputs = [incoming(0, Some(30)), incoming(1, None), incoming(2, Some(10))];
    let mut ready = VecDeque::from([0, 2, 1]);
    assert_eq!(
        next_input(
            &mut inputs,
            InputMergePolicy::EventTimeAcrossStreams,
            &mut ready
        )
        .await
        .unwrap(),
        Some(2),
        "timed heads before an untimed barrier still sort by event time"
    );
}
