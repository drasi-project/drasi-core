// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::models::ElementValue;
use std::sync::{atomic::AtomicUsize, Mutex};

async fn state_runner(path: &Path) -> anyhow::Result<DeliveryRunner> {
    DeliveryRunner::new_transactional(scope(), indexes(path).await, codec(), options())
}

#[derive(Debug, thiserror::Error)]
#[error("transactional handler rejected the operation")]
struct Rejected;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct StateReadError(#[source] anyhow::Error);

#[derive(Default)]
struct Stateful {
    attempts: Mutex<Vec<String>>,
    fail: Option<u64>,
    failures: AtomicUsize,
    transient: bool,
    hold: Option<Arc<Notify>>,
    arm_commit: Option<Arc<AtomicBool>>,
}

#[async_trait]
impl TransactionalDeliveryHandler for Stateful {
    fn retryable(&self, error: &anyhow::Error) -> bool {
        self.transient && error.downcast_ref::<Rejected>().is_some()
    }

    async fn handle(
        &self,
        item: DeliveryItem<'_>,
        context: &TransactionContext<'_>,
    ) -> anyhow::Result<()> {
        self.attempts
            .lock()
            .expect("attempts")
            .push(item.id.to_string());
        let count = match context.get("count").await? {
            Some(ElementValue::Integer(count)) => count,
            None => 0,
            value => anyhow::bail!("unexpected state: {value:?}"),
        };
        context
            .put("count", ElementValue::Integer(count + 1))
            .await?;
        if let Some(armed) = &self.arm_commit {
            armed.store(true, Ordering::Release);
        }
        if let Some(entered) = &self.hold {
            entered.notify_one();
            std::future::pending::<()>().await;
        }
        if self.fail == Some(item.operation.ordinal())
            && self
                .failures
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    value.checked_sub(1)
                })
                .is_ok()
        {
            return Err(Rejected.into());
        }
        Ok(())
    }
}

async fn state(delivery: &DeliveryRunner) -> anyhow::Result<Option<ElementValue>> {
    let context = TransactionContext::new(
        &delivery.scope.consumer,
        delivery
            .transaction
            .resources()
            .indexes()
            .element_index
            .as_ref(),
        StreamId::try_new("inspection")?,
        1,
    );
    Ok(delivery
        .transaction
        .run(async {
            context
                .get("count")
                .await
                .map_err(|error| IndexError::other(StateReadError(error)).into())
        })
        .await?)
}

