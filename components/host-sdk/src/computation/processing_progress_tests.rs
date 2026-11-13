// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::Context;
use drasi_computation_contracts::{SourceProgressKey, SourceProgressView};
use std::time::Duration;

#[test]
fn native_progress_graceful_suspension_revokes_views_without_failure() -> anyhow::Result<()> {
    let binding = ProcessingBinding::new(
        Arc::new(InMemoryComputationProvider),
        "graceful-stop".into(),
        ComponentId::try_new(super::PROCESSOR)?,
    )
    .with_committed_input_progress(Some(PortId::try_new("in")?))?;
    let reader = binding.progress_owner().context("progress owner")?;
    let view = binding
        .committed_input_progress()?
        .context("progress view")?;
    let cleanup = binding.cleanup_call()?.context("cleanup guard")?;
    binding.suspend_progress()?;
    assert!(view.snapshot().is_err());
    assert!(!reader.snapshot().ready && !reader.snapshot().admitting);
    assert!(reader.snapshot().failure.is_none());
    cleanup.complete();
    assert!(reader.snapshot().failure.is_none());
    Ok(())
}

#[test]
fn native_progress_cancelled_cleanup_fences_the_suspended_owner() -> anyhow::Result<()> {
    let binding = ProcessingBinding::new(
        Arc::new(InMemoryComputationProvider),
        "cancelled-stop".into(),
        ComponentId::try_new(super::PROCESSOR)?,
    )
    .with_committed_input_progress(Some(PortId::try_new("in")?))?;
    let reader = binding.progress_owner().context("progress owner")?;
    let cleanup = binding.cleanup_call()?.context("cleanup guard")?;
    binding.suspend_progress()?;
    drop(cleanup);
    assert!(reader.snapshot().failure.is_some());
    let retry = binding.cleanup_call()?.context("cleanup retry guard")?;
    binding.suspend_progress()?;
    retry.complete();
    assert!(
        reader.snapshot().failure.is_some(),
        "only confirmed resource cleanup may reset failed progress"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_volatile_processing_preserves_actual_indexes_without_claiming_atomicity(
) -> anyhow::Result<()> {
    let mut binding = ProcessingBinding::new(
        Arc::new(InMemoryComputationProvider),
        "volatile".into(),
        ComponentId::try_new(super::PROCESSOR)?,
    )
    .with_volatile_processing(true)
    .with_committed_input_progress(Some(PortId::try_new("in")?))?;
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    binding.open().await?;
    let rejected: anyhow::Result<()> = service
        .run(|context| async move {
            context.atomic_state_info().await?;
            Ok(())
        })
        .await;
    assert!(rejected.is_err());
    binding.close().await?;
    binding.open().await?;
    let bootstrap = binding.bootstrap_state();
    bootstrap.write(b"volatile-intent".to_vec().into()).await?;
    service
        .run_output(|context| async move {
            let info = context.state_info().await?;
            assert!(info.volatile && !info.atomic && !info.persistent_checkpoints);
            context.stage_bootstrap_complete(true).await?;
            context.recover_input_progress().await?;
            context.set_element(&element("retained"), &[0]).await?;
            context
                .stage_input_checkpoint(&StreamId::try_new("source/out")?, 7, Some(b"position"))
                .await?;
            Ok(())
        })
        .await?;
    binding.progress_ready()?;
    let previous = binding
        .committed_input_progress()?
        .context("volatile progress view")?;
    assert!(!previous.snapshot()?.persistent);
    binding.close().await?;
    assert!(previous.snapshot().is_err());
    binding.open().await?;
    service
        .run(|context| async move {
            context.recover_input_progress().await?;
            assert!(context
                .get_element(&ElementReference::new("source", "retained"))
                .await?
                .is_some());
            assert_eq!(
                context
                    .read_input_checkpoint(&StreamId::try_new("source/out")?)
                    .await?
                    .context("retained progress")?
                    .sequence,
                7
            );
            assert_eq!(context.reset_volatile_state(true).await?, 1);
            assert!(context
                .get_element(&ElementReference::new("source", "retained"))
                .await?
                .is_none());
            assert!(context
                .read_input_checkpoint(&StreamId::try_new("source/out")?)
                .await?
                .is_none());
            context.stage_bootstrap_complete(true).await?;
            context.recover_input_progress().await?;
            Ok(())
        })
        .await?;
    assert_eq!(
        bootstrap.read().await?.as_deref(),
        Some(b"volatile-intent".as_slice())
    );
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_progress_requires_prepared_input_to_be_staged(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut binding = ProcessingBinding::new(
        super::super::consumer_tests::provider(directory.path())?,
        "native-consumer".into(),
        ComponentId::try_new(super::PROCESSOR)?,
    )
    .with_committed_input_progress(Some(PortId::try_new("in")?))?;
    binding.open().await?;
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    service
        .run(|context| async move {
            context.stage_bootstrap_complete(true).await?;
            context.recover_input_progress().await?;
            Ok(())
        })
        .await?;
    let failure: anyhow::Result<()> = service
        .run(|context| async move {
            let changed = element("uncommitted");
            let input = InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: GraphChangeCodec::encode_change(
                    drasi_core::models::SourceChange::Insert {
                        element: changed.clone(),
                    },
                    StreamId::try_new("source/out")?,
                    1,
                    None,
                )?,
            };
            assert!(!context.prepare_input_progress(&input).await?.duplicate);
            context.set_element(&changed, &[0]).await?;
            Ok(())
        })
        .await;
    assert!(
        format!("{:#}", failure.expect_err("missing staging must roll back"))
            .contains("not staged")
    );
    binding.close().await?;
    binding.open().await?;
    service
        .run(|context| async move {
            context.recover_input_progress().await?;
            assert!(context
                .get_element(&ElementReference::new("source", "uncommitted"))
                .await?
                .is_none());
            assert!(context
                .read_input_checkpoint(&StreamId::try_new("source/out")?)
                .await?
                .is_none());
            Ok(())
        })
        .await?;
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_bootstrap_reset_is_atomic_and_preserves_intent(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut binding = ProcessingBinding::new(
        super::super::consumer_tests::provider(directory.path())?,
        "native-consumer".into(),
        ComponentId::try_new(super::PROCESSOR)?,
    )
    .with_committed_input_progress(Some(PortId::try_new("in")?))?;
    binding.open().await?;
    let state = binding.bootstrap_state();
    state
        .write(b"owned-provider-intent".to_vec().into())
        .await?;
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    service
        .run(|context| async move {
            context.stage_bootstrap_complete(false).await?;
            context.recover_input_progress().await?;
            context
                .stage_input_checkpoint(&StreamId::try_new("source/out")?, 0, Some(b"source-zero"))
                .await?;
            for id in 0..520 {
                context
                    .set_element(&element(&format!("row-{id}")), &[0])
                    .await?;
            }
            let rows: Vec<_> = (0..520)
                .map(|id| drasi_core::interface::RowMutation {
                    row_signature: id,
                    data: Some(b"row".as_slice()),
                })
                .collect();
            context.apply_live_rows(&rows).await?;
            context
                .stage_checkpoint("other-state", 12, Some(b"state"))
                .await?;
            context.stage_result_sequence(9).await?;
            context.append_outbox_and_trim(9, b"output", 9).await?;
            Ok(())
        })
        .await?;
    let rollback: anyhow::Result<()> = service
        .run(|context| async move {
            assert_eq!(context.reset_state(true).await?, 1);
            assert!(context
                .get_element(&ElementReference::new("source", "row-519"))
                .await?
                .is_none());
            assert!(context.read_checkpoint("other-state").await?.is_none());
            context.stage_bootstrap_complete(false).await?;
            context.recover_input_progress().await?;
            anyhow::bail!("rollback complete state reset")
        })
        .await;
    assert!(rollback.is_err());
    binding.close().await?;
    binding.open().await?;
    service
        .run(|context| async move {
            context.recover_input_progress().await?;
            assert!(context
                .get_element(&ElementReference::new("source", "row-519"))
                .await?
                .is_some());
            assert_eq!(
                context
                    .read_checkpoint("other-state")
                    .await?
                    .unwrap()
                    .sequence,
                12
            );
            assert_eq!(context.read_result_sequence().await?, Some(9));
            assert_eq!(
                context
                    .read_live_rows_page(
                        None,
                        OutboxPageLimits {
                            max_records: std::num::NonZeroUsize::new(1024).unwrap(),
                            max_bytes: std::num::NonZeroUsize::new(1 << 20).unwrap(),
                        }
                    )
                    .await?
                    .len(),
                520
            );
            Ok(())
        })
        .await?;
    service
        .run(|context| async move {
            assert_eq!(context.reset_state(true).await?, 1);
            context.stage_bootstrap_complete(false).await?;
            context.recover_input_progress().await?;
            Ok(())
        })
        .await?;
    let snapshot = binding.committed_input_progress()?.unwrap().snapshot()?;
    assert!(snapshot.recovered && !snapshot.bootstrap_complete && snapshot.checkpoints.is_empty());
    assert_eq!(snapshot.reset_generation, 1);
    assert_eq!(
        state.read().await?.as_deref(),
        Some(b"owned-provider-intent".as_slice())
    );
    assert!(binding.progress_ready().is_err());
    service
        .run(|context| async move {
            assert!(context
                .get_element(&ElementReference::new("source", "row-0"))
                .await?
                .is_none());
            assert_eq!(context.read_result_sequence().await?, Some(0));
            let limits = OutboxPageLimits {
                max_records: std::num::NonZeroUsize::new(1024).unwrap(),
                max_bytes: std::num::NonZeroUsize::new(1 << 20).unwrap(),
            };
            assert!(context.read_outbox_page(0, limits).await?.is_empty());
            assert!(context.read_live_rows_page(None, limits).await?.is_empty());
            assert_eq!(context.reset_state(false).await?, 2);
            Ok(())
        })
        .await?;
    assert!(state.read().await?.is_none());
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_bootstrap_state_uses_its_actual_atomic_owner(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut binding = ProcessingBinding::new(
        super::super::consumer_tests::provider(directory.path())?,
        "native-consumer".into(),
        ComponentId::try_new(super::PROCESSOR)?,
    )
    .with_committed_input_progress(Some(PortId::try_new("in")?))?;
    let state = binding.bootstrap_state();
    assert!(state.read().await.is_err());
    binding.open().await?;
    state.durability().require(FailureMode::ProcessRestart)?;
    assert!(state.read().await?.is_none());
    state
        .write(b"prepared-real-provider".to_vec().into())
        .await?;
    assert_eq!(
        state.read().await?.as_deref(),
        Some(b"prepared-real-provider".as_slice())
    );
    assert!(state.write(Vec::new().into()).await.is_err());
    assert!(state
        .write(vec![0; drasi_computation_contracts::MAX_BOOTSTRAP_STATE_BYTES + 1].into())
        .await
        .is_err());
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    service
        .run(|context| async move {
            let stored = context
                .read_checkpoint(drasi_computation_contracts::BOOTSTRAP_STATE_CHECKPOINT)
                .await?
                .expect("actual owner metadata");
            assert_eq!(stored.sequence, 1);
            assert_eq!(
                stored.source_position.as_deref(),
                Some(b"prepared-real-provider".as_slice())
            );
            context.recover_input_progress().await?;
            Ok(())
        })
        .await?;
    let local = binding
        .progress_owner()
        .expect("actual local progress owner");
    service
        .run(|context| async move {
            context
                .stage_checkpoint(
                    drasi_computation_contracts::BOOTSTRAP_STATE_CHECKPOINT,
                    2,
                    Some(b"bad-version"),
                )
                .await?;
            Ok(())
        })
        .await?;
    assert!(
        state.read().await.is_err(),
        "damaged bootstrap ownership must not read as absent"
    );
    state
        .write(b"prepared-real-provider".to_vec().into())
        .await?;
    binding.progress_ready()?;
    assert!(local.snapshot().ready && local.snapshot().persistent);
    binding.close().await?;
    assert!(state.read().await.is_err());
    binding.open().await?;
    assert_eq!(
        state.read().await?.as_deref(),
        Some(b"prepared-real-provider".as_slice())
    );
    binding.close().await?;
    drop(binding);
    assert!(state.read().await.is_err());
    Ok(())
}

fn checkpoint(view: &SourceProgressView, sequence: u64) -> anyhow::Result<()> {
    let snapshot = view.snapshot()?;
    let stream = StreamId::try_new("source/out")?;
    let saved = snapshot
        .checkpoints
        .get(&SourceProgressKey::Stream(stream.clone()))
        .ok_or_else(|| anyhow::anyhow!("committed input missing"))?;
    assert_eq!(saved.sequence, sequence);
    assert_eq!(
        saved.source_position.as_deref(),
        Some(b"\0zero-position".as_slice())
    );
    assert_eq!(snapshot.transport_sequences[&stream], sequence);
    Ok(())
}

async fn publication_case() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut binding = ProcessingBinding::new(
        super::super::consumer_tests::provider(directory.path())?,
        "native-consumer".into(),
        ComponentId::try_new(super::PROCESSOR)?,
    )
    .with_committed_input_progress(Some(PortId::try_new("in")?))?;
    let view = binding
        .committed_input_progress()?
        .expect("progress capability");
    assert!(view.same_owner(
        &binding
            .committed_input_progress()?
            .expect("same capability")
    ));
    assert!(!view.snapshot()?.recovered);
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    binding.open().await?;
    service
        .run(|context| async move {
            context.recover_input_progress().await?;
            Ok(())
        })
        .await?;
    let recovered = view.wait_recovered().await?;
    assert!(recovered.persistent && recovered.recovered);
    assert!(!recovered.ready && !recovered.admitting && !recovered.bootstrap_complete);
    assert!(
        recovered.checkpoints.is_empty(),
        "no fabricated sequence-zero checkpoint"
    );
    let mut updates = view.subscribe()?;
    assert!(!updates.snapshot()?.ready);
    let (entered, staged) = tokio::sync::oneshot::channel();
    let release = Arc::new(Notify::new());
    let resume = release.clone();
    let work = service.run(move |context| async move {
        context.set_element(&element("committed"), &[0]).await?;
        context
            .stage_input_checkpoint(
                &StreamId::try_new("source/out")?,
                0,
                Some(b"\0zero-position"),
            )
            .await?;
        entered
            .send(())
            .map_err(|_| anyhow::anyhow!("lost staged-work observer"))?;
        resume.notified().await;
        Ok(())
    });
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => anyhow::bail!("operation completed before release: {result:?}"),
        result = staged => result?,
    }
    assert!(
        view.snapshot()?.checkpoints.is_empty(),
        "staged progress became visible before commit"
    );
    binding.progress_ready()?;
    assert!(updates.snapshot()?.ready);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), updates.changed())
            .await
            .is_err()
    );
    release.notify_one();
    work.await?;
    tokio::time::timeout(Duration::from_secs(2), updates.changed()).await??;
    checkpoint(&view, 0)?;
    assert!(updates.snapshot()?.ready);
    let failure: anyhow::Result<()> = service
        .run(|context| async move {
            context.set_element(&element("rolled-back"), &[0]).await?;
            context
                .stage_input_checkpoint(
                    &StreamId::try_new("source/out")?,
                    1,
                    Some(b"not-committed"),
                )
                .await?;
            anyhow::bail!("rollback staged input progress")
        })
        .await;
    assert!(failure.is_err());
    checkpoint(&view, 0)?;
    let failed = view.snapshot()?;
    assert!(!failed.ready && !failed.admitting && failed.failure.is_some());
    binding.close().await?;
    assert!(view.snapshot().is_err());
    assert!(updates.snapshot().is_err());
    assert!(updates.changed().await.is_err());
    let replacement = binding
        .committed_input_progress()?
        .expect("replacement view");
    assert!(!view.same_owner(&replacement));
    assert!(!replacement.snapshot()?.recovered);
    binding.open().await?;
    service
        .run(|context| async move {
            context.recover_input_progress().await?;
            assert!(context
                .get_element(&ElementReference::new("source", "rolled-back"))
                .await?
                .is_none());
            assert!(context
                .get_element(&ElementReference::new("source", "committed"))
                .await?
                .is_some());
            Ok(())
        })
        .await?;
    checkpoint(&replacement, 0)?;
    assert!(!replacement.snapshot()?.ready);
    binding.progress_ready()?;
    assert!(replacement.wait_ready().await?.persistent);
    let external =
        drasi_lib::computation::v1::SourceProgressReader::External(Arc::new(replacement.clone()));
    assert!(
        external.local_owner().is_none(),
        "read-only view must not invent source-retirement authority"
    );
    let mut cancelled_updates = replacement.subscribe()?;
    cancelled_updates.snapshot()?;
    {
        let (entered, staged) = tokio::sync::oneshot::channel();
        let work = service.run(move |context| async move {
            context
                .stage_input_checkpoint(&StreamId::try_new("source/out")?, 1, Some(b"cancelled"))
                .await?;
            entered
                .send(())
                .map_err(|_| anyhow::anyhow!("cancelled operation lost its observer"))?;
            std::future::pending::<anyhow::Result<()>>().await
        });
        tokio::pin!(work);
        tokio::select! {
            result = &mut work => anyhow::bail!("cancelled operation finished early: {result:?}"),
            result = staged => result?,
        }
    }
    tokio::time::timeout(Duration::from_secs(2), cancelled_updates.changed()).await??;
    checkpoint(&replacement, 0)?;
    assert!(
        replacement.snapshot()?.failure.is_some(),
        "cancelled storage work must fence progress"
    );
    binding.close().await?;
    binding.open().await?;
    service
        .run(|context| async move {
            context.recover_input_progress().await?;
            let committed = context
                .read_input_checkpoint(&StreamId::try_new("source/out")?)
                .await?
                .expect("retained checkpoint");
            assert_eq!(
                committed.sequence, 0,
                "cancelled checkpoint must not survive restart"
            );
            Ok(())
        })
        .await?;
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_progress_is_recovered_and_published_only_after_commit_current_thread(
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(15), publication_case()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_indexed_processing_progress_is_recovered_and_published_only_after_commit_multi_thread(
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(15), publication_case()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn native_indexed_processing_progress_rejects_reserved_writes_and_oversized_positions(
) -> anyhow::Result<()> {
    for reserved in [false, true] {
        let directory = tempfile::tempdir()?;
        let mut binding = ProcessingBinding::new(
            super::super::consumer_tests::provider(directory.path())?,
            "native-consumer".into(),
            ComponentId::try_new(super::PROCESSOR)?,
        )
        .with_committed_input_progress(Some(PortId::try_new("in")?))?;
        binding.open().await?;
        let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
        service
            .run(|context| async move {
                context.recover_input_progress().await?;
                Ok(())
            })
            .await?;
        binding.progress_ready()?;
        let view = binding.committed_input_progress()?.expect("progress");
        let rejected = service
            .run(move |context| async move {
                context.set_element(&element("rejected"), &[0]).await?;
                if reserved {
                    assert!(context
                        .stage_checkpoint(
                            "\0computation:processing-input:v1:[\"in\",\"source/out\"]",
                            1,
                            None
                        )
                        .await
                        .is_err());
                } else {
                    let position =
                        vec![0; drasi_computation_contracts::MAX_SOURCE_POSITION_BYTES + 1];
                    assert!(context
                        .stage_input_checkpoint(
                            &StreamId::try_new("source/out")?,
                            1,
                            Some(&position)
                        )
                        .await
                        .is_err());
                }
                Ok(())
            })
            .await;
        assert!(rejected.is_err(), "ignored state failure must not commit");
        assert!(view.snapshot()?.checkpoints.is_empty());
        assert!(view.snapshot()?.failure.is_some());
        binding.close().await?;
        binding.open().await?;
        service
            .run(|context| async move {
                context.recover_input_progress().await?;
                assert!(context
                    .get_element(&ElementReference::new("source", "rejected"))
                    .await?
                    .is_none());
                Ok(())
            })
            .await?;
        binding.close().await?;
    }
    Ok(())
}
