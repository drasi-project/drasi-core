// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::Context;
use drasi_computation_contracts::{PhysicalClock, PortId, ScheduledRequest, ScheduledWork};
use drasi_computation_plugin_sdk as sdk;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference},
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use futures_util::FutureExt;
use tokio::sync::oneshot;

use super::super::ProcessingBinding;

struct Clock(AtomicU64);
impl PhysicalClock for Clock {
    fn now(&self) -> anyhow::Result<u64> {
        Ok(self.0.load(Ordering::Acquire))
    }
}

fn binding(path: &Path, clock: Arc<Clock>) -> anyhow::Result<ProcessingBinding> {
    ProcessingBinding::new(
        Arc::new(RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
            ),
        )),
        "scheduled-contract".into(),
        "processor".into(),
    )
    .with_committed_input_progress(Some(PortId::try_new("in")?))?
    .with_scheduling(true, Some(clock))
}

fn request(key: &str, due_time_ms: u64) -> ScheduledRequest {
    ScheduledRequest {
        key: key.into(),
        due_time_ms,
        payload: vec![1, 2, 3],
    }
}

fn element() -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "one"),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: 123,
        },
        properties: ElementPropertyMap::default(),
    }
}

async fn commit_visibility() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let clock = Arc::new(Clock(AtomicU64::new(10_000)));
    let mut binding = binding(directory.path(), clock)?;
    let service =
        binding.with_table(|table| unsafe { sdk::NativeProcessing::from_borrowed(table) })?;
    let scheduling = service
        .scheduling()
        .context("missing negotiated scheduling")?;
    assert!(
        scheduling.next_deadline().await.is_err(),
        "not active before open"
    );
    binding.open().await?;
    let (staged, staged_wait) = oneshot::channel();
    let (commit, commit_wait) = oneshot::channel();
    let work = service.run(move |context| async move {
        context.set_element(&element(), &[0]).await?;
        let work = context.request_scheduled_work(request("one", 1000)).await?;
        staged
            .send(())
            .map_err(|_| anyhow::anyhow!("staged observer dropped"))?;
        commit_wait.await?;
        Ok(work)
    });
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => anyhow::bail!("operation completed before commit permission: {result:?}"),
        result = staged_wait => result?,
    }
    assert_eq!(scheduling.next_deadline().await?, None);
    assert!(
        scheduling.wakeup_source().wait().now_or_never().is_none(),
        "staged overdue work must not wake the graph"
    );
    commit
        .send(())
        .map_err(|_| anyhow::anyhow!("commit receiver dropped"))?;
    let original = work.await?;
    assert_eq!(scheduling.next_deadline().await?, Some(1000));
    scheduling.wakeup_source().wait().await?;

    let token = original.token.clone();
    let replacement: ScheduledWork = service
        .run(move |context| async move {
            context
                .replace_scheduled_work(&token, request("one", 30_000))
                .await
                .map_err(Into::into)
        })
        .await?;
    assert_ne!(replacement.token, original.token);
    assert_eq!(scheduling.next_deadline().await?, Some(30_000));
    assert!(scheduling.wakeup_source().wait().now_or_never().is_none());
    service
        .run(|context| async move {
            assert!(context.claim_due_scheduled_work().await?.is_none());
            Ok(())
        })
        .await?;
    service
        .run(move |context| async move {
            context.cancel_scheduled_work(&replacement.token).await?;
            context.delete_element(element().get_reference()).await?;
            Ok(())
        })
        .await?;
    assert_eq!(scheduling.next_deadline().await?, None);
    binding.close().await?;
    assert!(
        scheduling.next_deadline().await.is_err(),
        "stop revokes observation"
    );
    assert!(scheduling.wakeup_source().wait().await.is_err());
    binding.open().await?;
    assert_eq!(scheduling.next_deadline().await?, None);
    service
        .run(|context| async move {
            assert!(context
                .get_element(element().get_reference())
                .await?
                .is_none());
            assert!(context.scheduled_work("one").await?.is_none());
            Ok(())
        })
        .await?;
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn scheduled_commit_visibility_current_thread() -> anyhow::Result<()> {
    commit_visibility().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_commit_visibility_multi_thread() -> anyhow::Result<()> {
    commit_visibility().await
}

async fn rollback_and_completion() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let clock = Arc::new(Clock(AtomicU64::new(10_000)));
    let mut binding = binding(directory.path(), clock)?;
    let service =
        binding.with_table(|table| unsafe { sdk::NativeProcessing::from_borrowed(table) })?;
    let scheduling = service.scheduling().context("scheduling")?;
    binding.open().await?;
    let (staged, staged_wait) = oneshot::channel();
    let failed: anyhow::Result<()> = service
        .run(move |context| async move {
            context.set_element(&element(), &[0]).await?;
            let work = context.request_scheduled_work(request("one", 1000)).await?;
            staged
                .send(work.token)
                .map_err(|_| anyhow::anyhow!("token observer dropped"))?;
            anyhow::bail!("intentional rollback")
        })
        .await;
    assert!(failed.is_err());
    let aborted = staged_wait.await?;
    assert_eq!(scheduling.next_deadline().await?, None);
    binding.close().await?;
    binding.open().await?;
    let committed: ScheduledWork = service
        .run(|context| async move {
            assert!(context
                .get_element(element().get_reference())
                .await?
                .is_none());
            assert!(context.scheduled_work("one").await?.is_none());
            Ok(context.request_scheduled_work(request("one", 1000)).await?)
        })
        .await?;
    assert_ne!(committed.token, aborted);

    let wrong = service
        .run(move |context| async move {
            context.cancel_scheduled_work(&aborted).await?;
            Ok(())
        })
        .await;
    assert!(
        wrong.is_err(),
        "a rolled-back token cannot cancel a later request"
    );
    binding.close().await?;
    binding.open().await?;
    let incomplete = service
        .run(|context| async move {
            assert!(context.claim_due_scheduled_work().await?.is_some());
            context.set_element(&element(), &[0]).await?;
            Ok(())
        })
        .await;
    assert!(
        incomplete.is_err(),
        "a claim without completion must roll back"
    );
    binding.close().await?;
    binding.open().await?;
    let token = committed.token.clone();
    service
        .run(move |context| async move {
            assert!(context
                .get_element(element().get_reference())
                .await?
                .is_none());
            let claimed = context
                .claim_due_scheduled_work()
                .await?
                .context("restored claim")?;
            assert_eq!(claimed.token, token);
            context.set_element(&element(), &[0]).await?;
            context.complete_scheduled_work(&claimed.token).await?;
            context.stage_checkpoint("completed", 1, None).await?;
            Ok(())
        })
        .await?;
    assert_eq!(scheduling.next_deadline().await?, None);
    binding.close().await?;
    binding.open().await?;
    service
        .run(|context| async move {
            assert!(context.claim_due_scheduled_work().await?.is_none());
            assert!(context
                .get_element(element().get_reference())
                .await?
                .is_some());
            assert_eq!(
                context
                    .read_checkpoint("completed")
                    .await?
                    .context("committed completion")?
                    .sequence,
                1
            );
            Ok(())
        })
        .await?;
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn scheduled_rollback_and_completion_current_thread() -> anyhow::Result<()> {
    rollback_and_completion().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_rollback_and_completion_multi_thread() -> anyhow::Result<()> {
    rollback_and_completion().await
}

#[tokio::test]
async fn scheduled_service_rejects_unnegotiated_requests_and_volatile_storage() -> anyhow::Result<()>
{
    let memory = ProcessingBinding::new(
        Arc::new(InMemoryComputationProvider),
        "graph".into(),
        "processor".into(),
    )
    .with_committed_input_progress(Some(PortId::try_new("in")?))?
    .with_scheduling(true, None);
    assert!(
        memory.is_err(),
        "no-op memory sessions cannot promise rollback"
    );

    let directory = tempfile::tempdir()?;
    let mut binding = ProcessingBinding::new(
        Arc::new(RocksDbComputationProvider::new(
            directory.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(16 << 20)?,
            ),
        )),
        "unnegotiated".into(),
        "processor".into(),
    );
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    assert!(service.scheduling().is_none());
    binding.open().await?;
    let failure = service
        .run(|context| async move {
            context.request_scheduled_work(request("one", 1000)).await?;
            Ok(())
        })
        .await;
    assert!(failure.is_err());
    binding.close().await?;
    Ok(())
}

async fn cancelled_operation() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let clock = Arc::new(Clock(AtomicU64::new(10_000)));
    let mut binding = binding(directory.path(), clock)?;
    let service =
        binding.with_table(|table| unsafe { sdk::NativeProcessing::from_borrowed(table) })?;
    let scheduling = service.scheduling().context("scheduling")?;
    binding.open().await?;
    let (staged, staged_wait) = oneshot::channel();
    let mut work = Box::pin(service.run(move |context| async move {
        context.request_scheduled_work(request("one", 1000)).await?;
        context.set_element(&element(), &[0]).await?;
        staged
            .send(())
            .map_err(|_| anyhow::anyhow!("cancel observer dropped"))?;
        std::future::pending::<anyhow::Result<()>>().await
    }));
    tokio::select! {
        result = &mut work => anyhow::bail!("cancelled operation completed prematurely: {result:?}"),
        result = staged_wait => result?,
    }
    let wakeup = scheduling.wakeup_source();
    let mut waiting = Box::pin(wakeup.wait());
    assert!(futures_util::poll!(&mut waiting).is_pending());
    drop(work);
    binding.close().await?;
    assert!(
        waiting.await.is_err(),
        "pending observation is revoked by cleanup"
    );
    binding.open().await?;
    assert_eq!(scheduling.next_deadline().await?, None);
    service
        .run(|context| async move {
            assert!(context
                .get_element(element().get_reference())
                .await?
                .is_none());
            assert!(context.scheduled_work("one").await?.is_none());
            Ok(())
        })
        .await?;
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn scheduled_cancellation_awaits_cleanup_current_thread() -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), cancelled_operation()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_cancellation_awaits_cleanup_multi_thread() -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), cancelled_operation()).await?
}
