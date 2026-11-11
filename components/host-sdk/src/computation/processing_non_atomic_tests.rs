// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::Context;
use drasi_computation_contracts::{SourceProgressKey, SourceProgressView};
use drasi_query_common::{QueryPublicationCheckpoint, PENDING_QUERY_OUTPUT};

#[tokio::test(flavor = "current_thread")]
async fn reset_baselines_require_one_actual_bootstrap_reset_and_cannot_acknowledge_live_output(
) -> anyhow::Result<()> {
    for non_atomic in [false, true] {
        let directory = tempfile::tempdir()?;
        let mut binding = ProcessingBinding::new(
            super::super::consumer_tests::provider(directory.path())?,
            "native-consumer".into(),
            "processor".into(),
        )
        .with_publication(
            if non_atomic {
                QueryPublicationMode::NonAtomic
            } else {
                QueryPublicationMode::Atomic
            },
            non_atomic,
        )?
        .with_committed_input_progress(Some(PortId::try_new("in")?))?
        .with_durable_output(
            true,
            Arc::new(sdk::wire::codec(&[GraphChangeCodec::schema()])?),
        )?;
        let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
        binding.open().await?;
        assert!(service
            .run(|context| async move {
                context.initialize_reset_output(37).await?;
                Ok(())
            })
            .await
            .is_err());
        binding.close().await?;
        binding.open().await?;
        if non_atomic {
            service
                .run(|context| async move {
                    context.begin_non_atomic_reset(true).await?;
                    Ok(())
                })
                .await?;
        }
        service
            .run(move |context| async move {
                if non_atomic {
                    context.reset_non_atomic_state(true).await?;
                } else {
                    context.reset_state(true).await?;
                }
                context.initialize_reset_output(37).await?;
                context.stage_bootstrap_complete(true).await?;
                context.recover_input_progress().await?;
                Ok(())
            })
            .await?;
        if non_atomic {
            service
                .run(|context| async move {
                    context.finish_non_atomic_reset().await?;
                    Ok(())
                })
                .await?;
        }
        binding.progress_ready()?;
        assert!(service
            .run(|context| async move {
                context.initialize_reset_output(99).await?;
                Ok(())
            })
            .await
            .is_err());
        binding.close().await?;
        binding.open().await?;
        service
            .run(|context| async move {
                assert_eq!(context.read_result_sequence().await?, Some(37));
                assert_eq!(context.read_delivery_progress().await?, Some(37));
                Ok(())
            })
            .await?;
        if non_atomic {
            service
                .run(|context| async move {
                    context.begin_non_atomic_reset(true).await?;
                    Ok(())
                })
                .await?;
        }
        assert!(service
            .run(move |context| async move {
                if non_atomic {
                    context.reset_non_atomic_state(true).await?;
                } else {
                    context.reset_state(true).await?;
                }
                context.initialize_reset_output(37).await?;
                context.initialize_reset_output(38).await?;
                Ok(())
            })
            .await
            .is_err());
        binding.close().await?;
        binding.open().await?;
        if non_atomic {
            service
                .run(|context| async move {
                    context.begin_non_atomic_reset(false).await?;
                    Ok(())
                })
                .await?;
        }
        assert!(service
            .run(move |context| async move {
                if non_atomic {
                    context.reset_non_atomic_state(false).await?;
                } else {
                    context.reset_state(false).await?;
                }
                context.initialize_reset_output(37).await?;
                Ok(())
            })
            .await
            .is_err());
        binding.close().await?;
    }
    Ok(())
}

async fn marker(
    context: &sdk::NativeProcessingContext,
    state: QueryPublicationCheckpoint,
) -> anyhow::Result<()> {
    let checkpoint = state.checkpoint();
    context
        .stage_checkpoint(
            PENDING_QUERY_OUTPUT,
            checkpoint.sequence,
            checkpoint.source_position.as_deref(),
        )
        .await?;
    Ok(())
}