#[tokio::test(flavor = "current_thread")]
async fn state_and_operation_progress_share_the_same_commit_and_rollback() -> anyhow::Result<()> {
    let path = tempfile::tempdir()?;
    let mut delivery = state_runner(path.path()).await?;
    let batch = input(&producer("out"), 1, 2);
    let handler = Stateful {
        fail: Some(1),
        failures: AtomicUsize::new(1),
        ..Default::default()
    };
    let error = delivery
        .deliver_transactional(&batch, &handler)
        .await
        .unwrap_err();
    let Some(DeliveryError::Handling {
        attempts, source, ..
    }) = error.downcast_ref()
    else {
        panic!("typed handling failure was lost: {error:#}");
    };
    assert_eq!(*attempts, 1);
    assert!(source.downcast_ref::<Rejected>().is_some());
    assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(1)));
    assert_eq!(delivery.progress().await?[0].completed_operations, 1);
    delivery
        .deliver_transactional(&input(&producer("other"), 1, 1), &handler)
        .await?;
    assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(2)));
    assert_eq!(delivery.progress().await?[0].completed_operations, 1);
    delivery.deliver_transactional(&batch, &handler).await?;
    assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(3)));
    assert_eq!(delivery.progress().await?[0].completed_operations, 2);
    delivery.deliver_transactional(&batch, &handler).await?;
    assert_eq!(handler.attempts.lock().expect("attempts").len(), 4);
    delivery.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn transactional_retries_roll_back_state_before_reusing_the_same_operation_id(
) -> anyhow::Result<()> {
    let path = tempfile::tempdir()?;
    let mut options = options();
    options.retry.max_attempts = size(2);
    let mut delivery =
        DeliveryRunner::new_transactional(scope(), indexes(path.path()).await, codec(), options)?;
    let handler = Stateful {
        fail: Some(0),
        failures: AtomicUsize::new(1),
        transient: true,
        ..Default::default()
    };
    delivery
        .deliver_transactional(&input(&producer("out"), 1, 2), &handler)
        .await?;
    assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(2)));
    let attempts = handler.attempts.lock().expect("attempts").clone();
    assert_eq!(attempts.len(), 3);
    assert_eq!(attempts[0], attempts[1]);
    assert_ne!(attempts[1], attempts[2]);
    delivery.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn uncertain_state_commits_reconstruct_state_and_progress_together() -> anyhow::Result<()> {
    for after in [false, true] {
        let path = tempfile::tempdir()?;
        let original = indexes(path.path()).await;
        let armed = Arc::new(AtomicBool::new(false));
        let control = Arc::new(CommitFailure {
            inner: original.indexes().session_control.clone(),
            armed: armed.clone(),
            after,
            exit: false,
        });
        let mut delivery = DeliveryRunner::new_transactional(
            scope(),
            with_control(original, control),
            codec(),
            options(),
        )?;
        let batch = input(&producer("out"), 1, 2);
        let handler = Stateful {
            arm_commit: Some(armed),
            ..Default::default()
        };
        assert!(delivery
            .deliver_transactional(&batch, &handler)
            .await
            .is_err());
        assert!(delivery.progress().await.is_err());
        assert!(delivery
            .deliver_transactional(&batch, &handler)
            .await
            .is_err());
        assert_eq!(handler.attempts.lock().expect("attempts").len(), 1);
        delivery.shutdown().await?;
        drop(delivery);
        let mut delivery = state_runner(path.path()).await?;
        assert_eq!(
            delivery.progress().await?[0].completed_operations,
            usize::from(after)
        );
        assert_eq!(
            state(&delivery).await?,
            after.then_some(ElementValue::Integer(1))
        );
        delivery
            .deliver_transactional(&batch, &Stateful::default())
            .await?;
        assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(2)));
        delivery.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_state_handling_fences_and_rolls_back_before_reconstruction() -> anyhow::Result<()>
{
    let path = tempfile::tempdir()?;
    let mut delivery = state_runner(path.path()).await?;
    let entered = Arc::new(Notify::new());
    let handler = Stateful {
        hold: Some(entered.clone()),
        ..Default::default()
    };
    let batch = input(&producer("out"), 1, 2);
    {
        let pending = delivery.deliver_transactional(&batch, &handler);
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("state transaction must remain active: {result:?}"),
            _ = entered.notified() => {}
        }
    }
    assert!(delivery.progress().await.is_err());
    delivery.shutdown().await?;
    drop(delivery);
    let mut delivery = state_runner(path.path()).await?;
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    assert_eq!(state(&delivery).await?, None);
    delivery
        .deliver_transactional(&batch, &Stateful::default())
        .await?;
    assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(2)));
    delivery.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn handling_modes_cannot_change_calls_or_reinterpret_persisted_progress() -> anyhow::Result<()>
{
    for transactional in [false, true] {
        let path = tempfile::tempdir()?;
        let mut delivery = if transactional {
            state_runner(path.path()).await?
        } else {
            runner(path.path()).await
        };
        let batch = input(&producer("out"), 1, 1);
        let mut external = Handler::default();
        let stateful = Stateful::default();
        let error = if transactional {
            delivery.deliver(&batch, &mut external).await.unwrap_err()
        } else {
            delivery
                .deliver_transactional(&batch, &stateful)
                .await
                .unwrap_err()
        };
        assert!(matches!(
            error.downcast_ref(),
            Some(DeliveryError::ModeMismatch)
        ));
        if transactional {
            delivery.deliver_transactional(&batch, &stateful).await?;
        } else {
            delivery.deliver(&batch, &mut external).await?;
        }
        let rows = delivery
            .transaction
            .resources()
            .outbox_writer()
            .expect("outbox")
            .read_from(METADATA_KEY, 0)
            .await?;
        let saved: serde_json::Value = serde_json::from_slice(&rows[0].1)?;
        assert_eq!(
            saved.get("mode").is_some(),
            transactional,
            "external v1 records must retain their original shape"
        );
        delivery.shutdown().await?;
        drop(delivery);
        let mut opposite = if transactional {
            runner(path.path()).await
        } else {
            state_runner(path.path()).await?
        };
        let error = opposite.progress().await.unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(DeliveryError::ModeMismatch)
        ));
        opposite.shutdown().await?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("injected rollback failure")]
struct RollbackRejected;

