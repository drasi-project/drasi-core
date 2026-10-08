// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::computation::v1::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{oneshot, Notify};

fn schema() -> SchemaDescriptor {
    QueryChangeCodec::schema().descriptor().clone()
}

fn input(sequence: u64) -> InputEnvelope {
    let changes = ChangeSet::try_new(
        ChangeSetId::try_new("native-application", sequence.to_be_bytes().to_vec().into()).unwrap(),
        schema(),
        vec![],
    )
    .unwrap();
    InputEnvelope {
        port: PortId::try_new(INPUT_PORT).unwrap(),
        envelope: ChangeEnvelope::new(
            EnvelopeId::try_new("native-application", sequence.to_be_bytes().to_vec().into())
                .unwrap(),
            changes,
            SystemMetadata::new(StreamId::try_new("query").unwrap(), sequence)
                .with_source_position(sequence.to_be_bytes().to_vec().into()),
        )
        .append_context(
            ContextEntry::try_new(
                ComponentId::try_new("query").unwrap(),
                "request",
                ContextValue::Unsigned(sequence),
            )
            .unwrap(),
        )
        .unwrap(),
    }
}

#[test]
fn descriptor_and_completion_do_not_invent_recovery_or_configuration() {
    let (channel, _receiver) =
        ApplicationReaction::channel("channel", schema(), NonZeroUsize::new(1).unwrap()).unwrap();
    let callback =
        ApplicationReaction::callback("callback", schema(), |_| async { Ok(()) }).unwrap();
    assert_eq!(channel.completion(), SinkCompletion::Accepted);
    assert_eq!(callback.completion(), SinkCompletion::Handled);
    for reaction in [&channel, &callback] {
        assert_eq!(reaction.descriptor().ports().len(), 1);
        assert_eq!(reaction.descriptor().ports()[0].schema(), &schema());
        assert_eq!(
            reaction.descriptor().ports()[0].direction(),
            PortDirection::Input
        );
        assert_eq!(reaction.recovery_contract(), ComponentRecovery::default());
        assert!(reaction.configuration().is_err());
    }
    assert!(validate_sink_completion(
        channel.completion(),
        &PipeRequirements::new([PipeCapability::ExplicitAcknowledgement])
    )
    .is_err());
    assert!(ApplicationReaction::channel("", schema(), NonZeroUsize::new(1).unwrap()).is_err());
    let oversized = NonZeroUsize::new(tokio::sync::Semaphore::MAX_PERMITS + 1).unwrap();
    assert!(matches!(
        ApplicationReaction::channel("oversized", schema(), oversized)
            .err()
            .unwrap()
            .downcast_ref(),
        Some(ApplicationReactionError::Capacity { capacity, maximum })
            if *capacity == oversized.get() && *maximum == tokio::sync::Semaphore::MAX_PERMITS
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn channel_backpressure_cancellation_and_three_restarts_preserve_envelopes() {
    let (mut reaction, mut receiver) =
        ApplicationReaction::channel("channel", schema(), NonZeroUsize::new(1).unwrap()).unwrap();
    for sequence in 1..=3 {
        reaction.start().await.unwrap();
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .is::<ApplicationReactionError>());
        let expected = input(sequence);
        reaction.handle(expected.clone()).await.unwrap();
        assert_eq!(receiver.len(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), reaction.handle(input(99)))
                .await
                .is_err()
        );
        reaction.stop().await.unwrap();
        let actual = receiver.recv().await.unwrap();
        assert_eq!(actual.port, expected.port);
        assert!(Arc::ptr_eq(
            actual.envelope.event(),
            expected.envelope.event()
        ));
        assert_eq!(
            actual.envelope.context().entries().collect::<Vec<_>>(),
            expected.envelope.context().entries().collect::<Vec<_>>()
        );
        assert!(
            receiver.try_recv().is_err(),
            "cancelled send must not publish"
        );
        reaction.stop().await.unwrap();
    }
    receiver.close();
    assert!(matches!(
        reaction.start().await.unwrap_err().downcast_ref(),
        Some(ApplicationReactionError::ReceiverClosed)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn closed_receiver_fails_delivery_without_success_fallback() {
    let (mut reaction, receiver) =
        ApplicationReaction::channel("channel", schema(), NonZeroUsize::new(1).unwrap()).unwrap();
    reaction.start().await.unwrap();
    drop(receiver);
    assert!(matches!(
        reaction.handle(input(1)).await.unwrap_err().downcast_ref(),
        Some(ApplicationReactionError::ReceiverClosed)
    ));
    reaction.stop().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn validates_lifecycle_port_and_full_schema_before_calling_application() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut reaction = ApplicationReaction::callback("callback", schema(), move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    })
    .unwrap();
    assert!(matches!(
        reaction.handle(input(1)).await.unwrap_err().downcast_ref(),
        Some(ApplicationReactionError::NotRunning)
    ));
    reaction.start().await.unwrap();
    let mut wrong_port = input(1);
    wrong_port.port = PortId::try_new("wrong").unwrap();
    assert!(matches!(
        reaction
            .handle(wrong_port)
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(ApplicationReactionError::InputPort(_))
    ));
    let wrong_schema = SchemaDescriptor::try_new(
        schema().id().clone(),
        schema().version(),
        "different-encoding",
        b"different-definition".to_vec().into(),
    )
    .unwrap();
    let mut wrong = input(1);
    wrong.envelope = ChangeEnvelope::new(
        wrong.envelope.id().clone(),
        ChangeSet::try_new(wrong.envelope.changes().id().clone(), wrong_schema, vec![]).unwrap(),
        wrong.envelope.system().as_ref().clone(),
    );
    assert!(matches!(
        reaction.handle(wrong).await.unwrap_err().downcast_ref(),
        Some(ApplicationReactionError::SchemaMismatch)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    reaction.handle(input(1)).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    reaction.stop().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_callback_and_stop_retain_submitted_blocking_work() {
    let (started, mut entered) = oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let completed = Arc::new(AtomicUsize::new(0));
    let observed = completed.clone();
    let mut job = Some((started, blocked));
    let mut reaction = ApplicationReaction::callback("callback", schema(), move |_| {
        let (started, blocked) = job.take().expect("callback invoked only once");
        let completed = observed.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                blocked.recv().unwrap();
                completed.fetch_add(1, Ordering::SeqCst);
            })
            .await?;
            Ok(())
        }
    })
    .unwrap();
    reaction.start().await.unwrap();
    {
        let handling = reaction.handle(input(1));
        tokio::pin!(handling);
        tokio::select! {
            _ = &mut entered => {}
            result = &mut handling => panic!("work completed while blocked: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("work never submitted"),
        }
    }
    for _ in 0..2 {
        assert!(
            tokio::time::timeout(Duration::from_millis(20), reaction.stop())
                .await
                .is_err()
        );
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        assert!(matches!(
            reaction.start().await.unwrap_err().downcast_ref(),
            Some(ApplicationReactionError::CleanupRequired)
        ));
        assert!(matches!(
            reaction.handle(input(2)).await.unwrap_err().downcast_ref(),
            Some(ApplicationReactionError::CleanupRequired)
        ));
    }
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), reaction.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    reaction.start().await.unwrap();
    reaction.stop().await.unwrap();
}

#[derive(Debug, thiserror::Error)]
#[error("application operation failed")]
struct ApplicationFailure;

#[tokio::test(flavor = "current_thread")]
async fn callback_errors_remain_typed_in_handling_and_cancelled_cleanup() {
    let release = Arc::new(Notify::new());
    let gate = release.clone();
    let mut reaction = ApplicationReaction::callback("callback", schema(), move |_| {
        let gate = gate.clone();
        async move {
            gate.notified().await;
            Err(ApplicationFailure.into())
        }
    })
    .unwrap();
    reaction.start().await.unwrap();
    release.notify_one();
    assert!(reaction
        .handle(input(1))
        .await
        .unwrap_err()
        .is::<ApplicationFailure>());
    assert!(reaction
        .handle(input(2))
        .await
        .unwrap_err()
        .is::<ApplicationReactionError>());
    reaction.stop().await.unwrap();
    reaction.start().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), reaction.handle(input(2)))
            .await
            .is_err()
    );
    release.notify_one();
    assert!(reaction
        .stop()
        .await
        .unwrap_err()
        .is::<ApplicationFailure>());
    assert!(reaction.start().await.is_err());
    reaction.stop().await.unwrap();
    reaction.start().await.unwrap();
    reaction.stop().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn callback_factory_and_future_panics_poison_only_their_binding() {
    let factory = ApplicationReaction::callback(
        "factory-panic",
        schema(),
        |_| -> std::future::Ready<anyhow::Result<()>> { panic!("factory panic") },
    )
    .unwrap();
    let future = ApplicationReaction::callback("future-panic", schema(), |_| async {
        tokio::task::yield_now().await;
        panic!("future panic");
    })
    .unwrap();
    for mut reaction in [factory, future] {
        reaction.start().await.unwrap();
        assert!(matches!(
            reaction.handle(input(1)).await.unwrap_err().downcast_ref(),
            Some(ApplicationReactionError::CallbackPanicked(_))
        ));
        reaction.stop().await.unwrap();
        assert!(matches!(
            reaction.start().await.unwrap_err().downcast_ref(),
            Some(ApplicationReactionError::CallbackPoisoned)
        ));
    }
    let mut healthy =
        ApplicationReaction::callback("healthy", schema(), |_| async { Ok(()) }).unwrap();
    for sequence in 1..=3 {
        healthy.start().await.unwrap();
        healthy.handle(input(sequence)).await.unwrap();
        healthy.stop().await.unwrap();
    }
}
