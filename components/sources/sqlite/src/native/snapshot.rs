// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use bytes::Bytes;
use drasi_core::interface::FailureMode;
use std::sync::{Mutex, Weak};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Handover {
    pub version: u32,
    pub epoch: Uuid,
    pub binding: String,
    pub completed: bool,
}

#[derive(Default)]
struct Binding {
    prepared: bool,
    active: bool,
    client: Option<Weak<tokio::sync::OwnedSemaphorePermit>>,
}

struct Shared {
    worker: RwLock<Option<JoinHandle<Result<Option<Handover>>>>>,
    run: Mutex<Option<Arc<SnapshotRun>>>,
    lifecycle: tokio::sync::Mutex<()>,
    binding: Mutex<Binding>,
    handover: Mutex<Option<Handover>>,
    timeout: Duration,
}

struct SnapshotRun {
    shutdown: watch::Sender<bool>,
}

impl Shared {
    fn ensure_run(&self, expected: &Arc<SnapshotRun>) -> Result<()> {
        anyhow::ensure!(
            self.run
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite snapshot run poisoned"))?
                .as_ref()
                .is_some_and(|run| Arc::ptr_eq(run, expected)),
            "SQLite snapshot stream belongs to a retired worker"
        );
        Ok(())
    }

    fn cancel(&self) -> Result<()> {
        if let Some(run) = self
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot run poisoned"))?
            .as_ref()
        {
            run.shutdown.send_replace(true);
        }
        Ok(())
    }

    fn current(&self) -> Result<Option<Handover>> {
        Ok(self
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite handover poisoned"))?
            .clone())
    }

    async fn join(&self) -> Result<Option<Handover>> {
        let mut worker = self.worker.write().await;
        let result = join_owned_worker_gracefully(&mut worker, self.timeout).await;
        if worker.is_none() {
            self.run
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite snapshot run poisoned"))?
                .take();
            self.binding
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite snapshot binding poisoned"))?
                .active = false;
        }
        match result? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Absent => Ok(None),
            WorkerCompletion::Cancelled => anyhow::bail!("SQLite snapshot worker was cancelled"),
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Err(error) = self.cancel() {
            log::error!("Native SQLite snapshot cancellation failed: {error:#}");
        }
        if self.worker.get_mut().is_some() {
            log::warn!("Native SQLite snapshot dropped without awaited cleanup");
        }
    }
}

/// Paired initial loading for a native SQLite source. The query owns this
/// provider's worker and awaits cleanup even when its startup is cancelled.
pub struct SqliteSnapshot {
    source: ComponentId,
    stream: StreamId,
    config: SqliteConfig,
    progress: SourceProgressReader,
    shared: Arc<Shared>,
}

impl SqliteSnapshot {
    pub(super) fn new(source: &SqliteSource) -> Result<Self> {
        Ok(Self {
            source: source.descriptor.id().clone(),
            stream: source.stream.clone(),
            config: source.config.clone(),
            progress: source
                .progress
                .clone()
                .context("SQLite snapshot requires persistent progress")?,
            shared: Arc::new(Shared {
                worker: RwLock::new(None),
                run: Mutex::new(None),
                lifecycle: tokio::sync::Mutex::new(()),
                binding: Mutex::new(Binding::default()),
                handover: Mutex::new(None),
                timeout: source.config.timeout(),
            }),
        })
    }

    pub(super) fn validate_source(&self, source: &SqliteSource) -> Result<()> {
        anyhow::ensure!(
            self.source == *source.descriptor.id()
                && self.stream == source.stream
                && self.config == source.config
                && source
                    .progress
                    .as_ref()
                    .is_some_and(|owner| owner.same_owner(&self.progress)),
            "native SQLite snapshot does not match its source and actual progress owner"
        );
        Ok(())
    }

    pub(super) fn bind_client(&self, owner: &Arc<tokio::sync::OwnedSemaphorePermit>) -> Result<()> {
        let mut binding = self
            .shared
            .binding
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot binding poisoned"))?;
        anyhow::ensure!(
            !binding.active && binding.client.as_ref().and_then(Weak::upgrade).is_none(),
            "native SQLite snapshot still belongs to another active client owner"
        );
        binding.client = Some(Arc::downgrade(owner));
        Ok(())
    }

    pub(super) fn live_epoch(&self, progress: &SourceProgressSnapshot) -> Result<Uuid> {
        let handover = self
            .shared
            .current()?
            .context("SQLite snapshot has not been prepared")?;
        anyhow::ensure!(
            handover.completed
                && progress.bootstrap_complete
                && journal::checkpoint(progress, &self.source, handover.epoch)?.is_some(),
            "native SQLite initialization has no completed owner position"
        );
        Ok(handover.epoch)
    }
}

pub(super) struct SnapshotWork {
    pub source: ComponentId,
    pub stream: StreamId,
    pub config: SqliteConfig,
    pub owner: SourceProgressReader,
    pub progress: Arc<SourceProgressSnapshot>,
    pub handover: Handover,
    pub outputs: mpsc::Sender<ChangeEnvelope>,
    pub shutdown: watch::Receiver<bool>,
}

struct Reader {
    receiver: mpsc::Receiver<ChangeEnvelope>,
    shared: Arc<Shared>,
    run: Arc<SnapshotRun>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.run.shutdown.send_replace(true);
    }
}

