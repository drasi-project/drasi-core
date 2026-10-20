// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{journal::GroupHandover, *};
use bytes::Bytes;
use drasi_core::interface::FailureMode;
use std::{
    collections::BTreeMap,
    sync::{Mutex, Weak},
};
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

#[derive(Default)]
struct MemberState {
    prepared: bool,
    cleanup_required: bool,
    handover: Option<GroupHandover>,
}
struct Member {
    owner: Arc<QuerySourceProgress>,
    lifecycle: tokio::sync::Mutex<()>,
    state: Mutex<MemberState>,
}
struct Run {
    consumer: ComponentId,
    shutdown: watch::Sender<bool>,
    _lease: OwnedMutexGuard<()>,
}
struct Shared {
    source: ComponentId,
    stream: StreamId,
    config: SqliteConfig,
    consumers: Arc<ConsumerProgressSet>,
    members: BTreeMap<ComponentId, Arc<Member>>,
    gate: Arc<tokio::sync::Mutex<()>>,
    lifecycle: tokio::sync::Mutex<()>,
    worker: RwLock<Option<JoinHandle<Result<Option<GroupHandover>>>>>,
    run: Mutex<Option<Arc<Run>>>,
    client: Mutex<Option<Weak<tokio::sync::OwnedSemaphorePermit>>>,
}

impl Shared {
    fn current(&self) -> Result<Option<Arc<Run>>> {
        Ok(self
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite group snapshot run poisoned"))?
            .clone())
    }
    fn ensure_run(&self, run: &Arc<Run>) -> Result<()> {
        anyhow::ensure!(
            self.current()?
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, run)),
            "SQLite shared snapshot belongs to a retired worker"
        );
        Ok(())
    }
    async fn join(&self, run: &Arc<Run>) -> Result<Option<GroupHandover>> {
        let _lifecycle = self.lifecycle.lock().await;
        self.ensure_run(run)?;
        let mut worker = self.worker.write().await;
        let result = join_owned_worker_gracefully(&mut worker, self.config.timeout()).await;
        if worker.is_none() {
            self.run
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite group snapshot run poisoned"))?
                .take();
        }
        match result? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Absent => Ok(None),
            WorkerCompletion::Cancelled => {
                anyhow::bail!("SQLite shared snapshot worker was cancelled")
            }
        }
    }
    fn binding(&self) -> Result<String> {
        journal::group_binding(
            &self.config,
            &self.source,
            &self.stream,
            &self.consumers.read_only(),
            true,
        )
    }
}
impl Drop for Shared {
    fn drop(&mut self) {
        match self.run.get_mut() {
            Ok(Some(run)) => {
                run.shutdown.send_replace(true);
            }
            Ok(None) => {}
            Err(error) => log::error!("SQLite shared snapshot run poisoned during drop: {error}"),
        }
        if self.worker.get_mut().is_some() {
            log::warn!("SQLite shared snapshot dropped without awaited cleanup");
        }
    }
}

/// One shared, serialized snapshot worker for a source's immutable consumer set.
/// Each provider records its own query-owned initialization and actual boundary.
pub struct SqliteConsumerSnapshots {
    shared: Arc<Shared>,
}

impl SqliteConsumerSnapshots {
    pub(super) fn new(source: &SqliteSource) -> Result<Self> {
        let consumers = source
            .consumer_progress
            .clone()
            .context("SQLite snapshot requires consumer progress")?;
        let members = consumers
            .owners()
            .iter()
            .map(|(id, owner)| {
                (
                    id.clone(),
                    Arc::new(Member {
                        owner: owner.clone(),
                        lifecycle: tokio::sync::Mutex::new(()),
                        state: Mutex::new(MemberState::default()),
                    }),
                )
            })
            .collect();
        Ok(Self {
            shared: Arc::new(Shared {
                source: source.descriptor.id().clone(),
                stream: source.stream.clone(),
                config: source.config.clone(),
                consumers,
                members,
                gate: Arc::new(tokio::sync::Mutex::new(())),
                lifecycle: tokio::sync::Mutex::new(()),
                worker: RwLock::new(None),
                run: Mutex::new(None),
                client: Mutex::new(None),
            }),
        })
    }

