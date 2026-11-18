// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The snapshot provider core shared by the source's coordinated bootstrap
//! (paired through the source's coordinator, with its barrier table and a
//! CDC boundary) and the standalone read-only snapshot. The plugins adapt it
//! to their declared provider kind.

use super::{markers, Cursor, NativeSettings, SnapshotRun};
use crate::client::Authentication;
use anyhow::{ensure, Context, Result};
use drasi_computation_contracts::{
    BootstrapLabels, BootstrapPreparation, BootstrapWatermark, ComponentId,
    ComputationBootstrapSnapshot, GraphChangeCodec, SourceProgressKey, SourceProgressView,
    StreamId,
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::Mutex;

/// How long `stop` waits for snapshot cleanup by default.
pub const DEFAULT_STOP_TIMEOUT_MS: u64 = 30_000;

/// Supplies the credentials a snapshot connects with: the paired source's
/// current activation credentials, or the standalone snapshot's literal ones.
pub type CredentialSource = Arc<dyn Fn() -> Result<Authentication> + Send + Sync>;

/// One query's SQL Server snapshot.
pub struct SnapshotProvider {
    source: ComponentId,
    stream: StreamId,
    settings: NativeSettings,
    /// Configured upper bound on the tables read (empty: every selected one).
    labels: Vec<String>,
    /// The query's declared labels (`None`: unrestricted).
    requested: Option<BootstrapLabels>,
    stop_timeout_ms: u64,
    credentials: CredentialSource,
    progress: Option<SourceProgressView>,
    run: Mutex<Option<Arc<SnapshotRun>>>,
    generation: Arc<AtomicU64>,
}

impl SnapshotProvider {
    /// A coordinated snapshot: it writes a marker into the source's barrier
    /// table and completes with the source's CDC boundary in `progress`.
    pub fn coordinated(
        source: ComponentId,
        stream: StreamId,
        settings: NativeSettings,
        progress: SourceProgressView,
        credentials: CredentialSource,
    ) -> Result<Self> {
        settings.validate()?;
        ensure!(
            settings.barrier_table.is_some(),
            "coordinated SQL Server bootstrap requires the source's barrierTable"
        );
        Ok(Self::build(
            source,
            stream,
            settings,
            Vec::new(),
            Some(progress),
            credentials,
        ))
    }

    /// A read-only snapshot without a CDC boundary.
    pub fn read_only(
        source: ComponentId,
        stream: StreamId,
        settings: NativeSettings,
        labels: Vec<String>,
    ) -> Result<Self> {
        settings.validate()?;
        // Standalone snapshots receive no activation credentials.
        settings.validate_credentials(false)?;
        ensure!(
            settings.barrier_table.is_none() && !settings.tables.is_empty(),
            "read-only SQL Server snapshot requires explicit tables and no barrierTable"
        );
        let literal = settings.literal_authentication();
        Ok(Self::build(
            source,
            stream,
            settings,
            labels,
            None,
            Arc::new(move || Ok(literal.clone())),
        ))
    }

    fn build(
        source: ComponentId,
        stream: StreamId,
        settings: NativeSettings,
        labels: Vec<String>,
        progress: Option<SourceProgressView>,
        credentials: CredentialSource,
    ) -> Self {
        Self {
            source,
            stream,
            settings,
            labels,
            requested: None,
            stop_timeout_ms: DEFAULT_STOP_TIMEOUT_MS,
            credentials,
            progress,
            run: Mutex::new(None),
            generation: Arc::default(),
        }
    }

    /// Read only the tables whose label (the table name) the query declares.
    pub fn with_labels(mut self, requested: Option<BootstrapLabels>) -> Self {
        self.requested = requested;
        self
    }

    /// How long `stop` waits for snapshot cleanup.
    pub fn with_stop_timeout_ms(mut self, stop_timeout_ms: u64) -> Self {
        self.stop_timeout_ms = stop_timeout_ms;
        self
    }

    pub fn recovery_reader(&self) -> Option<SourceProgressView> {
        self.progress.clone()
    }

    /// A coordinated snapshot is bound to its query's progress owner.
    pub fn validate_query(&self, graph: &str, query: &ComponentId) -> Result<()> {
        if let Some(progress) = &self.progress {
            ensure!(
                progress.graph_id() == graph && progress.component_id() == query,
                "SQL Server bootstrap progress belongs to another query"
            );
        }
        Ok(())
    }

    pub fn prepare(&self) -> Result<BootstrapPreparation> {
        if let Some(progress) = &self.progress {
            let progress = progress.snapshot()?;
            if let Some(failure) = &progress.failure {
                anyhow::bail!("SQL Server bootstrap progress failed: {failure}");
            }
            if let Some(checkpoint) = progress
                .checkpoints
                .get(&SourceProgressKey::Source(self.source.clone()))
            {
                Cursor::decode(
                    checkpoint
                        .source_position
                        .as_deref()
                        .context("SQL Server checkpoint lacks CDC position")?,
                )?;
            }
        }
        Ok(BootstrapPreparation::Ready)
    }

    pub async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let mut current = self.run.lock().await;
        ensure!(
            current.is_none(),
            "SQL Server snapshot requires successful stop before reuse"
        );
        let generation = self.generation.load(Ordering::Acquire);
        let marker_prefix = self
            .progress
            .as_ref()
            .map(|progress| markers::marker_prefix(progress.graph_id(), self.source.as_str()));
        self.settings
            .connection()
            .warn_about_transport("Native SQL Server bootstrap");
        let (run, rows) = super::start_snapshot(
            self.settings.clone(),
            (self.credentials)()?,
            self.source.to_string(),
            self.labels.clone(),
            self.requested.clone(),
            marker_prefix,
        )?;
        *current = Some(run.clone());
        drop(current);
        let stream = self.stream.clone();
        let active = self.generation.clone();
        let changes = futures_util::stream::unfold(
            (rows, run, active, stream, 0u64, false),
            move |(mut rows, run, active, stream, sequence, failed)| async move {
                if failed {
                    return None;
                }
                if active.load(Ordering::Acquire) != generation {
                    run.cancel();
                    return Some((
                        Err(anyhow::anyhow!("SQL Server snapshot generation retired")),
                        (rows, run, active, stream, sequence, true),
                    ));
                }
                let next = rows.next().await;
                let encoded = match next {
                    Ok(None) => return None,
                    Ok(Some(change)) => {
                        if active.load(Ordering::Acquire) != generation {
                            Err(anyhow::anyhow!("SQL Server snapshot generation retired"))
                        } else {
                            sequence
                                .checked_add(1)
                                .context("SQL Server snapshot sequence exhausted")
                                .and_then(|sequence| {
                                    GraphChangeCodec::encode_change(
                                        change,
                                        stream.clone(),
                                        sequence,
                                        Some(chrono::Utc::now()),
                                    )
                                    .map_err(Into::into)
                                })
                        }
                    }
                    Err(error) => Err(error),
                };
                let failed = encoded.is_err();
                if failed {
                    run.cancel();
                }
                Some((
                    encoded,
                    (
                        rows,
                        run,
                        active,
                        stream,
                        sequence.saturating_add(1),
                        failed,
                    ),
                ))
            },
        );
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(changes),
            watermarks: Vec::new(),
        })
    }

    pub async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        let run = self
            .run
            .lock()
            .await
            .clone()
            .context("SQL Server snapshot is not active")?;
        run.complete()
            .await?
            .map(|boundary| {
                Ok::<_, anyhow::Error>(BootstrapWatermark {
                    producer: None,
                    stream: self.stream.clone(),
                    source_id: Some(self.source.clone()),
                    sequence: 0,
                    position: Some(boundary.encode()?.into()),
                })
            })
            .transpose()
            .map(|watermark| watermark.into_iter().collect())
    }

    pub async fn stop(&self) -> Result<()> {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let run = self.run.lock().await.clone();
        if let Some(run) = run {
            run.stop(std::time::Duration::from_millis(self.stop_timeout_ms))
                .await?;
            let mut current = self.run.lock().await;
            if current
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &run))
            {
                *current = None;
            }
        }
        Ok(())
    }
}