struct RollbackFailure {
    inner: Arc<dyn SessionControl>,
    fail: AtomicBool,
}

#[async_trait]
impl SessionControl for RollbackFailure {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }

    async fn commit(&self) -> Result<(), IndexError> {
        self.inner.commit().await
    }

    fn rollback(&self) -> Result<(), IndexError> {
        if self.fail.swap(false, Ordering::AcqRel) {
            return Err(IndexError::other(RollbackRejected));
        }
        self.inner.rollback()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn failed_state_rollback_preserves_both_causes_and_prevents_retries() -> anyhow::Result<()> {
    let path = tempfile::tempdir()?;
    let original = indexes(path.path()).await;
    let control = Arc::new(RollbackFailure {
        inner: original.indexes().session_control.clone(),
        fail: AtomicBool::new(true),
    });
    let mut options = options();
    options.retry.max_attempts = size(2);
    let mut delivery = DeliveryRunner::new_transactional(
        scope(),
        with_control(original, control),
        codec(),
        options,
    )?;
    let handler = Stateful {
        fail: Some(0),
        failures: AtomicUsize::new(1),
        transient: true,
        ..Default::default()
    };
    let batch = input(&producer("out"), 1, 2);
    let error = delivery
        .deliver_transactional(&batch, &handler)
        .await
        .unwrap_err();
    let Some(ComputationQueryError::Rollback { failure, rollback }) = error.downcast_ref() else {
        panic!("rollback failure was lost: {error:#}");
    };
    let ComputationQueryError::Index(IndexError::Other(cause)) = failure.as_ref() else {
        panic!("handling failure was lost: {failure:?}");
    };
    let Some(DeliveryError::Handling { source, .. }) = cause.downcast_ref() else {
        panic!("typed handling failure was lost: {cause:?}");
    };
    assert!(source.downcast_ref::<Rejected>().is_some());
    let IndexError::Other(cause) = rollback else {
        panic!("rollback cause was lost: {rollback:?}");
    };
    assert!(cause.downcast_ref::<RollbackRejected>().is_some());
    assert!(delivery.progress().await.is_err());
    assert!(delivery
        .deliver_transactional(&batch, &handler)
        .await
        .is_err());
    assert_eq!(handler.attempts.lock().expect("attempts").len(), 1);
    delivery.shutdown().await?;
    drop(delivery);
    let mut delivery = state_runner(path.path()).await?;
    assert_eq!(state(&delivery).await?, None);
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    delivery.deliver_transactional(&batch, &handler).await?;
    assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(2)));
    delivery.shutdown().await?;
    Ok(())
}

struct StatefulSink {
    descriptor: ComponentDescriptor,
    delivery: DeliveryRunner,
    handler: Arc<Stateful>,
}

#[async_trait]
impl ComputationComponent for StatefulSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.delivery.progress().await?;
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.delivery.shutdown().await
    }
}

#[async_trait]
impl EnvelopeSink for StatefulSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.delivery
            .deliver_transactional(&input, self.handler.as_ref())
            .await
    }
}

async fn stateful_sink(
    path: &Path,
    handler: Arc<Stateful>,
) -> anyhow::Result<Box<dyn EnvelopeSink>> {
    Ok(Box::new(StatefulSink {
        descriptor: descriptor("sink", "in", PortDirection::Input)?,
        delivery: state_runner(path).await?,
        handler,
    }))
}