#[async_trait]
impl ComputationBootstrapProvider for SqliteSnapshot {
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        let _lifecycle = self.shared.lifecycle.lock().await;
        anyhow::ensure!(
            self.shared.worker.read().await.is_none(),
            "SQLite snapshot requires awaited cleanup before preparation"
        );
        state.durability().require(FailureMode::ProcessRestart)?;
        let progress = self.progress.snapshot()?;
        anyhow::ensure!(
            progress.recovered && progress.persistent && progress.failure.is_none(),
            "SQLite snapshot requires recovered persistent owner progress"
        );
        let handover = state
            .read()
            .await?
            .map(|bytes| serde_json::from_slice::<Handover>(&bytes))
            .transpose()?;
        if let Some(handover) = &handover {
            anyhow::ensure!(
                handover.version == 1
                    && handover.binding
                        == journal::binding(
                            &self.config,
                            &self.source,
                            &self.stream,
                            &self.progress,
                            true
                        )?,
                "native SQLite snapshot binding changed"
            );
            let checkpoint = journal::checkpoint(&progress, &self.source, handover.epoch)?;
            anyhow::ensure!(
                handover.completed == checkpoint.is_some()
                    && handover.completed == progress.bootstrap_complete,
                "SQLite handover and owner progress disagree; explicit recovery required"
            );
        } else {
            anyhow::ensure!(
                !progress.bootstrap_complete
                    && !progress
                        .checkpoints
                        .contains_key(&SourceProgressKey::Source(self.source.to_string())),
                "native SQLite handover ownership state is missing"
            );
        }
        let mut binding = self
            .shared
            .binding
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot binding poisoned"))?;
        anyhow::ensure!(
            !binding.active,
            "SQLite snapshot requires cleanup before preparation"
        );
        *self
            .shared
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite handover poisoned"))? = handover;
        binding.prepared = true;
        Ok(BootstrapPreparation::Ready)
    }

    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("native SQLite snapshot requires query-owned handover state")
    }

    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let _lifecycle = self.shared.lifecycle.lock().await;
        let client = {
            let mut binding = self
                .shared
                .binding
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite snapshot binding poisoned"))?;
            anyhow::ensure!(
                binding.prepared && !binding.active,
                "SQLite snapshot requires preparation and completed cleanup"
            );
            let client = binding
                .client
                .as_ref()
                .map(|owner| {
                    owner
                        .upgrade()
                        .context("SQLite snapshot source owner was dropped")
                })
                .transpose()?;
            binding.active = true;
            client
        };
        let handover = match self.shared.current()? {
            Some(handover) => {
                anyhow::ensure!(!handover.completed, "completed SQLite initialization requires explicit retirement, not automatic replacement");
                handover
            }
            None => Handover {
                version: 1,
                epoch: Uuid::new_v4(),
                binding: journal::binding(
                    &self.config,
                    &self.source,
                    &self.stream,
                    &self.progress,
                    true,
                )?,
                completed: false,
            },
        };
        state
            .write(Bytes::from(serde_json::to_vec(&handover)?))
            .await?;
        *self
            .shared
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite handover poisoned"))? = Some(handover.clone());
        let run = Arc::new(SnapshotRun {
            shutdown: watch::channel(false).0,
        });
        *self
            .shared
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot run poisoned"))? = Some(run.clone());
        let (outputs, receiver) = mpsc::channel(self.config.output_capacity.get());
        let work = SnapshotWork {
            source: self.source.clone(),
            stream: self.stream.clone(),
            config: self.config.clone(),
            owner: self.progress.clone(),
            progress: self.progress.snapshot()?,
            handover,
            outputs,
            shutdown: run.shutdown.subscribe(),
        };
        spawn_owned_blocking_worker(&self.shared.worker, move || {
            let _client = client;
            let result = worker::snapshot(work);
            if let Err(error) = &result {
                log::error!("Native SQLite snapshot failed: {error:#}");
            }
            result
        })
        .await?;
        let reader = Reader {
            receiver,
            shared: self.shared.clone(),
            run,
        };
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::try_unfold(
                reader,
                |mut reader| async move {
                    reader.shared.ensure_run(&reader.run)?;
                    if let Some(change) = reader.receiver.recv().await {
                        reader.shared.ensure_run(&reader.run)?;
                        return Ok(Some((change, reader)));
                    }
                    {
                        let _lifecycle = reader.shared.lifecycle.lock().await;
                        reader.shared.ensure_run(&reader.run)?;
                        let handover = reader
                            .shared
                            .join()
                            .await?
                            .context("native SQLite snapshot did not complete")?;
                        *reader
                            .shared
                            .handover
                            .lock()
                            .map_err(|_| anyhow::anyhow!("SQLite handover poisoned"))? =
                            Some(handover);
                    }
                    Ok(None)
                },
            )),
            watermarks: Vec::new(),
        })
    }

    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        let handover = self
            .shared
            .current()?
            .context("SQLite snapshot handover is missing")?;
        anyhow::ensure!(handover.completed, "native SQLite snapshot is incomplete");
        Ok(vec![BootstrapWatermark {
            stream: self.stream.clone(),
            source_id: Some(self.source.to_string()),
            sequence: 0,
            position: Some(journal::identity(handover.epoch, 0)?),
        }])
    }

    fn completion_state(&self) -> Result<Option<Bytes>> {
        let handover = self
            .shared
            .current()?
            .context("SQLite snapshot handover is missing")?;
        anyhow::ensure!(handover.completed, "native SQLite snapshot is incomplete");
        Ok(Some(Bytes::from(serde_json::to_vec(&handover)?)))
    }

    async fn stop(&self) -> Result<()> {
        let _lifecycle = self.shared.lifecycle.lock().await;
        self.shared.cancel()?;
        self.shared
            .binding
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot binding poisoned"))?
            .prepared = false;
        self.shared.join().await?;
        Ok(())
    }
}
