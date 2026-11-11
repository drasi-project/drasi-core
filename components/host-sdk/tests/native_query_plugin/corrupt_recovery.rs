// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::retained_recovery::{deliver, replay, sink};
use super::*;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationTransaction},
    interface::RowMutation,
};
use drasi_host_sdk::computation::NativeBootstrapResource;

#[derive(Clone, Copy, Debug)]
enum Damage {
    MissingRow,
    InvalidRow,
    InvalidOutbox,
    MissingOutbox,
    ForeignOutputAndInvalidRow,
}

fn query(
    database: &Path,
    provider: Arc<dyn ComputationIndexProvider>,
    generation: u64,
    non_atomic: bool,
    recovery: &str,
) -> Result<NativeComponentProxy> {
    let mut query = factory()?.create_component_with_processing_publication(
        ComponentId::try_new(COMPONENT)?,
        json!({
            "query": super::bootstrap::TEXT, "bootstrap": true, "outbox_capacity": 4,
            "recovery": recovery, "publication": if non_atomic { "non_atomic" } else { "atomic" },
        }),
        scope(generation),
        provider,
        None,
        if non_atomic {
            QueryPublicationMode::NonAtomic
        } else {
            QueryPublicationMode::Atomic
        },
    )?;
    query.bind_bootstrap(Arc::new(NativeBootstrapResource(
        super::bootstrap::file_provider(database, generation)?,
    )))?;
    Ok(query)
}

fn live(sequence: u64, amount: i64) -> Result<InputEnvelope> {
    input(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &format!("live-{sequence}")),
                    labels: Arc::from([Arc::from("items")]),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::from(json!({"amount": amount})),
            },
        },
        sequence,
    )
}

fn snapshot_reader(query: &NativeComponentProxy) -> Result<Arc<dyn SnapshotProvider>> {
    query
        .output_readers()
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|port| port.snapshot())
        .cloned()
        .context("query snapshot")
}