#[tokio::test(flavor = "current_thread")]
async fn connected_qos_reconstructs_state_before_acknowledging_the_whole_batch(
) -> anyhow::Result<()> {
    let path = tempfile::tempdir()?;
    let batch = input(&producer("out"), 1, 3);
    let handler = Arc::new(Stateful {
        fail: Some(1),
        failures: AtomicUsize::new(1),
        ..Default::default()
    });
    let (mut running, channel) = graph(
        path.path(),
        Some(batch),
        stateful_sink(path.path(), handler.clone()).await?,
    )
    .await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), running.start()?)
            .await?
            .is_err()
    );
    assert_eq!(channel.progress().await?.processed["target"], 0);
    running.shutdown().await?;
    channel.shutdown().await?;
    drop(running);
    drop(channel);
    let mut delivery = state_runner(path.path()).await?;
    assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(1)));
    assert_eq!(delivery.progress().await?[0].completed_operations, 1);
    delivery.shutdown().await?;
    drop(delivery);

    let (mut running, channel) = graph(
        path.path(),
        None,
        stateful_sink(path.path(), handler.clone()).await?,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), running.start()?).await??;
    assert_eq!(channel.progress().await?.accepted, 1);
    assert_eq!(channel.progress().await?.processed["target"], 1);
    let attempts = handler.attempts.lock().expect("attempts").clone();
    assert_eq!(attempts.len(), 4);
    assert_eq!(attempts[1], attempts[2]);
    running.shutdown().await?;
    channel.shutdown().await?;
    drop(running);
    drop(channel);
    let mut delivery = state_runner(path.path()).await?;
    assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(3)));
    assert_eq!(delivery.progress().await?[0].completed_operations, 3);
    delivery.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn damaged_handling_modes_reject_before_state_mutation() -> anyhow::Result<()> {
    for mode in [Some(serde_json::json!("unknown")), Some(serde_json::Value::Null), None] {
        let path = tempfile::tempdir()?;
        let mut delivery = state_runner(path.path()).await?;
        let batch = input(&producer("out"), 1, 1);
        delivery
            .deliver_transactional(&batch, &Stateful::default())
            .await?;
        delivery.shutdown().await?;
        drop(delivery);
        let transaction = ComputationTransaction::try_new(indexes(path.path()).await)?;
        let outbox = transaction.resources().outbox_writer().expect("outbox");
        transaction
            .run(async {
                let rows = outbox.read_from(METADATA_KEY, 0).await?;
                let mut ledger: serde_json::Value =
                    serde_json::from_slice(&rows[0].1).expect("ledger");
                let fields = ledger.as_object_mut().expect("ledger object");
                if let Some(mode) = mode {
                    fields.insert("mode".into(), mode);
                } else {
                    fields.remove("mode");
                }
                outbox
                    .append(
                        METADATA_KEY,
                        1,
                        &serde_json::to_vec(&ledger).expect("encode"),
                    )
                    .await?;
                Ok(())
            })
            .await?;
        transaction.shutdown().await?;
        drop(transaction);
        let mut delivery = state_runner(path.path()).await?;
        let handler = Stateful::default();
        assert!(delivery
            .deliver_transactional(&batch, &handler)
            .await
            .is_err());
        assert!(handler.attempts.lock().expect("attempts").is_empty());
        assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(1)));
        delivery.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "invoked by transactional_state_survives_process_exit_at_commit"]
async fn transactional_delivery_crash_child() -> anyhow::Result<()> {
    let path = std::path::PathBuf::from(std::env::var(CRASH_ROOT)?);
    let original = indexes(&path).await;
    let armed = Arc::new(AtomicBool::new(false));
    let control = Arc::new(CommitFailure {
        inner: original.indexes().session_control.clone(),
        armed: armed.clone(),
        after: std::env::var(CRASH_AFTER)? == "true",
        exit: true,
    });
    let mut delivery = DeliveryRunner::new_transactional(
        scope(),
        with_control(original, control),
        codec(),
        options(),
    )?;
    let batch = InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: codec().decode(&std::fs::read(path.join("input.json"))?)?,
    };
    delivery
        .deliver_transactional(
            &batch,
            &Stateful {
                arm_commit: Some(armed),
                ..Default::default()
            },
        )
        .await?;
    anyhow::bail!("transactional delivery did not reach its required commit");
}

#[tokio::test(flavor = "current_thread")]
async fn transactional_state_survives_process_exit_at_commit() -> anyhow::Result<()> {
    for after in [false, true] {
        let path = tempfile::tempdir()?;
        let batch = input(&producer("out"), 1, 2);
        std::fs::write(
            path.path().join("input.json"),
            codec().encode(&batch.envelope)?,
        )?;
        crash_at_commit(
            "computation::v1::delivery::tests::transactional::transactional_delivery_crash_child",
            path.path(),
            after,
        )
        .await?;
        let mut delivery = state_runner(path.path()).await?;
        assert_eq!(
            delivery.progress().await?[0].completed_operations,
            usize::from(after)
        );
        assert_eq!(
            state(&delivery).await?,
            after.then_some(ElementValue::Integer(1))
        );
        delivery
            .deliver_transactional(&batch, &Stateful::default())
            .await?;
        assert_eq!(state(&delivery).await?, Some(ElementValue::Integer(2)));
        delivery.shutdown().await?;
    }
    Ok(())
}
