// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

async fn crash_worker(name: &str, root: &Path, phase: usize) -> Result<std::process::Output> {
    Ok(tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(std::env::current_exe()?)
            .args(["--exact", name, "--ignored", "--nocapture"])
            .env("DRASI_QOS_RELIABILITY_ROOT", root)
            .env("DRASI_QOS_RELIABILITY_PHASE", phase.to_string())
            .env("RUST_LOG", "warn")
            .kill_on_drop(true)
            .output(),
    )
    .await??)
}

#[tokio::test]
#[ignore = "isolated log-capture worker invoked by the parent"]
async fn skip_notification_worker() -> Result<()> {
    let core = drasi_lib::DrasiLib::builder().build().await?;
    let desired = definition(false, RetentionPolicy::PruneOldest, 1);
    let channel = QosChannel::volatile(desired.clone())?;
    let mut pipe = endpoint(&channel, &desired, "slow", true)?;
    let mut receiver = pipe.pipe.take_receiver()?;
    for last in [4, 8] {
        for sequence in last - 3..=last {
            channel.publish(&event(sequence)?).await?;
        }
        // Dropping handling retries the same delivery, not the already committed skip.
        drop(received(receiver.as_mut(), last).await?);
        received(receiver.as_mut(), last)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
    }
    channel.shutdown().await?;

    let root = std::path::PathBuf::from(std::env::var("DRASI_QOS_RELIABILITY_ROOT")?);
    let mut desired = definition(true, RetentionPolicy::PruneOldest, 1);
    desired.subscribers = BTreeMap::from([("cancelled".into(), SubscriptionStart::Earliest)]);
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate = Arc::new(CommitGate {
        before_commit: true,
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let channel =
        uncertain_channel(&root, desired.clone(), fail.clone(), Some(gate.clone())).await?;
    channel.publish(&event(1)?).await?;
    channel.publish(&event(2)?).await?;
    let mut pipe = endpoint(&channel, &desired, "cancelled", true)?;
    let mut receiver = pipe.pipe.take_receiver()?;
    fail.store(true, std::sync::atomic::Ordering::Release);
    let mut skip = Box::pin(receiver.receive());
    tokio::select! {
        result = &mut skip => panic!("skip commit must be held: {:?}", result.err()),
        _ = gate.entered.notified() => {}
    }
    drop(skip);
    channel.shutdown().await?;
    let recovered = persistent(&root, desired).await?;
    assert_eq!(recovered.progress().await?.processed["cancelled"], 0);
    recovered.shutdown().await?;
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn skip_notifications_report_exact_committed_ranges_not_cancelled_attempts() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let output = crash_worker("reliability::skip_notification_worker", directory.path(), 0).await?;
    assert!(output.status.success(), "{output:?}");
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        logs.matches("QoS subscriber slow explicitly skips positions 1..4")
            .count(),
        1,
        "{logs}"
    );
    assert_eq!(
        logs.matches("QoS subscriber slow explicitly skips positions 5..8")
            .count(),
        1,
        "{logs}"
    );
    assert_eq!(
        logs.matches("explicitly skips positions").count(),
        2,
        "{logs}"
    );
    Ok(())
}

fn membership_definition() -> QosChannelDefinition {
    let mut desired = definition(true, RetentionPolicy::Backpressure, 32);
    desired.subscribers.remove("slow");
    desired
        .subscribers
        .insert("new".into(), SubscriptionStart::Latest);
    desired
}

#[tokio::test]
#[ignore = "membership process-exit worker invoked by the parent"]
async fn membership_crash_worker() -> Result<()> {
    let root = std::path::PathBuf::from(std::env::var("DRASI_QOS_RELIABILITY_ROOT")?);
    let phase: usize = std::env::var("DRASI_QOS_RELIABILITY_PHASE")?.parse()?;
    let original = definition(true, RetentionPolicy::Backpressure, 32);
    let channel = persistent(&root, original.clone()).await?;
    let mut pipe = endpoint(&channel, &original, "fast", false)?;
    let mut fast = pipe.pipe.take_receiver()?;
    for sequence in 1..=32 {
        channel.publish(&event(sequence)?).await?;
    }
    for sequence in 1..=16 {
        received(fast.as_mut(), sequence)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
    }
    let blocked = event(33)?;
    assert!(futures::poll!(Box::pin(channel.publish(&blocked))).is_pending());
    channel.shutdown().await?;
    drop((channel, pipe, fast));
    let updated = membership_definition();
    let channel = persistent(&root, updated.clone()).await?;
    assert_eq!(channel.progress().await?.processed["new"], 32);
    if phase == 1 {
        std::process::exit(85);
    }
    let mut fast_pipe = endpoint(&channel, &updated, "fast", false)?;
    let mut new_pipe = endpoint(&channel, &updated, "new", false)?;
    let mut fast = fast_pipe.pipe.take_receiver()?;
    let mut new = new_pipe.pipe.take_receiver()?;
    for sequence in 33..=48 {
        channel.publish(&event(sequence)?).await?;
    }
    for sequence in 17..=40 {
        received(fast.as_mut(), sequence)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
    }
    for sequence in 33..=40 {
        received(new.as_mut(), sequence)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
    }
    let _unhandled = received(new.as_mut(), 41).await?;
    std::process::exit(86);
}

#[tokio::test]
async fn membership_crashes_and_rejected_capacity_reductions_preserve_exact_obligations(
) -> Result<()> {
    for (phase, exit, head, fast_cut, new_cut) in [(1, 85, 32, 16, 32), (2, 86, 48, 40, 40)] {
        let directory = tempfile::tempdir()?;
        let output = crash_worker(
            "reliability::membership_crash_worker",
            directory.path(),
            phase,
        )
        .await?;
        assert_eq!(output.status.code(), Some(exit), "{output:?}");
        let desired = membership_definition();
        let mut rejected = desired.clone();
        rejected.capacity = NonZeroUsize::new(1).expect("nonzero capacity");
        assert!(persistent(directory.path(), rejected).await.is_err());
        let channel = persistent(directory.path(), desired.clone()).await?;
        let progress = channel.progress().await?;
        assert_eq!(progress.accepted, head);
        assert_eq!(progress.processed["fast"], fast_cut);
        assert_eq!(progress.processed["new"], new_cut);
        assert_eq!(progress.retired, ["slow"]);
        for (name, cut) in [("fast", fast_cut), ("new", new_cut)] {
            let mut pipe = endpoint(&channel, &desired, name, false)?;
            let mut receiver = pipe.pipe.take_receiver()?;
            for sequence in cut + 1..=head {
                let (envelope, ack) = receiver
                    .receive()
                    .await?
                    .expect("pending journal entry")
                    .into_parts();
                assert_eq!(envelope.id(), event(sequence)?.id());
                assert_eq!(
                    GraphChangeCodec::decode_changes(&envelope)?,
                    GraphChangeCodec::decode_changes(&event(sequence)?)?
                );
                ack.expect("handling acknowledgement")
                    .complete(HandlingOutcome::Handled)
                    .await?;
            }
        }
        channel.publish(&event(head + 1)?).await?;
        for name in ["fast", "new"] {
            let mut pipe = endpoint(&channel, &desired, name, false)?;
            let mut receiver = pipe.pipe.take_receiver()?;
            received(receiver.as_mut(), head + 1)
                .await?
                .complete(HandlingOutcome::Handled)
                .await?;
        }
        channel.shutdown().await?;
        let reopened = persistent(directory.path(), desired).await?;
        assert_eq!(reopened.progress().await?.processed["fast"], head + 1);
        assert_eq!(reopened.progress().await?.processed["new"], head + 1);
        reopened.shutdown().await?;
    }
    Ok(())
}

async fn disconnected_fanout() -> Result<()> {
    const MEMBERS: usize = 16;
    const CAPACITY: u64 = 16;
    let directory = tempfile::tempdir()?;
    let mut desired = definition(true, RetentionPolicy::Backpressure, CAPACITY as usize);
    desired.subscribers = (0..MEMBERS)
        .map(|index| (format!("member-{index}"), SubscriptionStart::Earliest))
        .collect();
    let channel = persistent(directory.path(), desired.clone()).await?;
    for batch in 0..8 {
        let first = batch * CAPACITY + 1;
        let last = first + CAPACITY - 1;
        for sequence in first..=last {
            channel.publish(&event(sequence)?).await?;
        }
        for member in 0..MEMBERS - 1 {
            let mut pipe = endpoint(&channel, &desired, &format!("member-{member}"), false)?;
            let mut receiver = pipe.pipe.take_receiver()?;
            for sequence in first..=last {
                received(receiver.as_mut(), sequence)
                    .await?
                    .complete(HandlingOutcome::Handled)
                    .await?;
            }
        }
        let next = event(last + 1)?;
        for _ in 0..256 {
            assert!(futures::poll!(Box::pin(channel.publish(&next))).is_pending());
            tokio::task::yield_now().await;
        }
        let progress = channel.progress().await?;
        assert_eq!(progress.accepted, last);
        assert_eq!(progress.earliest_available, Some(first));
        assert_eq!(progress.processed["member-15"], first - 1);
        let mut pipe = endpoint(&channel, &desired, "member-15", false)?;
        let mut receiver = pipe.pipe.take_receiver()?;
        let stale = received(receiver.as_mut(), first).await?;
        pipe.control.cancel();
        assert!(stale.complete(HandlingOutcome::Handled).await.is_err());
        drop((pipe, receiver));
        let mut pipe = endpoint(&channel, &desired, "member-15", false)?;
        let mut receiver = pipe.pipe.take_receiver()?;
        for sequence in first..=last {
            received(receiver.as_mut(), sequence)
                .await?
                .complete(HandlingOutcome::Handled)
                .await?;
        }
    }
    channel.shutdown().await?;
    let restored = persistent(directory.path(), desired).await?;
    let progress = restored.progress().await?;
    assert_eq!(progress.accepted, 128);
    assert!(progress.processed.values().all(|position| *position == 128));
    restored.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn disconnected_fanout_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(90), disconnected_fanout()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_fanout_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(90), disconnected_fanout()).await?
}