async fn initialized(
    path: &std::path::Path,
) -> anyhow::Result<(ProcessingBinding, sdk::NativeProcessing, SourceProgressView)> {
    let mut binding = ProcessingBinding::new(
        super::super::consumer_tests::provider(path)?,
        "native-consumer".into(),
        "processor".into(),
    )
    .with_publication(QueryPublicationMode::NonAtomic, true)?
    .with_committed_input_progress(Some(PortId::try_new("in")?))?
    .with_durable_output(
        true,
        Arc::new(sdk::wire::codec(&[GraphChangeCodec::schema()])?),
    )?;
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    binding.open().await?;
    service
        .run(|context| async move {
            let info = context.state_info().await?;
            assert!(info.non_atomic_publication && !info.volatile && info.persistent_checkpoints);
            context.stage_result_sequence(0).await?;
            context.stage_delivery_progress(0).await?;
            context.stage_bootstrap_complete(true).await?;
            context.recover_input_progress().await?;
            Ok(())
        })
        .await?;
    binding.progress_ready()?;
    let view = binding
        .committed_input_progress()?
        .context("actual progress view")?;
    Ok((binding, service, view))
}

async fn evaluate(service: &sdk::NativeProcessing) -> anyhow::Result<()> {
    service
        .run(
            |context| async move { marker(&context, QueryPublicationCheckpoint::Evaluating).await },
        )
        .await?;
    service
        .run_deferred_progress(|context| async move {
            context.set_element(&element("evaluated"), &[0]).await?;
            context
                .stage_input_checkpoint(
                    &StreamId::try_new("source/out")?,
                    1,
                    Some(b"source-position"),
                )
                .await?;
            Ok(())
        })
        .await
}

