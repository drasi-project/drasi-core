// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{QueryRecoveryHead, QueryRecoveryReader, QueryRecoveryView};
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationIndexes, ComputationResourceCleanup},
    interface::{IndexError, StorageDurability},
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct CountedReader {
    inner: Arc<dyn QueryRecoveryReader>,
    views: AtomicUsize,
}

struct CleanupGate {
    held: AtomicBool,
    entered: Notify,
    resume: Notify,
}

struct GatedProvider {
    inner: Arc<RocksDbComputationProvider>,
    gate: Arc<CleanupGate>,
}

struct GatedCleanup {
    inner: Arc<dyn ComputationResourceCleanup>,
    gate: Arc<CleanupGate>,
}

#[async_trait]
impl ComputationResourceCleanup for GatedCleanup {
    fn cancel(&self) {
        self.inner.cancel();
    }
    async fn shutdown(&self) -> Result<(), IndexError> {
        self.inner.shutdown().await?;
        if self.gate.held.load(Ordering::Acquire) {
            self.gate.entered.notify_one();
            self.gate.resume.notified().await;
        }
        Ok(())
    }
    async fn quiesce(&self) -> Result<(), IndexError> {
        self.inner.quiesce().await
    }
}

#[async_trait]
impl ComputationIndexProvider for GatedProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        let indexes = self.inner.create_indexes(graph, component).await?;
        let inner = indexes.cleanup().expect("actual RocksDB cleanup").clone();
        Ok(indexes.with_cleanup(Arc::new(GatedCleanup {
            inner,
            gate: self.gate.clone(),
        })))
    }
    fn is_volatile(&self) -> bool {
        false
    }
    fn durability(&self) -> StorageDurability {
        self.inner.durability()
    }
}

#[async_trait]
impl QueryRecoveryReader for CountedReader {
    async fn recovery_head(&self) -> Result<QueryRecoveryHead, OutputReadError> {
        self.inner.recovery_head().await
    }
    async fn recovery_view(
        &self,
        after: Option<u64>,
    ) -> Result<QueryRecoveryView, OutputReadError> {
        self.views.fetch_add(1, Ordering::AcqRel);
        self.inner.recovery_view(after).await
    }
    async fn wait_recovery(
        &self,
        after: Option<&QueryRecoveryHead>,
    ) -> Result<QueryRecoveryHead, OutputReadError> {
        self.inner.wait_recovery(after).await
    }
}

struct Destination {
    descriptor: ComponentDescriptor,
    received: Arc<Mutex<Vec<ChangeEnvelope>>>,
}

#[async_trait]
impl ComputationComponent for Destination {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Destination {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn snapshot_consumer(&mut self, port: &PortId) -> Option<&mut dyn SnapshotConsumer> {
        (port.as_str() == "in").then_some(self)
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.received
            .lock()
            .expect("destination observations")
            .push(input.envelope);
        Ok(())
    }
}

#[async_trait]
impl SnapshotConsumer for Destination {
    async fn replace_snapshot(&mut self, input: InputEnvelope) -> Result<()> {
        self.handle(input).await
    }
}

pub(super) fn sink(progress: Arc<dyn ConsumerProgressStore>) -> Result<CheckpointedSink> {
    Ok(CheckpointedSink::new(
        Box::new(Destination {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("destination")?,
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            received: Arc::new(Mutex::new(Vec::new())),
        }),
        progress,
    )?
    .allow_skipped_resets())
}

pub(super) fn replay(
    reader: Arc<dyn QueryRecoveryReader>,
    progress: Arc<dyn ConsumerProgressStore>,
    policy: ConsumerRecoveryPolicy,
) -> Result<QueryReplayTransformer> {
    Ok(QueryReplayTransformer::with_reader(
        ComponentId::try_new("replay")?,
        COMPONENT.into(),
        StreamId::try_new("replay/out")?,
        reader,
        progress,
        policy,
    ))
}

pub(super) async fn deliver(
    sink: &mut CheckpointedSink,
    outputs: Vec<OutputEnvelope>,
) -> Result<()> {
    for output in outputs {
        sink.handle(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: output.envelope,
        })
        .await?;
    }
    Ok(())
}

