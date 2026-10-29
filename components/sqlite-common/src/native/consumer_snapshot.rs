// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    journal::{self, GroupHandover},
    worker, SqliteConfig,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_computation_contracts::{
    workers::{join_owned_worker_gracefully, spawn_owned_blocking_worker, WorkerCompletion},
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ChangeEnvelope, ComponentId,
    ConsumerProgressSnapshot, ConsumerProgressView, SourceProgressKey, SourceProgressView,
    StreamId, MAX_CONSUMER_PROGRESS_MEMBERS,
};
use drasi_core_types::interface::FailureMode;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::{
    sync::{mpsc, oneshot, watch, OwnedMutexGuard, RwLock},
    task::JoinHandle,
};
use uuid::Uuid;

#[derive(Default)]
struct MemberState {
    prepared: bool,
    cleanup_required: bool,
    handover: Option<GroupHandover>,
}
struct Member {
    owner: SourceProgressView,
    lifecycle: tokio::sync::Mutex<()>,
    state: Mutex<MemberState>,
}
struct Run {
    consumer: ComponentId,
    shutdown: watch::Sender<bool>,
    lease: Mutex<Option<OwnedMutexGuard<()>>>,
}
struct Shared {
    source: ComponentId,
    stream: StreamId,
    config: SqliteConfig,
    consumers: ConsumerProgressView,
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
            run.lease
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite shared snapshot lease poisoned"))?
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
            &self.consumers,
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
    pub fn new(
        source: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        consumers: ConsumerProgressView,
        owners: BTreeMap<ComponentId, SourceProgressView>,
    ) -> Result<Self> {
        config.validate()?;
        anyhow::ensure!(
            config.path.is_some()
                && config.replay.is_some()
                && config.output == super::SqliteOutput::Transactions,
            "SQLite coordinated snapshots require a file, journal bounds and complete transactions"
        );
        anyhow::ensure!(
            consumers.producer_id() == &source
                && !owners.is_empty()
                && owners.len() <= MAX_CONSUMER_PROGRESS_MEMBERS
                && owners.iter().all(|(id, owner)| {
                    id == owner.component_id()
                        && owner.graph_id() == consumers.graph_id()
                        && id != &source
                }),
            "SQLite shared snapshot requires its fixed scoped consumer readers"
        );
        let members = owners
            .into_iter()
            .map(|(id, owner)| {
                (
                    id,
                    Arc::new(Member {
                        owner,
                        lifecycle: tokio::sync::Mutex::new(()),
                        state: Mutex::new(MemberState::default()),
                    }),
                )
            })
            .collect();
        Ok(Self {
            shared: Arc::new(Shared {
                source,
                stream,
                config,
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

    pub fn bind_client(&self, owner: &Arc<tokio::sync::OwnedSemaphorePermit>) -> Result<()> {
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

    pub fn live_boundary(
        &self,
        progress: &ConsumerProgressSnapshot,
    ) -> Result<(Uuid, OwnedMutexGuard<()>)> {
        anyhow::ensure!(
            progress.consumers.keys().eq(self.shared.members.keys()),
            "SQLite snapshot consumer membership changed"
        );
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

    pub async fn owns_worker(&self) -> bool {
        self.shared.worker.read().await.is_some()
    }

    pub fn has_active_run(&self) -> Result<bool> {
        Ok(self.shared.current()?.is_some())
    }

    pub fn ensure_idle(&self) -> Result<()> {
        let _lease = self
            .shared
            .gate
            .clone()
            .try_lock_owned()
            .context("SQLite snapshot/live worker still requires cleanup")?;
        anyhow::ensure!(
            self.shared.current()?.is_none(),
            "SQLite snapshot worker is not joined"
        );
        Ok(())
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
pub struct SnapshotReader {
    shared: Arc<Shared>,
    member: Arc<Member>,
    run: Arc<Run>,
    receiver: mpsc::Receiver<ChangeEnvelope>,
}
impl SnapshotReader {
    pub async fn next(&mut self) -> Result<Option<ChangeEnvelope>> {
        self.shared.ensure_run(&self.run)?;
        if let Some(change) = self.receiver.recv().await {
            self.shared.ensure_run(&self.run)?;
            return Ok(Some(change));
        }
        let handover = self
            .shared
            .join(&self.run)
            .await?
            .context("SQLite shared snapshot did not complete")?;
        let mut state = self
            .member
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?;
        state.handover = Some(handover);
        state.cleanup_required = false;
        Ok(None)
    }
}

impl Drop for SnapshotReader {
    fn drop(&mut self) {
        self.run.shutdown.send_replace(true);
    }
}

impl SqliteConsumerSnapshot {
    pub fn ensure_bindable(&self) -> Result<()> {
        anyhow::ensure!(
            !self
                .member
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite snapshot member poisoned"))?
                .cleanup_required,
            "SQLite shared snapshot still requires awaited cleanup before rebinding"
        );
        Ok(())
    }

    pub fn validate_query_scope(
        &self,
        _: &str,
        graph: &str,
        component: &ComponentId,
    ) -> Result<()> {
        anyhow::ensure!(
            graph == self.shared.consumers.graph_id()
                && component == self.member.owner.component_id(),
            "SQLite shared snapshot belongs to another query"
        );
        Ok(())
    }
    pub fn recovery_reader(&self) -> SourceProgressView {
        self.member.owner.clone()
    }
    pub async fn prepare_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<BootstrapPreparation> {
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
        let progress = self.member.owner.snapshot()?;
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
    pub async fn snapshot_with_state(&self, state: &dyn BootstrapState) -> Result<SnapshotReader> {
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
            lease: Mutex::new(Some(lease)),
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
            consumers: self.shared.consumers.clone(),
            member: self.member.owner.clone(),
            generation: self.member.owner.snapshot()?.reset_generation,
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
        Ok(SnapshotReader {
            shared: self.shared.clone(),
            member: self.member.clone(),
            run,
            receiver,
        })
    }
    pub async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
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
            producer: None,
            stream: self.shared.stream.clone(),
            source_id: Some(self.shared.source.to_string()),
            sequence: handover.sequence,
            position: Some(journal::identity(handover.epoch, handover.sequence)?),
        }])
    }
    pub fn completion_state(&self) -> Result<Option<Bytes>> {
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
    pub async fn stop(&self) -> Result<()> {
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