async fn publication_case() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut binding, service, view) = initialized(directory.path()).await?;
    evaluate(&service).await?;
    assert!(view.snapshot()?.checkpoints.is_empty());
    let observed = view.clone();
    service
        .run_output(move |context| async move {
            assert!(context
                .get_element(&ElementReference::new("source", "evaluated"))
                .await?
                .is_some());
            assert_eq!(
                context
                    .read_input_checkpoint(&StreamId::try_new("source/out")?)
                    .await?
                    .context("committed evaluation checkpoint")?
                    .sequence,
                1,
            );
            context.resume_input_progress().await?;
            marker(&context, QueryPublicationCheckpoint::Complete).await?;
            assert!(
                observed.snapshot()?.checkpoints.is_empty(),
                "completion has not committed"
            );
            Ok(())
        })
        .await?;
    let snapshot = view.snapshot()?;
    let checkpoint = snapshot
        .checkpoints
        .get(&SourceProgressKey::Stream(StreamId::try_new("source/out")?))
        .context("published checkpoint")?;
    assert_eq!(checkpoint.sequence, 1);
    assert_eq!(
        checkpoint.source_position.as_deref(),
        Some(b"source-position".as_slice())
    );
    assert!(snapshot.ready && snapshot.persistent);
    service
        .run(|context| async move {
            assert!(!QueryPublicationCheckpoint::is_incomplete(
                context
                    .read_checkpoint(PENDING_QUERY_OUTPUT)
                    .await?
                    .as_ref(),
            ));
            assert_eq!(
                context.read_result_sequence().await?,
                Some(0),
                "no-op input must not invent output"
            );
            Ok(())
        })
        .await?;
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn non_atomic_progress_waits_for_completed_output_current_thread() -> anyhow::Result<()> {
    publication_case().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_atomic_progress_waits_for_completed_output_multi_thread() -> anyhow::Result<()> {
    publication_case().await
}

#[tokio::test(flavor = "current_thread")]
async fn non_atomic_failed_or_unfinished_publication_keeps_input_invisible() -> anyhow::Result<()> {
    for resume in [false, true] {
        let directory = tempfile::tempdir()?;
        let (mut binding, service, view) = initialized(directory.path()).await?;
        evaluate(&service).await?;
        let failure: anyhow::Result<()> = service
            .run_output(move |context| async move {
                if resume {
                    context.resume_input_progress().await?;
                    marker(&context, QueryPublicationCheckpoint::Complete).await?;
                    anyhow::bail!("publication failed before commit");
                }
                Ok(())
            })
            .await;
        assert!(failure.is_err());
        assert!(view.snapshot()?.checkpoints.is_empty());
        assert!(view.snapshot()?.failure.is_some());
        binding.close().await?;
        binding.open().await?;
        service
            .run(|context| async move {
                assert!(QueryPublicationCheckpoint::is_incomplete(
                    context
                        .read_checkpoint(PENDING_QUERY_OUTPUT)
                        .await?
                        .as_ref(),
                ));
                assert_eq!(
                    context
                        .read_input_checkpoint(&StreamId::try_new("source/out")?)
                        .await?
                        .context("retained evaluation checkpoint")?
                        .sequence,
                    1
                );
                Ok(())
            })
            .await?;
        binding.close().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn non_atomic_deferred_work_requires_explicit_negotiation() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut binding = ProcessingBinding::new(
        super::super::consumer_tests::provider(directory.path())?,
        "native-consumer".into(),
        "processor".into(),
    )
    .with_committed_input_progress(Some(PortId::try_new("in")?))?;
    binding.open().await?;
    let service = unsafe { sdk::NativeProcessing::from_borrowed(&binding.table())? };
    let result: anyhow::Result<()> = service
        .run_deferred_progress(|_| async {
            panic!("unnegotiated deferred work must not execute");
        })
        .await;
    assert!(result.is_err());
    binding.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn non_atomic_reset_retains_intent_through_failure_and_completes_after_bootstrap(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let (mut binding, service, _) = initialized(directory.path()).await?;
    binding
        .bootstrap_state()
        .write(b"provider-intent".to_vec().into())
        .await?;
    service
        .run(|context| async move {
            context.set_element(&element("before-reset"), &[0]).await?;
            context.stage_checkpoint("retained", 7, None).await?;
            Ok(())
        })
        .await?;
    binding.close().await?;
    binding.open().await?;
    service
        .run(|context| async move {
            assert_eq!(context.begin_non_atomic_reset(true).await?, 1);
            Ok(())
        })
        .await?;
    let interrupted: anyhow::Result<()> = service
        .run(|context| async move {
            assert_eq!(context.reset_non_atomic_state(true).await?, 1);
            anyhow::bail!("reset failed after staging checkpoint clearing");
        })
        .await;
    assert!(interrupted.is_err());
    binding.close().await?;
    binding.open().await?;
    service
        .run(|context| async move {
            let reset = context
                .state_info()
                .await?
                .non_atomic_reset
                .context("retained reset intent")?;
            assert!(reset.in_progress);
            assert_eq!(reset.generation, 1);
            assert_eq!(
                context
                    .read_checkpoint("retained")
                    .await?
                    .context("rolled-back checkpoint")?
                    .sequence,
                7
            );
            assert_eq!(context.begin_non_atomic_reset(true).await?, 2);
            Ok(())
        })
        .await?;
    service
        .run(|context| async move {
            assert_eq!(context.reset_non_atomic_state(true).await?, 2);
            context.stage_delivery_progress(0).await?;
            context.stage_bootstrap_complete(false).await?;
            Ok(())
        })
        .await?;
    assert_eq!(
        binding.bootstrap_state().read().await?.as_deref(),
        Some(b"provider-intent".as_slice()),
    );
    service
        .run(|context| async move {
            assert!(context
                .get_element(&ElementReference::new("source", "before-reset"))
                .await?
                .is_none());
            assert!(context.read_checkpoint("retained").await?.is_none());
            assert!(
                context
                    .state_info()
                    .await?
                    .non_atomic_reset
                    .context("reset intent")?
                    .in_progress
            );
            context.stage_bootstrap_complete(true).await?;
            context.recover_input_progress().await?;
            context.finish_non_atomic_reset().await?;
            Ok(())
        })
        .await?;
    binding.progress_ready()?;
    service
        .run(|context| async move {
            let reset = context
                .state_info()
                .await?
                .non_atomic_reset
                .context("completed reset")?;
            assert!(!reset.in_progress);
            assert_eq!(reset.generation, 2);
            assert_eq!(context.reset_generation().await?, Some(2));
            Ok(())
        })
        .await?;
    binding.close().await?;
    binding.open().await?;
    service
        .run(|context| async move {
            assert_eq!(context.begin_non_atomic_reset(false).await?, 3);
            Ok(())
        })
        .await?;
    service
        .run(|context| async move {
            assert_eq!(context.reset_non_atomic_state(false).await?, 3);
            Ok(())
        })
        .await?;
    service
        .run(|context| async move {
            context.finish_non_atomic_reset().await?;
            Ok(())
        })
        .await?;
    assert!(binding.bootstrap_state().read().await?.is_none());
    binding.close().await?;
    Ok(())
}