fn create_profile(path: &Path, profile: &str) -> Result<NativeComponentProxy> {
    use drasi_core::computation::{ComputationIndexProvider, InMemoryComputationProvider};
    let (provider, publication): (Arc<dyn ComputationIndexProvider>, _) = match profile {
        "memory" => (
            Arc::new(InMemoryComputationProvider),
            QueryPublicationMode::Atomic,
        ),
        "atomic" => (provider(path)?, QueryPublicationMode::Atomic),
        "non_atomic" => (
            Arc::new(super::non_atomic_store::SplitOutputProvider::new(path)?),
            QueryPublicationMode::NonAtomic,
        ),
        _ => anyhow::bail!("unknown test profile"),
    };
    factory()?.create_component_with_processing_publication(
        ComponentId::try_new(COMPONENT)?,
        json!({
            "query": QUERY, "outbox_capacity": 2, "volatile": profile == "memory",
            "publication": if profile == "non_atomic" { "non_atomic" } else { "atomic" },
        }),
        scope(1),
        provider,
        None,
        publication,
    )
}

async fn retained_consumers() -> Result<()> {
    for profile in ["memory", "atomic", "non_atomic"] {
        let directory = tempfile::tempdir()?;
        let mut query = create_profile(directory.path(), profile)?;
        let reader = Arc::new(CountedReader {
            inner: query
                .query_api()
                .context("query API")?
                .recovery_reader()
                .context("recovery reader")?,
            views: AtomicUsize::new(0),
        });
        assert!(matches!(
            reader.recovery_head().await,
            Err(OutputReadError::NotReady(_))
        ));
        query.start().await?;
        query.continue_transform().await?;
        let first = query
            .transform(input(
                SourceChange::Insert {
                    element: item(5, 1),
                },
                1,
            )?)
            .await?;
        query.delivery_completed(&first).await?;
        let head = reader.recovery_head().await?;
        let progress = Arc::new(MemoryConsumerProgress::default());
        let mut consumer = replay(
            reader.clone(),
            progress.clone(),
            ConsumerRecoveryPolicy::Strict,
        )?;
        let mut destination = sink(progress.clone())?;
        consumer.start().await?;
        let initial = consumer.on_wakeup().await?;
        assert_eq!(initial.len(), 1);
        assert!(QueryChangeCodec::is_snapshot(&initial[0].envelope));
        assert!(
            progress.load(COMPONENT).await?.is_none(),
            "reading cannot acknowledge delivery"
        );
        deliver(&mut destination, initial).await?;
        assert_eq!(
            progress
                .load(COMPONENT)
                .await?
                .context("handled snapshot")?
                .sequence,
            1
        );
        let wakeup = consumer.wakeup_source().context("replay wakeup")?;
        let mut wait = Box::pin(wakeup.wait());
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut wait)
            .await
            .is_err());

        query.stop().await?;
        assert_eq!(reader.recovery_head().await?, head);
        let retained = reader.recovery_view(Some(0)).await?;
        assert_eq!(retained.snapshot.rows.len(), 1);
        assert_eq!(retained.retained.context("retained output")?.len(), 1);
        let outputs = query.output_readers();
        assert!(
            outputs.ports()[&PortId::try_new("out")?]
                .query()
                .context("query view")?
                .query_snapshot()
                .await
                .is_err(),
            "ordinary stopped reads must remain fenced"
        );
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut wait)
            .await
            .is_err());
        let mut resumed = replay(
            reader.clone(),
            progress.clone(),
            ConsumerRecoveryPolicy::Strict,
        )?;
        resumed.start().await?;
        assert!(resumed.on_wakeup().await?.is_empty());
        let views = reader.views.load(Ordering::Acquire);
        query.start().await?;
        query.continue_transform().await?;
        for sequence in 2..=4 {
            let output = query
                .transform(input(
                    SourceChange::Replace {
                        element: item(sequence as i64, sequence),
                    },
                    sequence,
                )?)
                .await?;
            query.delivery_completed(&output).await?;
            for output in output {
                deliver(
                    &mut destination,
                    resumed
                        .transform(InputEnvelope {
                            port: PortId::try_new("in")?,
                            envelope: output.envelope,
                        })
                        .await?,
                )
                .await?;
            }
        }
        tokio::time::timeout(Duration::from_secs(2), wait).await??;
        assert_eq!(
            reader.views.load(Ordering::Acquire),
            views,
            "contiguous live delivery must not transfer the full result snapshot"
        );
        assert_eq!(
            progress
                .load(COMPONENT)
                .await?
                .context("handled output")?
                .sequence,
            4
        );
        query.stop().await?;
        let retained = reader.recovery_view(Some(0)).await?;
        assert_eq!(retained.oldest_sequence, Some(3));
        assert_eq!(retained.snapshot.as_of_sequence, 4);

        for policy in [
            ConsumerRecoveryPolicy::Strict,
            ConsumerRecoveryPolicy::AutoReset,
            ConsumerRecoveryPolicy::AutoSkipGap,
        ] {
            let stale = Arc::new(MemoryConsumerProgress::default());
            stale
                .commit_handled(
                    COMPONENT,
                    ConsumerCheckpoint {
                        identity: head.identity.clone(),
                        generation: head.generation,
                        sequence: 1,
                    },
                )
                .await?;
            let mut recovering = replay(reader.clone(), stale.clone(), policy)?;
            recovering.start().await?;
            let output = recovering.on_wakeup().await;
            if policy == ConsumerRecoveryPolicy::Strict {
                assert!(output
                    .expect_err("Strict must reject the retained-history gap")
                    .downcast_ref::<QueryHistoryError>()
                    .is_some());
                assert_eq!(
                    stale
                        .load(COMPONENT)
                        .await?
                        .context("unchanged checkpoint")?
                        .sequence,
                    1
                );
            } else {
                let output = output?;
                if policy == ConsumerRecoveryPolicy::AutoReset {
                    assert_eq!(output.len(), 1);
                    assert!(QueryChangeCodec::is_snapshot(&output[0].envelope));
                } else {
                    assert_eq!(output.len(), 3);
                    assert!(QueryChangeCodec::is_progress_only(&output[0].envelope));
                }
                let mut destination = sink(stale.clone())?;
                deliver(&mut destination, output).await?;
                assert_eq!(
                    stale
                        .load(COMPONENT)
                        .await?
                        .context("recovered checkpoint")?
                        .sequence,
                    4
                );
            }
        }
        query.deprovision().await?;
        assert!(matches!(
            reader.recovery_head().await,
            Err(OutputReadError::Retired)
        ));
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_retained_recovery_and_consumer_policies_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), retained_consumers()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_retained_recovery_and_consumer_policies_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), retained_consumers()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn failed_native_query_does_not_expose_uncertain_retained_state() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut query = create(
        directory.path(),
        "MATCH (n:Item) RETURN abs(n.value) AS value",
        1,
    )?;
    let reader = query
        .query_api()
        .context("query API")?
        .recovery_reader()
        .context("recovery reader")?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(5, 1),
            },
            1,
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    let committed = reader.recovery_head().await?;
    let mut invalid = item(5, 2);
    if let Element::Node { properties, .. } = &mut invalid {
        *properties = ElementPropertyMap::from(json!({"value":"not a number"}));
    }
    assert!(query
        .transform(input(SourceChange::Replace { element: invalid }, 2)?)
        .await
        .is_err());
    query.stop().await?;
    assert!(matches!(
        reader.recovery_head().await,
        Err(OutputReadError::NotReady(_))
    ));
    query.start().await?;
    assert_eq!(reader.recovery_head().await?, committed);
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retained_query_reads_require_completed_host_cleanup_after_plugin_stop() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let gate = Arc::new(CleanupGate {
        held: AtomicBool::new(false),
        entered: Notify::new(),
        resume: Notify::new(),
    });
    let mut query = factory()?.create_component_with_processing(
        ComponentId::try_new(COMPONENT)?,
        json!({"query": QUERY}),
        scope(1),
        Arc::new(GatedProvider {
            inner: provider(directory.path())?,
            gate: gate.clone(),
        }),
    )?;
    let recovery = query
        .query_api()
        .context("query API")?
        .recovery_reader()
        .context("recovery reader")?;
    query.start().await?;
    let output = query
        .transform(input(
            SourceChange::Insert {
                element: item(5, 1),
            },
            1,
        )?)
        .await?;
    query.delivery_completed(&output).await?;
    let head = recovery.recovery_head().await?;
    gate.held.store(true, Ordering::Release);
    {
        let mut stop = Box::pin(query.stop());
        tokio::select! {
            result = &mut stop => anyhow::bail!("cleanup finished before release: {result:?}"),
            reached = tokio::time::timeout(Duration::from_secs(10), gate.entered.notified()) =>
                reached.context("host cleanup did not start")?,
        }
        assert!(matches!(
            recovery.recovery_head().await,
            Err(OutputReadError::NotReady(_))
        ));
        assert!(matches!(
            recovery.recovery_view(None).await,
            Err(OutputReadError::NotReady(_))
        ));
    }
    assert!(
        query.start().await.is_err(),
        "cancelled cleanup cannot permit restart"
    );
    gate.held.store(false, Ordering::Release);
    gate.resume.notify_one();
    tokio::time::timeout(Duration::from_secs(10), query.stop()).await??;
    assert_eq!(recovery.recovery_head().await?, head);
    assert_eq!(recovery.recovery_view(None).await?.snapshot.rows.len(), 1);
    Ok(())
}
