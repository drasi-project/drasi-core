// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct Gate {
    pub(super) reached: Notify,
    resume: Notify,
    hold: AtomicBool,
    cleanup_resume: Notify,
    hold_cleanup: AtomicBool,
    pub(super) fresh_snapshot: AtomicBool,
}

impl Gate {
    pub(super) fn new(hold_cleanup: bool) -> Arc<Self> {
        Arc::new(Self {
            reached: Notify::new(),
            resume: Notify::new(),
            hold: AtomicBool::new(true),
            cleanup_resume: Notify::new(),
            hold_cleanup: AtomicBool::new(hold_cleanup),
            fresh_snapshot: AtomicBool::new(false),
        })
    }

    pub(super) fn release(&self) {
        self.hold.store(false, Ordering::Release);
        self.resume.notify_one();
    }

    fn release_cleanup(&self) {
        self.hold_cleanup.store(false, Ordering::Release);
        self.cleanup_resume.notify_one();
    }
}

pub(super) struct PausedProvider {
    pub(super) inner: Arc<drasi_host_sdk::computation::NativeBootstrapProxy>,
    pub(super) gate: Arc<Gate>,
}

#[async_trait]
impl ComputationBootstrapProvider for PausedProvider {
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        self.inner.validate_query_scope(instance, graph, query)
    }
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.inner.recovery_reader()
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    fn has_pending_snapshot(&self) -> Result<bool> {
        Ok(
            self.gate.fresh_snapshot.load(Ordering::Acquire)
                || self.inner.has_pending_snapshot()?,
        )
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let mut snapshot = self.inner.snapshot_with_state(state).await?;
        let gate = self.gate.clone();
        snapshot.changes = Box::pin(futures_util::stream::try_unfold(
            (snapshot.changes, gate, 0usize),
            |(mut stream, gate, delivered)| async move {
                if delivered == 1 && gate.hold.load(Ordering::Acquire) {
                    gate.reached.notify_one();
                    gate.resume.notified().await;
                }
                Ok(stream
                    .next()
                    .await
                    .transpose()?
                    .map(|input| (input, (stream, gate, delivered + 1))))
            },
        ));
        Ok(snapshot)
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete_snapshot().await
    }
    fn completion_state(&self) -> Result<Option<bytes::Bytes>> {
        self.inner.completion_state()
    }
    async fn stop(&self) -> Result<()> {
        if self.gate.hold_cleanup.load(Ordering::Acquire) {
            self.gate.cleanup_resume.notified().await;
        }
        self.inner.stop().await
    }
}

fn query(
    indexes: &Path,
    text: &str,
    generation: u64,
    bootstrap: Option<Arc<dyn ComputationBootstrapProvider>>,
) -> Result<NativeComponentProxy> {
    let mut query = factory()?.create_component_with_processing(
        ComponentId::try_new(COMPONENT)?,
        json!({"query":text,"bootstrap":true,"recovery":"auto_reset"}),
        scope(generation),
        super::super::provider(indexes)?,
    )?;
    if let Some(bootstrap) = bootstrap {
        query.bind_bootstrap(Arc::new(NativeBootstrapResource(bootstrap)))?;
    }
    Ok(query)
}