async fn corrupted_state_recovery() -> Result<()> {
    for non_atomic in [false, true] {
        for damage in [
            Damage::MissingRow,
            Damage::InvalidRow,
            Damage::InvalidOutbox,
            Damage::MissingOutbox,
            Damage::ForeignOutputAndInvalidRow,
        ] {
            let directory = tempfile::tempdir()?;
            let database = directory.path().join("source.sqlite");
            crate::bootstrap::provider::create_database(&database)?;
            let (provider, primary, outbox): (Arc<dyn ComputationIndexProvider>, _, _) =
                if non_atomic {
                    let provider = Arc::new(super::non_atomic_store::SplitOutputProvider::new(
                        &directory.path().join("state"),
                    )?);
                    let (primary, outbox) = provider.storage_providers();
                    (provider, primary, outbox)
                } else {
                    let provider = provider(&directory.path().join("state"))?;
                    (provider.clone(), provider.clone(), provider)
                };
            let mut original = query(&database, provider.clone(), 1, non_atomic, "strict")?;
            original.start().await?;
            let mut output = Vec::new();
            for sequence in 1..=3 {
                let emitted = original.transform(live(sequence, 7)?).await?;
                original.delivery_completed(&emitted).await?;
                output.extend(emitted);
            }
            let snapshot = snapshot_reader(&original)?.snapshot().await?;
            assert_eq!(super::bootstrap::total(&snapshot)?, 51.0);
            let signature = QueryChangeCodec::decode_row(
                snapshot.records().values().next().context("stored row")?,
            )?
            .signature
            .context("row signature")?;
            let original_reader = original
                .query_api()
                .context("query API")?
                .recovery_reader()
                .context("recovery reader")?;
            let initial = original_reader.recovery_head().await?;
            let progress = Arc::new(MemoryConsumerProgress::default());
            let mut destination = sink(progress.clone())?;
            let mut consumer = replay(
                original_reader,
                progress.clone(),
                ConsumerRecoveryPolicy::AutoReset,
            )?;
            consumer.start().await?;
            deliver(&mut destination, consumer.on_wakeup().await?).await?;
            original.stop().await?;
            drop(original);

            let owner =
                ComputationTransaction::try_new(primary.create_indexes(GRAPH, COMPONENT).await?)?;
            let codec = drasi_computation_plugin_sdk::wire::codec(&[GraphChangeCodec::schema()])?;
            let mut foreign = output
                .last()
                .context("last committed output")?
                .envelope
                .clone();
            let foreign_identity = initial
                .identity
                .clone()
                .with_construction_scope("foreign-instance")?;
            foreign_identity.annotate(&mut foreign, &ComponentId::try_new(COMPONENT)?)?;
            assert_eq!(
                QueryRecoveryIdentity::from_envelope(&foreign)?,
                foreign_identity
            );
            let foreign = codec.encode(&foreign)?;
            owner
                .run(async {
                    match damage {
                        Damage::MissingRow
                        | Damage::InvalidRow
                        | Damage::ForeignOutputAndInvalidRow => {
                            owner
                                .resources()
                                .live_results_writer()
                                .expect("stored rows")
                                .apply_mutations(
                                    COMPONENT,
                                    &[RowMutation {
                                        row_signature: signature,
                                        data: if matches!(damage, Damage::MissingRow) {
                                            None
                                        } else {
                                            Some(b"invalid row")
                                        },
                                    }],
                                )
                                .await?;
                        }
                        _ => {}
                    }
                    Ok(())
                })
                .await?;
            owner.shutdown().await?;
            drop(owner);
            let owner =
                ComputationTransaction::try_new(outbox.create_indexes(GRAPH, COMPONENT).await?)?;
            owner
                .run(async {
                    match damage {
                        Damage::InvalidOutbox => {
                            owner
                                .resources()
                                .outbox_writer()
                                .expect("outbox")
                                .append(COMPONENT, initial.sequence, b"invalid output")
                                .await?
                        }
                        Damage::MissingOutbox => {
                            owner
                                .resources()
                                .outbox_writer()
                                .expect("outbox")
                                .clear(COMPONENT)
                                .await?
                        }
                        Damage::ForeignOutputAndInvalidRow => {
                            owner
                                .resources()
                                .outbox_writer()
                                .expect("outbox")
                                .append(COMPONENT, initial.sequence, &foreign)
                                .await?
                        }
                        _ => {}
                    }
                    Ok(())
                })
                .await?;
            owner.shutdown().await?;
            drop(owner);

            let mut strict = query(&database, provider.clone(), 2, non_atomic, "strict")?;
            assert!(strict.start().await.is_err(), "Strict adopted {damage:?}");
            assert!(snapshot_reader(&strict)?.snapshot().await.is_err());
            strict.stop().await?;
            drop(strict);
            let mut repaired = query(&database, provider.clone(), 3, non_atomic, "auto_reset")?;
            if matches!(damage, Damage::ForeignOutputAndInvalidRow) {
                let error = repaired
                    .start()
                    .await
                    .expect_err("foreign state must not be reset");
                assert!(format!("{error:#}").contains("different producer"));
                repaired.stop().await?;
                drop(repaired);
                let owner = ComputationTransaction::try_new(
                    primary.create_indexes(GRAPH, COMPONENT).await?,
                )?;
                assert_eq!(
                    owner
                        .resources()
                        .checkpoint_store()
                        .context("checkpoints")?
                        .read_result_sequence(COMPONENT)
                        .await?,
                    Some(initial.sequence)
                );
                owner.shutdown().await?;
                drop(owner);
                let owner = ComputationTransaction::try_new(
                    outbox.create_indexes(GRAPH, COMPONENT).await?,
                )?;
                let retained = owner
                    .resources()
                    .outbox_writer()
                    .context("outbox")?
                    .read_from(COMPONENT, 0)
                    .await?;
                assert_eq!(retained.len(), 3);
                assert_eq!(retained.last(), Some(&(initial.sequence, foreign)));
                owner.shutdown().await?;
                continue;
            }
            repaired
                .start()
                .await
                .with_context(|| format!("AutoReset of {damage:?}"))?;
            let snapshot = snapshot_reader(&repaired)?.snapshot().await?;
            assert_eq!(super::bootstrap::total(&snapshot)?, 30.0);
            let reader = repaired
                .query_api()
                .context("repaired API")?
                .recovery_reader()
                .context("repaired reader")?;
            let reset = reader.recovery_head().await?;
            assert_eq!(reset.identity, initial.identity);
            assert_eq!(reset.generation, 1);
            assert_eq!(
                reset.sequence, initial.sequence,
                "reset cannot regress consumer progress"
            );
            let mut consumer = replay(reader, progress.clone(), ConsumerRecoveryPolicy::AutoReset)?;
            consumer.start().await?;
            let reset_output = consumer.on_wakeup().await?;
            assert!(QueryChangeCodec::is_snapshot(&reset_output[0].envelope));
            deliver(&mut destination, reset_output).await?;
            assert_eq!(
                progress
                    .load(COMPONENT)
                    .await?
                    .context("handled reset")?
                    .generation,
                1
            );
            repaired.stop().await?;
            drop(repaired);

            let mut reopened = query(&database, provider.clone(), 4, non_atomic, "strict")?;
            reopened.start().await?;
            assert_eq!(
                snapshot_reader(&reopened)?
                    .snapshot()
                    .await?
                    .cursor()
                    .sequence(),
                initial.sequence
            );
            let output = reopened.transform(live(4, 9)?).await?;
            assert_eq!(QueryChangeCodec::query_sequence(&output[0].envelope)?, 4);
            assert_eq!(
                super::bootstrap::total(&snapshot_reader(&reopened)?.snapshot().await?)?,
                39.0
            );
            reopened.delivery_completed(&output).await?;
            reopened.stop().await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_auto_reset_corrupt_state_preserves_ownership_and_consumer_progress_current_thread(
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(90), corrupted_state_recovery()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_auto_reset_corrupt_state_preserves_ownership_and_consumer_progress_multi_thread(
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(90), corrupted_state_recovery()).await?
}