    pub fn for_consumer(&self, consumer: &ComponentId) -> Result<Arc<SqliteConsumerSnapshot>> {
        let member = self
            .shared
            .members
            .get(consumer)
            .context("consumer is not in the SQLite snapshot group")?
            .clone();
        Ok(Arc::new(SqliteConsumerSnapshot {
            shared: self.shared.clone(),
            member,
        }))
    }

    pub(super) fn validate_source(&self, source: &SqliteSource) -> Result<()> {
        anyhow::ensure!(
            self.shared.source == *source.descriptor.id()
                && self.shared.stream == source.stream
                && self.shared.config == source.config
                && source
                    .consumer_progress
                    .as_ref()
                    .is_some_and(|owners| Arc::ptr_eq(owners, &self.shared.consumers)),
            "SQLite shared snapshots require their actual source and consumer set"
        );
        Ok(())
    }

    pub(super) fn bind_client(&self, owner: &Arc<tokio::sync::OwnedSemaphorePermit>) -> Result<()> {
        let mut client = self
            .shared
            .client
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite shared client binding poisoned"))?;
        anyhow::ensure!(
            self.shared.current()?.is_none() && client.as_ref().and_then(Weak::upgrade).is_none(),
            "SQLite shared snapshots still belong to another client owner"
        );
        *client = Some(Arc::downgrade(owner));
        Ok(())
    }

    pub(super) fn live_boundary(
        &self,
        progress: &ConsumerProgressSnapshot,
    ) -> Result<(Uuid, OwnedMutexGuard<()>)> {
        let lease = self
            .shared
            .gate
            .clone()
            .try_lock_owned()
            .context("SQLite shared snapshot worker still requires cleanup")?;
        anyhow::ensure!(
            self.shared.current()?.is_none(),
            "SQLite shared snapshot is still active"
        );
        let mut epoch = None;
        for (consumer, member) in &self.shared.members {
            let state = member
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?;
            let handover = state
                .handover
                .as_ref()
                .context("SQLite consumer snapshot has not been prepared")?;
            let owner = progress
                .consumers
                .get(consumer)
                .context("SQLite snapshot consumer membership changed")?;
            anyhow::ensure!(
                !state.cleanup_required && handover.completed && owner.bootstrap_complete,
                "SQLite consumer {consumer} has not committed its snapshot initialization"
            );
            let position = journal::checkpoint(owner, &self.shared.source, handover.epoch)?
                .context("SQLite snapshot consumer has no committed boundary")?;
            anyhow::ensure!(
                position >= handover.sequence,
                "SQLite consumer {consumer} moved behind its snapshot boundary"
            );
            anyhow::ensure!(
                epoch.is_none_or(|epoch| epoch == handover.epoch),
                "SQLite consumers refer to different journal identities"
            );
            epoch = Some(handover.epoch);
        }
        Ok((epoch.context("SQLite snapshot group is empty")?, lease))
    }
}

/// Query-specific view of a shared SQLite initialization worker.
pub struct SqliteConsumerSnapshot {
    shared: Arc<Shared>,
    member: Arc<Member>,
}

struct StartGuard {
    run: Arc<Run>,
    transferred: bool,
}
impl Drop for StartGuard {
    fn drop(&mut self) {
        if !self.transferred {
            self.run.shutdown.send_replace(true);
        }
    }
}
struct Reader {
    shared: Arc<Shared>,
    member: Arc<Member>,
    run: Arc<Run>,
    receiver: mpsc::Receiver<ChangeEnvelope>,
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.run.shutdown.send_replace(true);
    }
}