async fn interrupted_after_row() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    let indexes = directory.path().join("indexes");
    sqlite::create_database(&database)?;
    let gate = Gate::new(true);
    let provider = file_provider(&database, 1)?;
    let mut loading = query(
        &indexes,
        TEXT,
        1,
        Some(Arc::new(PausedProvider {
            inner: provider.clone(),
            gate: gate.clone(),
        })),
    )?;
    let snapshot = reader(&loading)?;
    let progress = loading
        .processing_progress_owner()
        .context("missing progress owner")?;
    {
        let start = loading.start();
        tokio::pin!(start);
        tokio::select! {
            result = &mut start => anyhow::bail!("bootstrap completed before its second row: {result:?}"),
            _ = gate.reached.notified() => {}
        }
        assert!(
            snapshot.snapshot().await.is_err(),
            "partial initialization became readable"
        );
        assert!(
            progress.snapshot().recovered
                && !progress.snapshot().ready
                && !progress.snapshot().bootstrap_complete
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), loading.stop())
            .await
            .is_err()
    );
    assert!(
        loading.start().await.is_err(),
        "unfinished cleanup allowed another initialization"
    );
    assert!(provider.freeze_for_retirement().is_err());
    gate.release_cleanup();
    loading.stop().await?;
    provider.freeze_for_retirement()?.resume();
    drop(loading);

    let mut strict = file_query(&database, &indexes, 2)?;
    let failure = strict
        .start()
        .await
        .expect_err("Strict must not adopt partial initialization");
    assert!(format!("{failure:#}").contains("incomplete"));
    strict.stop().await?;
    drop(strict);

    gate.release();
    let mut resumed = query(&indexes, TEXT, 3, Some(file_provider(&database, 3)?))?;
    resumed.start().await?;
    assert_eq!(total(&reader(&resumed)?.snapshot().await?)?, 30.0);
    let progress = resumed
        .processing_progress_owner()
        .expect("actual progress owner")
        .snapshot();
    assert_eq!(progress.reset_generation, 1);
    assert!(progress.bootstrap_complete && progress.ready && progress.checkpoints.is_empty());
    assert!(resumed.continue_transform().await?.is_empty());
    resumed.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_bootstrap_interrupted_rows_cleanup_and_auto_reset_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), interrupted_after_row()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_bootstrap_interrupted_rows_cleanup_and_auto_reset_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), interrupted_after_row()).await?
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_bootstrap_auto_reset_requires_provider_and_changes_output_epoch() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    let indexes = directory.path().join("indexes");
    sqlite::create_database(&database)?;
    let mut original = file_query(&database, &indexes, 1)?;
    original.start().await?;
    let producer = reader(&original)?
        .snapshot()
        .await?
        .cursor()
        .producer()
        .clone();
    original.stop().await?;
    drop(original);
    let text = "MATCH (n:items) RETURN sum(n.amount * 2) AS total";
    let mut unbound = query(&indexes, text, 2, None)?;
    assert!(unbound.start().await.is_err());
    drop(unbound);
    let mut unchanged = file_query(&database, &indexes, 3)?;
    unchanged.start().await?;
    let snapshot = reader(&unchanged)?.snapshot().await?;
    assert_eq!(total(&snapshot)?, 30.0);
    assert_eq!(snapshot.cursor().producer(), &producer);
    unchanged.stop().await?;
    drop(unchanged);
    let mut reset = query(&indexes, text, 4, Some(file_provider(&database, 4)?))?;
    reset.start().await?;
    let snapshot = reader(&reset)?.snapshot().await?;
    assert_eq!(total(&snapshot)?, 60.0);
    assert_ne!(snapshot.cursor().producer(), &producer);
    assert_eq!(snapshot.cursor().sequence(), 0);
    reset.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_bootstrap_prepared_refresh_obeys_recovery_policy() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    let indexes = directory.path().join("indexes");
    sqlite::create_database(&database)?;
    let mut original = file_query(&database, &indexes, 1)?;
    original.start().await?;
    let producer = reader(&original)?
        .snapshot()
        .await?
        .cursor()
        .producer()
        .clone();
    original.stop().await?;
    drop(original);
    rusqlite::Connection::open(&database)?.execute("UPDATE items SET amount=11 WHERE id=1", [])?;
    let gate = Gate::new(false);
    gate.release();
    gate.fresh_snapshot.store(true, Ordering::Release);
    let mut strict = factory()?.create_component_with_processing(
        ComponentId::try_new(COMPONENT)?,
        json!({"query":TEXT,"bootstrap":true}),
        scope(2),
        super::super::provider(&indexes)?,
    )?;
    strict.bind_bootstrap(Arc::new(NativeBootstrapResource(Arc::new(
        PausedProvider {
            inner: file_provider(&database, 2)?,
            gate: gate.clone(),
        },
    ))))?;
    let failure = strict
        .start()
        .await
        .expect_err("Strict must not replace a completed projection");
    assert!(format!("{failure:#}").contains("Strict"));
    strict.stop().await?;
    drop(strict);
    let mut reset = query(
        &indexes,
        TEXT,
        3,
        Some(Arc::new(PausedProvider {
            inner: file_provider(&database, 3)?,
            gate,
        })),
    )?;
    reset.start().await?;
    let snapshot = reader(&reset)?.snapshot().await?;
    assert_eq!(total(&snapshot)?, 31.0);
    assert_ne!(snapshot.cursor().producer(), &producer);
    assert_eq!(
        reset
            .processing_progress_owner()
            .expect("actual progress owner")
            .snapshot()
            .reset_generation,
        1
    );
    reset.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_bootstrap_auto_reset_preserves_unconfirmed_output() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    let indexes = directory.path().join("indexes");
    sqlite::create_database(&database)?;
    let mut original = file_query(&database, &indexes, 1)?;
    original.start().await?;
    let output = original
        .transform(input(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", "extra"),
                        labels: Arc::from([Arc::from("items")]),
                        effective_from: 1,
                    },
                    properties: ElementPropertyMap::from(json!({"amount":5})),
                },
            },
            1,
        )?)
        .await?;
    aggregate(&output, Some(30.0), 35.0, 1)?;
    original.stop().await?;
    drop(original);
    let mut reset = query(
        &indexes,
        "MATCH (n:items) RETURN sum(n.amount * 2) AS total",
        2,
        Some(file_provider(&database, 2)?),
    )?;
    let failure = reset
        .start()
        .await
        .expect_err("AutoReset must not discard pending output");
    assert!(format!("{failure:#}").contains("unconfirmed output"));
    reset.stop().await?;
    drop(reset);
    let mut retained = file_query(&database, &indexes, 3)?;
    retained.start().await?;
    let replayed = retained.continue_transform().await?;
    aggregate(&replayed, Some(30.0), 35.0, 1)?;
    assert_eq!(
        GraphProducerProgress::from_envelope(&replayed[0].envelope)?,
        GraphProducerProgress::from_envelope(&output[0].envelope)?
    );
    retained.delivery_completed(&replayed).await?;
    assert_eq!(total(&reader(&retained)?.snapshot().await?)?, 35.0);
    retained.stop().await?;
    Ok(())
}
