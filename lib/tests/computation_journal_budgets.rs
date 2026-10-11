// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[allow(dead_code)]
mod computation_support;

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc, time::Duration};

use anyhow::Result;
use computation_support::*;
use drasi_lib::computation::v1::*;
use futures::poll;

fn nonzero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("positive fixture limit")
}

fn size(envelope: &ChangeEnvelope) -> usize {
    BinaryEnvelopeCodec::encoded_size(envelope).expect("full binary size")
}

fn large(sequence: u64) -> ChangeEnvelope {
    let mut envelope = root("source", sequence, &[sequence as u16]);
    envelope
        .append_annotation(
            ContextEntry::try_new(
                component("context"),
                "payload",
                ContextValue::Bytes(Arc::from(vec![7; 4096])),
            )
            .expect("context"),
        )
        .expect("annotation");
    envelope
}

fn definition(durable: bool, policy: RetentionPolicy) -> QosChannelDefinition {
    QosChannelDefinition {
        stream: stream("source"),
        capacity: nonzero(8),
        durable,
        retention: policy,
        subscribers: BTreeMap::from([
            ("fast".into(), SubscriptionStart::Earliest),
            ("slow".into(), SubscriptionStart::Earliest),
        ]),
    }
}

fn subscribe(channel: &Arc<QosChannel>, subscriber: &str) -> Result<ProvidedPipe> {
    let resource = ResourceId::try_new("channel")?;
    Ok(channel
        .definition()
        .pipe(resource.clone(), subscriber)
        .create_with_resources(&BTreeMap::from([(resource, channel.resource())]))?)
}

async fn handle(receiver: &mut dyn EnvelopeReceiver, sequence: u64) -> Result<()> {
    let delivery = tokio::time::timeout(Duration::from_secs(5), receiver.receive())
        .await??
        .expect("pending delivery");
    assert_eq!(delivery.envelope().system().sequence(), sequence);
    delivery
        .into_parts()
        .1
        .expect("acknowledgement")
        .complete(HandlingOutcome::Handled)
        .await?;
    Ok(())
}

async fn retained_pressure(store: &dyn RetainedEnvelopeStore) -> Result<()> {
    let generation = store.acquire_generation()?;
    store.append(generation, &root("source", 1, &[1])).await?;
    store.append(generation, &root("source", 2, &[2])).await?;
    store.acknowledge(generation, 1).await?;
    assert!(matches!(
        store.append(generation, &large(3)).await,
        Err(PipeError::CapacityExhausted)
    ));
    assert_eq!(
        store
            .next(generation, 0)
            .await?
            .expect("no partial pruning")
            .position,
        1
    );
    store.acknowledge(generation, 2).await?;
    assert_eq!(store.append(generation, &large(3)).await?, 3);
    assert!(matches!(
        store.next(generation, 0).await,
        Err(PipeError::PositionUnavailable { oldest: 3, .. })
    ));
    assert!(matches!(
        store.append(generation, &root("source", 4, &[4])).await,
        Err(PipeError::CapacityExhausted)
    ));
    store.acknowledge(generation, 3).await?;
    assert_eq!(store.append(generation, &root("source", 4, &[4])).await?, 4);
    assert_eq!(
        store
            .next(generation, 3)
            .await?
            .expect("next event")
            .position,
        4
    );
    Ok(())
}

async fn qos_pressure(channel: &Arc<QosChannel>) -> Result<()> {
    let mut fast = subscribe(channel, "fast")?;
    let mut slow = subscribe(channel, "slow")?;
    let mut fast_receiver = fast.pipe.take_receiver()?;
    let mut slow_receiver = slow.pipe.take_receiver()?;
    channel.publish(&root("source", 1, &[1])).await?;
    channel.publish(&root("source", 2, &[2])).await?;
    let oversized = large(3);
    for _ in 0..128 {
        let mut cancelled = Box::pin(channel.publish(&oversized));
        assert!(poll!(&mut cancelled).is_pending());
    }
    let mut blocked = Box::pin(channel.publish(&oversized));
    assert!(poll!(&mut blocked).is_pending());
    handle(fast_receiver.as_mut(), 1).await?;
    handle(fast_receiver.as_mut(), 2).await?;
    handle(slow_receiver.as_mut(), 1).await?;
    assert!(poll!(&mut blocked).is_pending());
    assert_eq!(channel.progress().await?.earliest_available, Some(1));
    handle(slow_receiver.as_mut(), 2).await?;
    tokio::time::timeout(Duration::from_secs(5), blocked).await??;
    let next = root("source", 4, &[4]);
    let mut blocked = Box::pin(channel.publish(&next));
    assert!(poll!(&mut blocked).is_pending());
    handle(fast_receiver.as_mut(), 3).await?;
    assert!(poll!(&mut blocked).is_pending());
    slow.control.cancel();
    drop(slow_receiver);
    assert!(
        poll!(&mut blocked).is_pending(),
        "disconnect is not retirement"
    );
    let mut resumed = subscribe(channel, "slow")?;
    let mut resumed_receiver = resumed.pipe.take_receiver()?;
    handle(resumed_receiver.as_mut(), 3).await?;
    tokio::time::timeout(Duration::from_secs(5), blocked).await??;
    assert_eq!(channel.progress().await?.accepted, 4);
    handle(fast_receiver.as_mut(), 4).await?;
    handle(resumed_receiver.as_mut(), 4).await?;
    Ok(())
}

