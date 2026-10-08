use super::*;

async fn open(
    path: &Path,
    definition: QosChannelDefinition,
    recovery: QosRecoveryOptions,
) -> Result<Arc<QosChannel>> {
    let provider =
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(path, false, false)));
    let indexes = provider.create_indexes("qos", "channel").await?;
    let codec =
        FactoryRegistry::standard().envelope_codec(NonZeroUsize::new(1 << 20).expect("limit"))?;
    Ok(
        QosChannel::persistent_with_recovery(definition, indexes, codec, "journal", recovery)
            .await?,
    )
}

async fn metadata(path: &Path) -> Result<Option<Bytes>> {
    let provider =
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(path, false, false)));
    let indexes = provider.create_indexes("qos", "channel").await?;
    let result = indexes
        .checkpoint_store()
        .expect("checkpoint")
        .read_checkpoint("journal")
        .await?
        .and_then(|checkpoint| checkpoint.source_position);
    drasi_core::computation::ComputationTransaction::try_new(indexes)?
        .shutdown()
        .await?;
    Ok(result)
}

#[tokio::test(flavor = "current_thread")]
async fn configured_recovery_preserves_receipts_and_rejects_changed_or_disabled_modes_without_writes(
) -> Result<()> {
    for options in [
        QosRecoveryOptions::Admission(admission_options()?),
        QosRecoveryOptions::Replay(replay::options()),
    ] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("configured");
        let definition = definition(true, RetentionPolicy::Backpressure, 2);
        let input = replay::admitted_output(&directory.path().join("source")).await?;
        let channel = open(&path, definition.clone(), options.clone()).await?;
        let journal = channel.output_journal_identity();
        let session = if matches!(options, QosRecoveryOptions::Admission(_)) {
            let session = channel
                .register_producer(ComponentId::try_new("client")?)
                .await?;
            channel.admit(&session, 1, &event(1)?).await?;
            Some(session)
        } else {
            channel.publish(&input).await?;
            assert!(journal.is_some());
            None
        };
        channel.shutdown().await?;
        drop(channel);
        let before = metadata(&path).await?;
        let mut changed = options.clone();
        match &mut changed {
            QosRecoveryOptions::Admission(options) => options.construction_scope = "foreign".into(),
            QosRecoveryOptions::Replay(options) => {
                options.receipt_capacity = NonZeroUsize::new(3).expect("window")
            }
            QosRecoveryOptions::Disabled => unreachable!(),
        }
        for rejected in [QosRecoveryOptions::Disabled, changed] {
            let mut weakened = definition.clone();
            weakened.subscribers.remove("slow");
            assert!(open(&path, weakened, rejected).await.is_err());
            assert_eq!(
                metadata(&path).await?,
                before,
                "rejected policy cannot retire subscribers or rewrite receipts"
            );
        }
        let restored = open(&path, definition.clone(), options).await?;
        assert_eq!(restored.output_journal_identity(), journal);
        if let Some(session) = session {
            assert!(restored.admission_receipt(&session, 1).await?.is_some());
            restored.admit(&session, 1, &event(1)?).await?;
        } else {
            restored.publish(&input).await?;
        }
        assert_eq!(restored.progress().await?.accepted, 1);
        assert_eq!(restored.progress().await?.processed["slow"], 0);
        handle_admitted(&restored, &definition, 1).await?;
        restored.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn tracked_configuration_establishes_only_an_empty_journal_boundary() -> Result<()> {
    for options in [
        QosRecoveryOptions::Admission(admission_options()?),
        QosRecoveryOptions::Replay(replay::options()),
    ] {
        for populated in [false, true] {
            let directory = tempfile::tempdir()?;
            let definition = definition(true, RetentionPolicy::Backpressure, 2);
            let channel = open(
                directory.path(),
                definition.clone(),
                QosRecoveryOptions::Disabled,
            )
            .await?;
            if populated {
                channel.publish(&event(1)?).await?;
            }
            channel.shutdown().await?;
            drop(channel);
            let before = metadata(directory.path()).await?;
            let result = open(directory.path(), definition.clone(), options.clone()).await;
            if populated {
                assert!(
                    result.is_err(),
                    "pre-boundary messages must not become tracked input"
                );
                assert_eq!(metadata(directory.path()).await?, before);
                let original =
                    open(directory.path(), definition, QosRecoveryOptions::Disabled).await?;
                assert_eq!(original.progress().await?.accepted, 1);
                original.shutdown().await?;
            } else {
                let channel = result?;
                channel.shutdown().await?;
                drop(channel);
                let reopened = open(directory.path(), definition, options.clone()).await?;
                assert_eq!(reopened.progress().await?.accepted, 0);
                reopened.shutdown().await?;
            }
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_configured_survival_does_not_initialize_channel_metadata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut options = admission_options()?;
    options.failure_scope = drasi_core::interface::FailureMode::StorageLoss;
    assert!(open(
        directory.path(),
        definition(true, RetentionPolicy::Backpressure, 2),
        QosRecoveryOptions::Admission(options)
    )
    .await
    .is_err());
    assert!(metadata(directory.path()).await?.is_none());
    Ok(())
}