#[async_trait]
impl ComputationBootstrapProvider for SqliteConsumerSnapshot {
    fn validate_query_scope(&self, _: &str, graph: &str, component: &ComponentId) -> Result<()> {
        anyhow::ensure!(
            graph == self.shared.consumers.graph_id()
                && component == self.member.owner.component_id(),
            "SQLite shared snapshot belongs to another query"
        );
        Ok(())
    }
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        Some(SourceProgressReader::from(self.member.owner.clone()))
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        let _lifecycle = self.member.lifecycle.lock().await;
        anyhow::ensure!(
            !self
                .member
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?
                .cleanup_required,
            "SQLite shared snapshot requires awaited cleanup before preparation"
        );
        state.durability().require(FailureMode::ProcessRestart)?;
        let progress = self.member.owner.snapshot();
        anyhow::ensure!(
            progress.recovered && progress.persistent && progress.failure.is_none(),
            "SQLite shared snapshot requires recovered persistent consumer progress"
        );
        let handover = state
            .read()
            .await?
            .map(|bytes| serde_json::from_slice::<GroupHandover>(&bytes))
            .transpose()?;
        if let Some(handover) = &handover {
            anyhow::ensure!(
                handover.version == 1
                    && handover.binding == self.shared.binding()?
                    && &handover.consumer == self.member.owner.component_id(),
                "SQLite shared snapshot binding changed"
            );
            let checkpoint = journal::checkpoint(&progress, &self.shared.source, handover.epoch)?;
            anyhow::ensure!(
                handover.completed == progress.bootstrap_complete
                    && handover.completed == checkpoint.is_some(),
                "SQLite shared handover and consumer progress disagree"
            );
            if let Some(checkpoint) = checkpoint {
                anyhow::ensure!(
                    checkpoint >= handover.sequence,
                    "SQLite snapshot checkpoint moved backwards"
                );
            }
        } else {
            anyhow::ensure!(
                !progress.bootstrap_complete
                    && !progress
                        .checkpoints
                        .contains_key(&SourceProgressKey::Source(self.shared.source.to_string())),
                "SQLite shared snapshot ownership state is missing"
            );
        }
        let mut member = self
            .member
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?;
        member.handover = handover;
        member.prepared = true;
        Ok(BootstrapPreparation::Ready)
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("SQLite shared snapshot requires query-owned initialization state")
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let _lifecycle = self.member.lifecycle.lock().await;
        let previous = {
            let member = self
                .member
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?;
            anyhow::ensure!(
                member.prepared
                    && !member.cleanup_required
                    && member
                        .handover
                        .as_ref()
                        .is_none_or(|handover| !handover.completed),
                "SQLite shared snapshot requires preparation and an incomplete initialization"
            );
            member.handover.clone()
        };
        let lease = tokio::time::timeout(
            self.shared.config.timeout(),
            self.shared.gate.clone().lock_owned(),
        )
        .await
        .context(
            "SQLite shared snapshot admission timed out; the active worker requires cleanup",
        )?;
        let _global = self.shared.lifecycle.lock().await;
        anyhow::ensure!(
            self.shared.worker.read().await.is_none() && self.shared.current()?.is_none(),
            "SQLite shared snapshot worker requires cleanup"
        );
        let client = self
            .shared
            .client
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite shared client binding poisoned"))?
            .as_ref()
            .map(|client| {
                client
                    .upgrade()
                    .context("SQLite shared snapshot source owner was dropped")
            })
            .transpose()?;
        self.member
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?
            .cleanup_required = true;
        let run = Arc::new(Run {
            consumer: self.member.owner.component_id().clone(),
            shutdown: watch::channel(false).0,
            _lease: lease,
        });
        *self
            .shared
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite group snapshot run poisoned"))? =
            Some(run.clone());
        let mut guard = StartGuard {
            run: run.clone(),
            transferred: false,
        };
        let (prepared, ready) = oneshot::channel();
        let (proceed, continue_read) = oneshot::channel();
        let (outputs, receiver) = mpsc::channel(self.shared.config.output_capacity.get());
        let work = worker::GroupSnapshotWork {
            source: self.shared.source.clone(),
            stream: self.shared.stream.clone(),
            config: self.shared.config.clone(),
            consumers: self.shared.consumers.read_only(),
            member: SourceProgressReader::from(self.member.owner.clone()).read_only(),
            generation: self.member.owner.snapshot().reset_generation,
            previous,
            prepared,
            proceed: continue_read,
            outputs,
            shutdown: run.shutdown.subscribe(),
        };
        let owned_run = run.clone();
        spawn_owned_blocking_worker(&self.shared.worker, move || {
            let _owner = (client, owned_run);
            let result = worker::group_snapshot(work);
            if let Err(error) = &result {
                log::error!("SQLite shared snapshot failed: {error:#}");
            }
            result
        })
        .await?;
        drop(_global);
        let handover = match ready.await {
            Ok(handover) => handover,
            Err(_) => {
                self.shared.join(&run).await?;
                anyhow::bail!("SQLite shared snapshot worker ended before preparation");
            }
        };
        let mut shutdown = run.shutdown.subscribe();
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stopped| *stopped) => anyhow::bail!("SQLite shared snapshot was cancelled"),
            result = state.write(Bytes::from(serde_json::to_vec(&handover)?)) => result?,
        };
        self.member
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?
            .handover = Some(handover);
        proceed
            .send(())
            .map_err(|_| anyhow::anyhow!("SQLite snapshot worker closed before initialization"))?;
        guard.transferred = true;
        let reader = Reader {
            shared: self.shared.clone(),
            member: self.member.clone(),
            run,
            receiver,
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
                    let handover = reader
                        .shared
                        .join(&reader.run)
                        .await?
                        .context("SQLite shared snapshot did not complete")?;
                    let mut state = reader
                        .member
                        .state
                        .lock()
                        .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?;
                    state.handover = Some(handover);
                    state.cleanup_required = false;
                    drop(state);
                    Ok(None)
                },
            )),
            watermarks: Vec::new(),
        })
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        let state = self
            .member
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?;
        let handover = state
            .handover
            .as_ref()
            .context("SQLite shared handover missing")?;
        anyhow::ensure!(
            handover.completed && !state.cleanup_required,
            "SQLite shared snapshot is incomplete"
        );
        Ok(vec![BootstrapWatermark {
            stream: self.shared.stream.clone(),
            source_id: Some(self.shared.source.to_string()),
            sequence: handover.sequence,
            position: Some(journal::identity(handover.epoch, handover.sequence)?),
        }])
    }
    fn completion_state(&self) -> Result<Option<Bytes>> {
        let state = self
            .member
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?;
        let handover = state
            .handover
            .as_ref()
            .context("SQLite shared handover missing")?;
        anyhow::ensure!(
            handover.completed && !state.cleanup_required,
            "SQLite shared snapshot is incomplete"
        );
        Ok(Some(Bytes::from(serde_json::to_vec(handover)?)))
    }
    async fn stop(&self) -> Result<()> {
        if let Some(run) = self
            .shared
            .current()?
            .filter(|run| run.consumer == *self.member.owner.component_id())
        {
            run.shutdown.send_replace(true);
        }
        let _lifecycle = self.member.lifecycle.lock().await;
        if let Some(run) = self
            .shared
            .current()?
            .filter(|run| run.consumer == *self.member.owner.component_id())
        {
            self.shared.join(&run).await?;
        }
        let mut state = self
            .member
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?;
        state.prepared = false;
        state.cleanup_required = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use std::num::{NonZeroU64, NonZeroUsize};

    async fn cancelled_worker_retains_one_client() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source.db");
        let lock = rusqlite::Connection::open(&path)?;
        lock.execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'initial'); BEGIN EXCLUSIVE;"
        )?;
        let size = |value| NonZeroUsize::new(value).expect("test bound");
        let config = SqliteConfig {
            path: Some(path),
            tables: vec![],
            table_keys: vec![],
            output: SqliteOutput::Transactions,
            replay: Some(SqliteReplayConfig {
                max_transactions: size(4),
                max_bytes: size(1 << 20),
            }),
            transactions: SourceTransactionLimits {
                max_changes: size(8),
                max_bytes: size(65536),
                max_duration_ms: NonZeroU64::new(1000).expect("duration"),
            },
            max_sql_bytes: size(65536),
            command_capacity: size(4),
            output_capacity: size(1),
            shutdown_timeout_ms: NonZeroU64::new(10).expect("cleanup bound"),
        };
        let first = Arc::new(QuerySourceProgress::new(
            "sqlite-shared-cleanup",
            ComponentId::try_new("first")?,
        )?);
        let second = Arc::new(QuerySourceProgress::new(
            "sqlite-shared-cleanup",
            ComponentId::try_new("second")?,
        )?);
        let owners = Arc::new(ConsumerProgressSet::new(
            "sqlite-shared-cleanup",
            ComponentId::try_new("sqlite")?,
            [first.clone(), second],
        )?);
        let client = Arc::new(SqliteClientResource::new(
            ComponentId::try_new("sqlite")?,
            config.clone(),
        )?);
        let (source, snapshots) = SqliteSource::coordinated_with_consumers(
            ComponentId::try_new("sqlite")?,
            StreamId::try_new("changes")?,
            config.clone(),
            owners.clone(),
        )?;
        let source = source.with_client_resource(client.clone())?;
        let snapshot = snapshots.for_consumer(first.component_id())?;
        let mut query = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: "sqlite-shared-cleanup".into(),
                id: first.component_id().clone(),
                query: "MATCH (n:items) RETURN n.value AS value".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("first/out")?,
                outbox_capacity: size(4),
            },
            Arc::new(RocksDbComputationProvider::new(
                directory.path().join("query"),
                RocksIndexOptions::new(
                    false,
                    false,
                    RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
                ),
            )),
            QueryOptions::default(),
            QueryExecutionSettings {
                source_transactions: Some(config.transactions),
                ..Default::default()
            },
            None,
        )
        .await?
        .with_source_progress(first)?
        .with_bootstrap(snapshot.clone());
        {
            let start = query.start();
            tokio::pin!(start);
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::select! {
                    result = &mut start => anyhow::bail!("locked snapshot completed unexpectedly: {result:?}"),
                    _ = async {
                        while snapshots.shared.worker.read().await.is_none() { tokio::task::yield_now().await; }
                    } => Ok::<_, anyhow::Error>(()),
                }
            }).await??;
        }
        assert_eq!(
            Arc::strong_count(source.client_owner.as_ref().expect("client owner")),
            2
        );
        drop(source);
        let replacement = || {
            SqliteSource::new_replayable_with_consumers(
                ComponentId::try_new("sqlite").expect("source"),
                StreamId::try_new("changes").expect("stream"),
                config.clone(),
                owners.clone(),
            )?
            .with_client_resource(client.clone())
        };
        assert!(replacement().is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(1), snapshot.stop())
                .await
                .is_err()
        );
        assert!(
            replacement().is_err(),
            "cancelled cleanup must retain the worker lease"
        );
        let before = tokio::time::Instant::now();
        let error = snapshot.stop().await.expect_err("blocked SQLite worker");
        assert!(matches!(
            error.downcast_ref::<drasi_lib::context::workers::WorkerCleanupError>(),
            Some(drasi_lib::context::workers::WorkerCleanupError::TimedOut { .. })
        ));
        assert!(before.elapsed() >= config.timeout());
        assert!(snapshots.shared.worker.read().await.is_some());
        assert!(
            replacement().is_err(),
            "timed-out cleanup must retain the actual worker lease"
        );
        lock.execute_batch("ROLLBACK")?;
        drop(lock);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match snapshot.stop().await {
                    Ok(()) => return Ok(()),
                    Err(error)
                        if matches!(
                            error.downcast_ref::<drasi_lib::context::workers::WorkerCleanupError>(),
                            Some(drasi_lib::context::workers::WorkerCleanupError::TimedOut { .. })
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
        })
        .await??;
        query.stop().await?;
        assert!(snapshots.shared.worker.read().await.is_none());
        assert!(snapshots.shared.current()?.is_none());
        let mut replacement = replacement()?;
        replacement.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn group_snapshot_cancelled_cleanup_retains_client_current_thread() -> Result<()> {
        cancelled_worker_retains_one_client().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn group_snapshot_cancelled_cleanup_retains_client_multi_thread() -> Result<()> {
        cancelled_worker_retains_one_client().await
    }
}