async fn memory_pressure() -> Result<()> {
    let limit = nonzero(2 * size(&root("source", 1, &[1])));
    let retained =
        MemoryEnvelopeStore::new_with_byte_budget(nonzero(8), RetentionPolicy::Backpressure, limit);
    assert_eq!(retained.max_bytes(), Some(limit));
    retained_pressure(&retained).await?;
    retained.shutdown().await?;
    let qos = QosChannel::volatile_with_byte_budget(
        definition(false, RetentionPolicy::Backpressure),
        limit,
    )?;
    assert_eq!(qos.max_bytes(), Some(limit));
    qos_pressure(&qos).await?;
    qos.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byte_quota_waits_for_handling_and_every_subscriber_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), memory_pressure()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn byte_quota_waits_for_handling_and_every_subscriber_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), memory_pressure()).await?
}

#[tokio::test]
async fn lossy_byte_pruning_reports_exact_gaps_and_keeps_an_oversized_singleton() -> Result<()> {
    let limit = nonzero(2 * size(&root("source", 1, &[1])));
    let store =
        MemoryEnvelopeStore::new_with_byte_budget(nonzero(8), RetentionPolicy::PruneOldest, limit);
    let generation = store.acquire_generation()?;
    let channel = QosChannel::volatile_with_byte_budget(
        definition(false, RetentionPolicy::PruneOldest),
        limit,
    )?;
    let mut slow = subscribe(&channel, "slow")?;
    let mut receiver = slow.pipe.take_receiver()?;
    for event in [root("source", 1, &[1]), root("source", 2, &[2]), large(3)] {
        store.append(generation, &event).await?;
        channel.publish(&event).await?;
    }
    assert!(matches!(
        store.next(generation, 0).await,
        Err(PipeError::PositionUnavailable { oldest: 3, .. })
    ));
    assert!(matches!(
        receiver.receive().await,
        Err(PipeError::PositionUnavailable { oldest: 3, .. })
    ));
    assert_eq!(
        store
            .next(generation, 2)
            .await?
            .expect("oversized")
            .position,
        3
    );
    assert_eq!(channel.progress().await?.earliest_available, Some(3));
    assert_eq!(channel.progress().await?.processed["slow"], 0);
    channel.shutdown().await?;
    store.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn retirement_releases_bytes_but_cancelled_publication_does_not_consume_them() -> Result<()> {
    let channel = QosChannel::volatile_with_byte_budget(
        definition(false, RetentionPolicy::Backpressure),
        nonzero(1),
    )?;
    let mut fast = subscribe(&channel, "fast")?;
    let mut receiver = fast.pipe.take_receiver()?;
    channel.publish(&large(1)).await?;
    handle(receiver.as_mut(), 1).await?;
    let next = large(2);
    let mut pending = Box::pin(channel.publish(&next));
    assert!(poll!(&mut pending).is_pending());
    channel.retire("slow").await?;
    tokio::time::timeout(Duration::from_secs(5), pending).await??;
    assert_eq!(channel.progress().await?.accepted, 2);
    channel.shutdown().await?;
    Ok(())
}

#[cfg(feature = "computation-rocksdb-tests")]
mod durable {
    use super::*;
    use drasi_core::computation::{ComputationIndexProvider, ComputationIndexes};
    use drasi_core::interface::OutboxPageLimits;
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use std::path::Path;

    async fn indexes(path: &Path) -> Result<ComputationIndexes> {
        let provider = RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        );
        Ok(provider.create_indexes("byte-budgets", "journal").await?)
    }

    fn codec() -> Result<EnvelopeCodec> {
        let mut codec = EnvelopeCodec::new(nonzero(1 << 20));
        codec.register_schema(Arc::new(Schema::new(
            schema_descriptor(),
            Arc::new(ReadingValidator(schema_descriptor())),
        )))?;
        Ok(codec)
    }

    async fn retained(path: &Path, limit: NonZeroUsize) -> Result<IndexedEnvelopeStore> {
        Ok(IndexedEnvelopeStore::try_new_with_byte_budget(
            indexes(path).await?,
            Arc::new(codec()?),
            "edge",
            nonzero(8),
            RetentionPolicy::Backpressure,
            limit,
        )?)
    }

    async fn paged_retained(
        path: &Path,
        max_bytes: Option<NonZeroUsize>,
    ) -> Result<IndexedEnvelopeStore> {
        Ok(IndexedEnvelopeStore::try_new_with_options(
            indexes(path).await?,
            Arc::new(codec()?),
            "edge",
            nonzero(17),
            RetentionPolicy::Backpressure,
            IndexedJournalOptions {
                max_bytes,
                page_limits: Some(OutboxPageLimits {
                    max_records: nonzero(2),
                    max_bytes: nonzero(2048),
                }),
            },
        )?)
    }

    #[tokio::test]
    async fn paged_retained_history_replays_and_prunes_across_page_boundaries() -> Result<()> {
        for max_bytes in [None, Some(nonzero(32 * size(&large(1))))] {
            let directory = tempfile::tempdir()?;
            let store = paged_retained(directory.path(), max_bytes).await?;
            let generation = store.acquire_generation()?;
            for sequence in 1..=17 {
                let envelope = if sequence == 7 {
                    large(sequence)
                } else {
                    root("source", sequence, &[sequence as u16])
                };
                store.append(generation, &envelope).await?;
            }
            assert!(matches!(
                store.append(generation, &large(18)).await,
                Err(PipeError::CapacityExhausted)
            ));
            store.acknowledge(generation, 4).await?;
            for sequence in 18..=21 {
                store
                    .append(generation, &root("source", sequence, &[sequence as u16]))
                    .await?;
            }
            store.shutdown().await?;
            drop(store);
            let store = paged_retained(directory.path(), max_bytes).await?;
            let generation = store.acquire_generation()?;
            assert_eq!(store.progress(generation).await?, 4);
            assert!(matches!(
                store.next(generation, 3).await,
                Err(PipeError::PositionUnavailable { oldest: 5, .. })
            ));
            for sequence in 5..=21 {
                let stored = store
                    .next(generation, sequence - 1)
                    .await?
                    .expect("complete replay");
                assert_eq!(stored.position, sequence);
                assert_eq!(stored.envelope.system().sequence(), sequence);
                store.acknowledge(generation, sequence).await?;
            }
            assert!(store.next(generation, 21).await?.is_none());
            assert!(store.next(generation, u64::MAX).await?.is_none());
            assert_eq!(store.append(generation, &large(22)).await?, 22);
            assert_eq!(
                store
                    .next(generation, 21)
                    .await?
                    .expect("new head")
                    .position,
                22
            );
            store.shutdown().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn paged_retained_byte_pruning_preserves_the_handling_boundary() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let store = paged_retained(
            directory.path(),
            Some(nonzero(2 * size(&root("source", 1, &[1])))),
        )
        .await?;
        retained_pressure(&store).await?;
        store.shutdown().await?;
        drop(store);
        let store = paged_retained(directory.path(), Some(nonzero(1))).await?;
        let generation = store.acquire_generation()?;
        assert_eq!(store.progress(generation).await?, 3);
        assert_eq!(
            store
                .next(generation, 3)
                .await?
                .expect("preserved obligation")
                .position,
            4
        );
        store.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn paged_retained_startup_validates_corruption_beyond_the_first_page() -> Result<()> {
        for corrupt_position in [9, 17] {
            let directory = tempfile::tempdir()?;
            let store = paged_retained(directory.path(), None).await?;
            let generation = store.acquire_generation()?;
            for sequence in 1..=17 {
                store
                    .append(generation, &root("source", sequence, &[sequence as u16]))
                    .await?;
            }
            store.shutdown().await?;
            drop(store);
            let indexes = indexes(directory.path()).await?;
            indexes.indexes().session_control.begin().await?;
            indexes
                .outbox_writer()
                .expect("outbox")
                .append("edge", corrupt_position, b"invalid frame")
                .await?;
            indexes.indexes().session_control.commit().await?;
            let store = IndexedEnvelopeStore::try_new_with_options(
                indexes,
                Arc::new(codec()?),
                "edge",
                nonzero(17),
                RetentionPolicy::Backpressure,
                IndexedJournalOptions {
                    max_bytes: None,
                    page_limits: Some(OutboxPageLimits {
                        max_records: nonzero(2),
                        max_bytes: nonzero(2048),
                    }),
                },
            )?;
            let generation = store.acquire_generation()?;
            assert!(
                matches!(store.progress(generation).await, Err(PipeError::Backend(_))),
                "late corruption must fail reconstruction"
            );
            store.shutdown().await?;
        }
        Ok(())
    }

    async fn qos(
        path: &Path,
        limit: NonZeroUsize,
        recovery: QosRecoveryOptions,
    ) -> Result<Arc<QosChannel>> {
        Ok(QosChannel::persistent_with_options(
            definition(true, RetentionPolicy::Backpressure),
            indexes(path).await?,
            codec()?,
            "edge",
            QosJournalOptions {
                max_bytes: Some(limit),
                recovery: Some(recovery),
                page_limits: None,
            },
        )
        .await?)
    }

    fn admission_options() -> QosRecoveryOptions {
        QosRecoveryOptions::Admission(AdmissionOptions {
            construction_scope: "instance".into(),
            graph_id: "byte-budget".into(),
            component_id: component("source"),
            failure_scope: drasi_core::interface::FailureMode::ProcessRestart,
            max_producers: nonzero(2),
            receipts_per_producer: nonzero(8),
        })
    }

    async fn paged_qos(path: &Path, recovery: QosRecoveryOptions) -> Result<Arc<QosChannel>> {
        Ok(QosChannel::persistent_with_options(
            QosChannelDefinition {
                capacity: nonzero(17),
                ..definition(true, RetentionPolicy::Backpressure)
            },
            indexes(path).await?,
            codec()?,
            "edge",
            QosJournalOptions {
                recovery: Some(recovery),
                max_bytes: Some(nonzero(32 * size(&large(1)))),
                page_limits: Some(OutboxPageLimits {
                    max_records: nonzero(2),
                    max_bytes: nonzero(2048),
                }),
            },
        )
        .await?)
    }

    fn positioned(sequence: u64) -> ChangeEnvelope {
        let envelope = root("source", sequence, &[sequence as u16]);
        envelope.derive(
            envelope.id().clone(),
            envelope.changes().clone(),
            SystemMetadata::new(stream("source"), sequence)
                .with_source_position(bytes::Bytes::copy_from_slice(&sequence.to_be_bytes())),
        )
    }

    async fn admitted_outputs(path: &Path) -> Result<Vec<ChangeEnvelope>> {
        let channel = paged_qos(path, admission_options()).await?;
        let session = channel.register_producer(component("client")).await?;
        for sequence in 1..=17 {
            channel
                .admit(&session, sequence, &positioned(sequence))
                .await?;
        }
        let mut pipe = subscribe(&channel, "fast")?;
        let mut receiver = pipe.pipe.take_receiver()?;
        let mut outputs = Vec::new();
        for sequence in 1..=17 {
            let delivery = receiver.receive().await?.expect("admitted output");
            assert_eq!(delivery.envelope().system().sequence(), sequence);
            let (envelope, acknowledgement) = delivery.into_parts();
            acknowledgement
                .expect("ack")
                .complete(HandlingOutcome::Handled)
                .await?;
            outputs.push(envelope);
        }
        channel.shutdown().await?;
        Ok(outputs)
    }

    async fn shared_journal(group: &SharedStorageGroup) -> Result<Arc<QosChannel>> {
        shared_journal_with_capacity(group, nonzero(17)).await
    }

    async fn shared_journal_with_capacity(
        group: &SharedStorageGroup,
        capacity: NonZeroUsize,
    ) -> Result<Arc<QosChannel>> {
        Ok(group
            .channel_with_page_limits(
                QosChannelDefinition {
                    capacity,
                    ..definition(true, RetentionPolicy::Backpressure)
                },
                "history",
                codec()?,
                ReplayOptions {
                    failure_scope: drasi_core::interface::FailureMode::ProcessRestart,
                    receipt_capacity: nonzero(8),
                },
                OutboxPageLimits {
                    max_records: nonzero(2),
                    max_bytes: nonzero(2048),
                },
            )
            .await?)
    }

    #[tokio::test]
    async fn paged_shared_journal_evicts_payloads_and_restores_independent_cursors_and_receipts(
    ) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let inputs = admitted_outputs(&directory.path().join("inputs")).await?;
        let mut identity = None;
        for subscriber in ["fast", "slow"] {
            let group = SharedStorageGroup::new(
                "shared",
                component("producer"),
                indexes(&directory.path().join("shared")).await?,
            )?;
            let channel = shared_journal(&group).await?;
            if subscriber == "fast" {
                for input in &inputs {
                    channel.publish(input).await?;
                }
                identity = channel.output_journal_identity();
            } else {
                assert_eq!(channel.output_journal_identity(), identity);
                assert_eq!(channel.progress().await?.processed["fast"], 17);
                assert_eq!(channel.progress().await?.processed["slow"], 0);
            }
            let journal = ResourceId::try_new("journal")?;
            let storage = ResourceId::try_new("storage")?;
            let mut pipe = channel
                .definition()
                .pipe(journal.clone(), subscriber)
                .with_shared_storage(storage.clone())
                .create_with_resources(&BTreeMap::from([
                    (journal, channel.resource()),
                    (storage, group.resource()),
                ]))?;
            let mut receiver = pipe.pipe.take_receiver()?;
            let mut payloads = Vec::new();
            for sequence in 1..=17 {
                let delivery = tokio::time::timeout(Duration::from_secs(5), receiver.receive())
                    .await??
                    .expect("retained history");
                assert_eq!(delivery.envelope().system().sequence(), sequence);
                payloads.push(Arc::downgrade(delivery.envelope().event()));
                delivery
                    .into_parts()
                    .1
                    .expect("acknowledgement")
                    .complete(HandlingOutcome::Handled)
                    .await?;
                assert!(
                    payloads
                        .iter()
                        .filter(|payload| payload.upgrade().is_some())
                        .count()
                        <= 2
                );
            }
            assert_eq!(channel.publish(&inputs[9]).await?.position(), Some(10));
            assert!(
                channel.publish(&inputs[0]).await.is_err(),
                "expired output receipts must not become new appends"
            );
            assert_eq!(channel.progress().await?.accepted, 17);
            pipe.control.cancel();
            channel.shutdown().await?;
            group.shutdown().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn paged_shared_startup_rejects_corruption_beyond_the_first_page() -> Result<()> {
        use drasi_core::computation::ComputationTransaction;
        let directory = tempfile::tempdir()?;
        let inputs = admitted_outputs(&directory.path().join("inputs")).await?;
        let group = SharedStorageGroup::new(
            "shared",
            component("producer"),
            indexes(&directory.path().join("shared")).await?,
        )?;
        let channel = shared_journal(&group).await?;
        for input in &inputs {
            channel.publish(input).await?;
        }
        channel.shutdown().await?;
        let transaction: ComputationTransaction = group
            .transaction_group()
            .expect("group")
            .journal_transaction("history")?;
        transaction
            .run(async {
                transaction
                    .resources()
                    .outbox_writer()
                    .expect("outbox")
                    .append("events", 17, b"corrupt")
                    .await?;
                Ok(())
            })
            .await?;
        transaction.shutdown().await?;
        drop(transaction);
        assert!(shared_journal(&group).await.is_err());
        group.shutdown().await?;
        Ok(())
    }

    const SHARED_SUBSCRIBERS: [&str; 2] = ["fast", "slow"];

    fn shared_subscriber(
        group: &Arc<SharedStorageGroup>,
        channel: &Arc<QosChannel>,
        subscriber: &str,
    ) -> Result<ProvidedPipe> {
        let journal = ResourceId::try_new("journal")?;
        let storage = ResourceId::try_new("storage")?;
        Ok(channel
            .definition()
            .pipe(journal.clone(), subscriber)
            .with_shared_storage(storage.clone())
            .create_with_resources(&BTreeMap::from([
                (journal, channel.resource()),
                (storage, group.resource()),
            ]))?)
    }

    /// Asserts the journal window against a model of accepted and completed positions.
    async fn assert_shared_window(
        channel: &QosChannel,
        capacity: NonZeroUsize,
        accepted: u64,
        completed: [u64; 2],
    ) -> Result<()> {
        let progress = channel.progress().await?;
        assert_eq!(progress.accepted, accepted);
        for (subscriber, position) in SHARED_SUBSCRIBERS.iter().zip(completed) {
            assert_eq!(progress.processed[*subscriber], position);
        }
        if let Some(earliest) = progress.earliest_available {
            let slowest = completed.into_iter().min().unwrap_or_default();
            assert!(
                earliest <= slowest + 1,
                "position {} was trimmed before every subscriber completed it",
                earliest - 1
            );
            assert!(
                accepted + 1 - earliest <= capacity.get() as u64,
                "journal retains more than its capacity"
            );
        }
        Ok(())
    }

    /// Regression: the shared append path once took its trim floor from the
    /// decoded page cache rather than the journal window.
    #[tokio::test]
    async fn paged_shared_append_retains_history_outside_the_page_cache() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let inputs = admitted_outputs(&directory.path().join("inputs")).await?;
        let path = directory.path().join("shared");
        let capacity = nonzero(4);
        let group =
            SharedStorageGroup::new("shared", component("producer"), indexes(&path).await?)?;
        let channel = shared_journal_with_capacity(&group, capacity).await?;
        for input in &inputs[..4] {
            channel.publish(input).await?;
        }
        let mut pipes = Vec::new();
        let mut receivers = Vec::new();
        for subscriber in SHARED_SUBSCRIBERS {
            let mut pipe = shared_subscriber(&group, &channel, subscriber)?;
            receivers.push(pipe.pipe.take_receiver()?);
            pipes.push(pipe);
        }
        let [fast, slow] = receivers.as_mut_slice() else {
            unreachable!("two subscribers");
        };
        // The slow reader caches page [1, 2]; the fast reader replaces it with [3, 4].
        handle(slow.as_mut(), 1).await?;
        for sequence in 1..=4 {
            handle(fast.as_mut(), sequence).await?;
        }
        assert_eq!(channel.publish(&inputs[4]).await?.position(), Some(5));
        assert_shared_window(&channel, capacity, 5, [4, 1]).await?;
        assert_eq!(channel.progress().await?.earliest_available, Some(2));
        for sequence in 2..=5 {
            handle(slow.as_mut(), sequence).await?;
        }
        handle(fast.as_mut(), 5).await?;
        for pipe in &pipes {
            pipe.control.cancel();
        }
        channel.shutdown().await?;
        group.shutdown().await?;
        drop((receivers, pipes, channel, group));

        let group =
            SharedStorageGroup::new("shared", component("producer"), indexes(&path).await?)?;
        let channel = shared_journal_with_capacity(&group, capacity).await?;
        assert_shared_window(&channel, capacity, 5, [5, 5]).await?;
        assert_eq!(channel.progress().await?.earliest_available, Some(2));
        channel.shutdown().await?;
        group.shutdown().await?;
        Ok(())
    }

    /// Drives deterministic interleavings of appends and per-subscriber
    /// completions across a reopen, checking the retained window after every step.
    #[tokio::test]
    async fn paged_shared_journal_window_follows_every_subscriber_across_reopen() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let inputs = admitted_outputs(&directory.path().join("inputs")).await?;
        let capacity = nonzero(4);
        for seed in [1u64, 7, 42, 1009] {
            let path = directory.path().join(format!("shared-{seed}"));
            let mut random = seed;
            let mut accepted = 0u64;
            let mut completed = [0u64; 2];
            for (session, until) in [inputs.len() / 2, inputs.len()].into_iter().enumerate() {
                let last = session == 1;
                let group = SharedStorageGroup::new(
                    "shared",
                    component("producer"),
                    indexes(&path).await?,
                )?;
                let channel = shared_journal_with_capacity(&group, capacity).await?;
                assert_shared_window(&channel, capacity, accepted, completed).await?;
                let mut pipes = Vec::new();
                let mut receivers = Vec::new();
                for subscriber in SHARED_SUBSCRIBERS {
                    let mut pipe = shared_subscriber(&group, &channel, subscriber)?;
                    receivers.push(pipe.pipe.take_receiver()?);
                    pipes.push(pipe);
                }
                while (accepted as usize) < until
                    || last && completed.iter().any(|position| *position < accepted)
                {
                    random = random
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    let slowest = completed.into_iter().min().unwrap_or_default();
                    match (random >> 33) % 3 {
                        0 if (accepted as usize) < until
                            && accepted - slowest < capacity.get() as u64 =>
                        {
                            channel.publish(&inputs[accepted as usize]).await?;
                            accepted += 1;
                        }
                        choice @ (1 | 2) => {
                            let index = choice as usize - 1;
                            if completed[index] < accepted {
                                completed[index] += 1;
                                handle(receivers[index].as_mut(), completed[index]).await?;
                            }
                        }
                        _ => {}
                    }
                    assert_shared_window(&channel, capacity, accepted, completed).await?;
                }
                for pipe in &pipes {
                    pipe.control.cancel();
                }
                channel.shutdown().await?;
                group.shutdown().await?;
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn paged_qos_keeps_only_a_page_of_live_payloads_and_retries_outside_the_cache(
    ) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let channel = paged_qos(directory.path(), QosRecoveryOptions::Disabled).await?;
        for sequence in 1..=17 {
            channel.publish(&positioned(sequence)).await?;
        }
        let mut fast = subscribe(&channel, "fast")?;
        let mut receiver = fast.pipe.take_receiver()?;
        let mut payloads = Vec::new();
        for sequence in 1..=17 {
            let delivery = receiver.receive().await?.expect("complete history");
            assert_eq!(delivery.envelope().system().sequence(), sequence);
            payloads.push(Arc::downgrade(delivery.envelope().event()));
            delivery
                .into_parts()
                .1
                .expect("ack")
                .complete(HandlingOutcome::Handled)
                .await?;
            assert!(
                payloads
                    .iter()
                    .filter(|payload| payload.upgrade().is_some())
                    .count()
                    <= 2,
                "decoded payloads must be evicted, not retained behind a paged reader"
            );
            assert_eq!(
                channel.progress().await?.source_position,
                Some(bytes::Bytes::copy_from_slice(&17u64.to_be_bytes()))
            );
        }
        assert_eq!(channel.progress().await?.earliest_available, Some(1));
        assert_eq!(channel.publish(&positioned(1)).await?.position(), Some(1));
        assert!(channel.publish(&root("source", 1, &[999])).await.is_err());
        fast.control.cancel();
        drop(receiver);
        channel.shutdown().await?;
        drop(fast);
        drop(channel);
        let channel = paged_qos(directory.path(), QosRecoveryOptions::Disabled).await?;
        assert_eq!(channel.progress().await?.processed["fast"], 17);
        assert_eq!(channel.progress().await?.processed["slow"], 0);
        assert_eq!(
            channel.progress().await?.source_position,
            Some(bytes::Bytes::copy_from_slice(&17u64.to_be_bytes()))
        );
        assert_eq!(channel.publish(&positioned(9)).await?.position(), Some(9));
        let mut slow = subscribe(&channel, "slow")?;
        let mut receiver = slow.pipe.take_receiver()?;
        for sequence in 1..=17 {
            handle(receiver.as_mut(), sequence).await?;
        }
        channel.publish(&positioned(18)).await?;
        assert_eq!(channel.progress().await?.earliest_available, Some(2));
        assert!(
            channel.publish(&positioned(1)).await.is_err(),
            "pruned legacy retries must not reapply"
        );
        channel.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn paged_qos_preserves_admission_and_output_replay_receipts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let channel = paged_qos(directory.path(), admission_options()).await?;
        let session = channel.register_producer(component("client")).await?;
        for sequence in 1..=17 {
            channel
                .admit(&session, sequence, &positioned(sequence))
                .await?;
        }
        channel.shutdown().await?;
        drop(channel);
        let channel = paged_qos(directory.path(), admission_options()).await?;
        assert_eq!(
            channel.admit(&session, 17, &positioned(17)).await?.position,
            17
        );
        assert!(channel.admit(&session, 1, &positioned(1)).await.is_err());
        assert!(channel.admit(&session, 17, &large(17)).await.is_err());
        for subscriber in ["fast", "slow"] {
            let mut pipe = subscribe(&channel, subscriber)?;
            let mut receiver = pipe.pipe.take_receiver()?;
            for sequence in 1..=17 {
                handle(receiver.as_mut(), sequence).await?;
            }
        }
        channel.shutdown().await?;
        drop(channel);

        let directory = tempfile::tempdir()?;
        let recovery = QosRecoveryOptions::Replay(ReplayOptions {
            failure_scope: drasi_core::interface::FailureMode::ProcessRestart,
            receipt_capacity: nonzero(8),
        });
        let outputs = admitted_outputs(&directory.path().join("source")).await?;
        let channel = paged_qos(directory.path(), recovery.clone()).await?;
        for envelope in &outputs {
            channel.publish(envelope).await?;
        }
        channel.shutdown().await?;
        drop(channel);
        let channel = paged_qos(directory.path(), recovery).await?;
        assert_eq!(channel.publish(&outputs[16]).await?.position(), Some(17));
        assert!(channel.publish(&outputs[0]).await.is_err());
        for subscriber in ["fast", "slow"] {
            let mut pipe = subscribe(&channel, subscriber)?;
            let mut receiver = pipe.pipe.take_receiver()?;
            for sequence in 1..=17 {
                handle(receiver.as_mut(), sequence).await?;
            }
        }
        channel.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn paged_qos_reconfiguration_preserves_strict_gaps_and_explicit_skipping() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let channel = paged_qos(directory.path(), QosRecoveryOptions::Disabled).await?;
        for sequence in 1..=17 {
            channel.publish(&positioned(sequence)).await?;
        }
        channel.shutdown().await?;
        drop(channel);
        for retention in [RetentionPolicy::Backpressure, RetentionPolicy::PruneOldest] {
            let result = QosChannel::persistent_with_options(
                QosChannelDefinition {
                    capacity: nonzero(2),
                    ..definition(true, retention)
                },
                indexes(directory.path()).await?,
                codec()?,
                "edge",
                QosJournalOptions {
                    recovery: Some(QosRecoveryOptions::Disabled),
                    max_bytes: None,
                    page_limits: Some(OutboxPageLimits {
                        max_records: nonzero(1),
                        max_bytes: nonzero(1),
                    }),
                },
            )
            .await;
            if retention == RetentionPolicy::Backpressure {
                assert!(
                    result.is_err(),
                    "a page cache must not hide pending obligations"
                );
                continue;
            }
            let channel = result?;
            assert_eq!(channel.progress().await?.earliest_available, Some(16));
            assert_eq!(channel.progress().await?.processed["slow"], 0);
            let mut strict = subscribe(&channel, "slow")?;
            let mut receiver = strict.pipe.take_receiver()?;
            assert!(matches!(
                receiver.receive().await,
                Err(PipeError::PositionUnavailable { oldest: 16, .. })
            ));
            strict.control.cancel();
            drop(receiver);
            let resource = ResourceId::try_new("channel")?;
            let mut config = channel.definition().pipe(resource.clone(), "slow");
            config.gap_policy = ReplayGapPolicy::SkipWithNotification;
            let mut skip =
                config.create_with_resources(&BTreeMap::from([(resource, channel.resource())]))?;
            let mut receiver = skip.pipe.take_receiver()?;
            for sequence in 16..=17 {
                handle(receiver.as_mut(), sequence).await?;
            }
            channel.publish(&positioned(18)).await?;
            assert_eq!(channel.progress().await?.earliest_available, Some(17));
            handle(receiver.as_mut(), 18).await?;
            channel.shutdown().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn paged_qos_detects_late_progress_corruption_in_every_recovery_mode() -> Result<()> {
        for recovery in [
            QosRecoveryOptions::Disabled,
            admission_options(),
            QosRecoveryOptions::Replay(ReplayOptions {
                failure_scope: drasi_core::interface::FailureMode::ProcessRestart,
                receipt_capacity: nonzero(8),
            }),
        ] {
            let directory = tempfile::tempdir()?;
            let channel = paged_qos(directory.path(), recovery.clone()).await?;
            let session = if matches!(recovery, QosRecoveryOptions::Admission(_)) {
                Some(channel.register_producer(component("client")).await?)
            } else {
                None
            };
            let outputs = if matches!(recovery, QosRecoveryOptions::Replay(_)) {
                Some(admitted_outputs(&directory.path().join("source")).await?)
            } else {
                None
            };
            for sequence in 1..=17 {
                let envelope = outputs.as_ref().map_or_else(
                    || positioned(sequence),
                    |outputs| outputs[sequence as usize - 1].clone(),
                );
                if let Some(session) = &session {
                    channel.admit(session, sequence, &envelope).await?;
                } else {
                    channel.publish(&envelope).await?;
                }
            }
            channel.shutdown().await?;
            drop(channel);
            let resources = indexes(directory.path()).await?;
            resources.indexes().session_control.begin().await?;
            let invalid = if matches!(recovery, QosRecoveryOptions::Disabled) {
                bytes::Bytes::from_static(b"invalid frame")
            } else {
                codec()?.encode(&positioned(9))?
            };
            resources
                .outbox_writer()
                .expect("outbox")
                .append("edge", 9, &invalid)
                .await?;
            resources.indexes().session_control.commit().await?;
            resources.cleanup().expect("cleanup").shutdown().await?;
            drop(resources);
            assert!(
                paged_qos(directory.path(), recovery).await.is_err(),
                "late corrupt progress must not evade full validation"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn real_persistent_journals_charge_binary_size_not_storage_json_and_preserve_acknowledgements(
    ) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let limit = nonzero(2 * size(&root("source", 1, &[1])));
        assert_ne!(
            size(&root("source", 1, &[1])),
            codec()?.encode(&root("source", 1, &[1]))?.len()
        );
        let store = retained(&directory.path().join("retained"), limit).await?;
        retained_pressure(&store).await?;
        store.shutdown().await?;
        drop(store);
        let store = retained(&directory.path().join("retained"), limit).await?;
        let generation = store.acquire_generation()?;
        assert_eq!(store.progress(generation).await?, 3);
        assert_eq!(
            store
                .next(generation, 3)
                .await?
                .expect("retained input")
                .position,
            4
        );
        store.shutdown().await?;
        let channel = qos(
            &directory.path().join("qos"),
            limit,
            QosRecoveryOptions::Disabled,
        )
        .await?;
        qos_pressure(&channel).await?;
        channel.shutdown().await?;
        drop(channel);
        let channel = qos(
            &directory.path().join("qos"),
            limit,
            QosRecoveryOptions::Disabled,
        )
        .await?;
        assert_eq!(channel.progress().await?.accepted, 4);
        assert!(channel
            .progress()
            .await?
            .processed
            .values()
            .all(|position| *position == 4));
        channel.publish(&large(5)).await?;
        assert_eq!(channel.progress().await?.earliest_available, Some(5));
        channel.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn smaller_reopened_budgets_preserve_existing_pending_history() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let event_size = size(&root("source", 1, &[1]));
        let store = retained(directory.path(), nonzero(2 * event_size)).await?;
        let generation = store.acquire_generation()?;
        store.append(generation, &root("source", 1, &[1])).await?;
        store.append(generation, &root("source", 2, &[2])).await?;
        store.shutdown().await?;
        drop(store);
        let store = retained(directory.path(), nonzero(event_size)).await?;
        let generation = store.acquire_generation()?;
        assert!(matches!(
            store.append(generation, &root("source", 3, &[3])).await,
            Err(PipeError::CapacityExhausted)
        ));
        assert_eq!(
            store
                .next(generation, 0)
                .await?
                .expect("first obligation")
                .position,
            1
        );
        store.acknowledge(generation, 2).await?;
        store.append(generation, &root("source", 3, &[3])).await?;
        store.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn admission_charges_derived_envelopes_and_preserves_retry_receipts_across_reopen(
    ) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let options = admission_options();
        let channel = qos(directory.path(), nonzero(1), options.clone()).await?;
        let session = channel.register_producer(component("client")).await?;
        let first = root("source", 1, &[1]);
        let receipt = channel.admit(&session, 1, &first).await?;
        assert_eq!(channel.admit(&session, 1, &first).await?, receipt);
        assert!(matches!(
            channel.admit(&session, 2, &large(2)).await,
            Err(PipeError::CapacityExhausted)
        ));
        assert!(channel.admission_receipt(&session, 2).await?.is_none());
        channel.shutdown().await?;
        drop(channel);
        let channel = qos(directory.path(), nonzero(1), options).await?;
        assert_eq!(channel.admit(&session, 1, &first).await?, receipt);
        for subscriber in ["fast", "slow"] {
            let mut pipe = subscribe(&channel, subscriber)?;
            handle(pipe.pipe.take_receiver()?.as_mut(), 1).await?;
        }
        assert_eq!(channel.admit(&session, 2, &large(2)).await?.position, 2);
        channel.shutdown().await?;
        Ok(())
    }
}
