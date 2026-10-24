// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    component::{Progress, SqliteSource},
    Configuration,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_contracts::{
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ChangeEnvelope, ComponentId,
    ComputationBootstrapSnapshot, SourceProgressView, StreamId,
};
use drasi_computation_plugin_sdk::{
    Component, ControlSender, CreateRequest, CreatedComponent, NativeBootstrapProvider,
    NativeSourceBootstrapCoordinator, SourceBootstrapLiveProgress, SourceBootstrapProgress,
};
use drasi_sqlite_common::native::{
    consumer_snapshot::{SqliteConsumerSnapshot, SqliteConsumerSnapshots},
    progress::Observed,
    snapshot::SqliteSnapshot,
};
use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};
use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

enum Snapshots {
    Single(Arc<SqliteSnapshot>),
    Shared(Arc<SqliteConsumerSnapshots>),
}

impl Snapshots {
    fn bind_client(&self, owner: &Arc<OwnedSemaphorePermit>) -> Result<()> {
        match self {
            Self::Single(snapshot) => snapshot.bind_client(owner),
            Self::Shared(snapshots) => snapshots.bind_client(owner),
        }
    }

    fn ensure_idle(&self) -> Result<()> {
        match self {
            Self::Single(snapshot) => snapshot.ensure_idle(),
            Self::Shared(snapshots) => snapshots.ensure_idle(),
        }
    }
}

pub(crate) struct SourceBinding {
    snapshots: Arc<Snapshots>,
    pub owner: Arc<OwnedSemaphorePermit>,
    retired: Arc<AtomicBool>,
}

impl SourceBinding {
    pub fn ensure_active(&self) -> Result<()> {
        anyhow::ensure!(
            !self.retired.load(Ordering::Acquire),
            "SQLite source/bootstrap binding is retired"
        );
        Ok(())
    }

    pub fn live_boundary(&self, progress: &Observed) -> Result<(Uuid, OwnedMutexGuard<()>)> {
        self.ensure_active()?;
        match (self.snapshots.as_ref(), progress) {
            (Snapshots::Single(snapshot), Observed::Single(progress)) => {
                snapshot.live_boundary(progress)
            }
            (Snapshots::Shared(snapshots), Observed::Group(progress)) => {
                snapshots.live_boundary(progress)
            }
            _ => anyhow::bail!("SQLite source/bootstrap progress mode changed"),
        }
    }
}

pub(crate) struct Coordinator {
    request: CreateRequest,
    members: BTreeMap<ComponentId, SourceProgressView>,
    snapshots: Arc<Snapshots>,
    source_owner: Arc<Semaphore>,
    current_source: Arc<Mutex<Weak<OwnedSemaphorePermit>>>,
    retired: Arc<AtomicBool>,
}

impl Coordinator {
    pub fn new(request: &CreateRequest, progress: SourceBootstrapProgress) -> Result<Self> {
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        anyhow::ensure!(
            config.coordinated_snapshot,
            "SQLite coordination must be explicitly configured"
        );
        let scope = request
            .scope
            .as_ref()
            .context("SQLite coordination requires graph scope")?;
        let stream = StreamId::try_new(config.stream)?;
        let (members, snapshots) = match progress {
            SourceBootstrapProgress::Single(reader) => {
                anyhow::ensure!(
                    reader.graph_id() == scope.graph_id && reader.component_id() != &request.id,
                    "SQLite coordinator progress belongs to another graph or source"
                );
                let members = BTreeMap::from([(reader.component_id().clone(), reader.clone())]);
                let snapshot =
                    SqliteSnapshot::new(request.id.clone(), stream, config.settings, reader)?;
                (members, Snapshots::Single(Arc::new(snapshot)))
            }
            SourceBootstrapProgress::Shared { consumers, members } => {
                anyhow::ensure!(
                    consumers.graph_id() == scope.graph_id
                        && consumers.producer_id() == &request.id,
                    "SQLite coordinator consumer set belongs to another graph or source"
                );
                let snapshots = SqliteConsumerSnapshots::new(
                    request.id.clone(),
                    stream,
                    config.settings,
                    consumers,
                    members.clone(),
                )?;
                (members, Snapshots::Shared(Arc::new(snapshots)))
            }
        };
        Ok(Self {
            request: request.clone(),
            members,
            snapshots: Arc::new(snapshots),
            source_owner: Arc::new(Semaphore::new(1)),
            current_source: Arc::new(Mutex::new(Weak::new())),
            retired: Arc::new(AtomicBool::new(false)),
        })
    }

    fn ensure_active(&self) -> Result<()> {
        anyhow::ensure!(
            !self.retired.load(Ordering::Acquire),
            "SQLite source/bootstrap binding is retired"
        );
        Ok(())
    }

    fn validate_request(&self, request: &CreateRequest) -> Result<()> {
        self.ensure_active()?;
        let expected = self
            .request
            .scope
            .as_ref()
            .context("SQLite coordinator scope is missing")?;
        let actual = request
            .scope
            .as_ref()
            .context("SQLite source scope is missing")?;
        anyhow::ensure!(
            request.id == self.request.id
                && request.implementation == self.request.implementation
                && request.configuration_version == self.request.configuration_version
                && request.configuration == self.request.configuration
                && actual.instance_id == expected.instance_id
                && actual.graph_id == expected.graph_id,
            "SQLite source does not match its actual coordinator binding"
        );
        Ok(())
    }
}

#[async_trait]
impl NativeSourceBootstrapCoordinator for Coordinator {
    fn create_source(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        live: SourceBootstrapLiveProgress,
    ) -> Result<CreatedComponent> {
        self.validate_request(request)?;
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        let scope = request
            .scope
            .as_ref()
            .context("SQLite source scope is missing")?;
        let progress = match (self.snapshots.as_ref(), live) {
            (Snapshots::Single(_), SourceBootstrapLiveProgress::Single(reader)) => {
                anyhow::ensure!(
                    reader.graph_id() == scope.graph_id
                        && self.members.contains_key(reader.component_id()),
                    "SQLite live progress does not match the coordinator owner"
                );
                Progress::Single(reader)
            }
            (Snapshots::Shared(_), SourceBootstrapLiveProgress::Shared(reader)) => {
                anyhow::ensure!(
                    reader.graph_id() == scope.graph_id && reader.producer_id() == &request.id,
                    "SQLite live consumer set does not match the coordinator source"
                );
                Progress::Shared {
                    reader,
                    members: self.members.keys().cloned().collect::<Vec<_>>().into(),
                }
            }
            _ => anyhow::bail!("SQLite source/bootstrap live progress mode changed"),
        };
        self.snapshots.ensure_idle()?;
        let owner =
            Arc::new(self.source_owner.clone().try_acquire_owned().context(
                "SQLite coordinator still belongs to another source or unfinished worker",
            )?);
        self.snapshots.bind_client(&owner)?;
        *self
            .current_source
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite source lease poisoned"))? = Arc::downgrade(&owner);
        let mut source = SqliteSource::build(
            request.id.clone(),
            StreamId::try_new(config.stream)?,
            config.settings,
            progress,
        )?
        .with_coordinator(SourceBinding {
            snapshots: self.snapshots.clone(),
            owner,
            retired: self.retired.clone(),
        });
        if let Some(rest) = config.rest_api {
            source = source.with_rest_api(rest)?;
        }
        self.ensure_active()?;
        Ok(Component::Source(Box::new(source)).into())
    }

    fn open(
        &self,
        query: &ComponentId,
        progress: SourceProgressView,
    ) -> Result<Arc<dyn NativeBootstrapProvider>> {
        self.ensure_active()?;
        let expected = self
            .members
            .get(query)
            .context("query is not a SQLite coordinator consumer")?;
        anyhow::ensure!(
            expected.same_owner(&progress),
            "SQLite bootstrap requires its captured actual query reader"
        );
        let snapshot = match self.snapshots.as_ref() {
            Snapshots::Single(snapshot) => {
                snapshot.ensure_bindable()?;
                ProviderSnapshot::Single(snapshot.clone())
            }
            Snapshots::Shared(snapshots) => {
                let snapshot = snapshots.for_consumer(query)?;
                snapshot.ensure_bindable()?;
                ProviderSnapshot::Shared(snapshot)
            }
        };
        self.ensure_active()?;
        Ok(Arc::new(Provider {
            snapshot,
            progress: expected.clone(),
            retired: self.retired.clone(),
            instance: self
                .request
                .scope
                .as_ref()
                .context("SQLite scope is missing")?
                .instance_id
                .clone(),
            source: self.current_source.clone(),
        }))
    }

    async fn stop(&self) -> Result<()> {
        self.retired.store(true, Ordering::Release);
        match self.snapshots.as_ref() {
            Snapshots::Single(snapshot) => snapshot.stop().await?,
            Snapshots::Shared(snapshots) => {
                for query in self.members.keys() {
                    snapshots.for_consumer(query)?.stop().await?;
                }
            }
        }
        self.snapshots.ensure_idle()
    }
}

enum ProviderSnapshot {
    Single(Arc<SqliteSnapshot>),
    Shared(Arc<SqliteConsumerSnapshot>),
}

struct Provider {
    snapshot: ProviderSnapshot,
    progress: SourceProgressView,
    retired: Arc<AtomicBool>,
    instance: String,
    source: Arc<Mutex<Weak<OwnedSemaphorePermit>>>,
}

impl Provider {
    fn ensure_active(&self) -> Result<()> {
        anyhow::ensure!(
            !self.retired.load(Ordering::Acquire),
            "SQLite source/bootstrap binding is retired"
        );
        Ok(())
    }

    fn ensure_source_owner(&self) -> Result<()> {
        self.ensure_active()?;
        anyhow::ensure!(
            self.source
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite source lease poisoned"))?
                .upgrade()
                .is_some(),
            "SQLite paired bootstrap has no actual source owner"
        );
        Ok(())
    }
}

#[async_trait]
impl NativeBootstrapProvider for Provider {
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        Some(self.progress.clone())
    }
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        self.ensure_active()?;
        anyhow::ensure!(
            instance == self.instance
                && graph == self.progress.graph_id()
                && query == self.progress.component_id(),
            "SQLite paired bootstrap belongs to another query"
        );
        Ok(())
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.ensure_active()?;
        match &self.snapshot {
            ProviderSnapshot::Single(snapshot) => snapshot.prepare_with_state(state).await,
            ProviderSnapshot::Shared(snapshot) => snapshot.prepare_with_state(state).await,
        }
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("SQLite paired bootstrap requires its actual query-owned state")
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        self.ensure_source_owner()?;
        let changes: Pin<Box<dyn futures::Stream<Item = Result<ChangeEnvelope>> + Send>> =
            match &self.snapshot {
                ProviderSnapshot::Single(snapshot) => {
                    let reader = snapshot.snapshot_with_state(state).await?;
                    Box::pin(futures::stream::try_unfold(
                        reader,
                        |mut reader| async move {
                            Ok(reader.next().await?.map(|change| (change, reader)))
                        },
                    ))
                }
                ProviderSnapshot::Shared(snapshot) => {
                    let reader = snapshot.snapshot_with_state(state).await?;
                    Box::pin(futures::stream::try_unfold(
                        reader,
                        |mut reader| async move {
                            Ok(reader.next().await?.map(|change| (change, reader)))
                        },
                    ))
                }
            };
        Ok(ComputationBootstrapSnapshot {
            changes,
            watermarks: Vec::new(),
        })
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.ensure_active()?;
        match &self.snapshot {
            ProviderSnapshot::Single(snapshot) => snapshot.complete_snapshot().await,
            ProviderSnapshot::Shared(snapshot) => snapshot.complete_snapshot().await,
        }
    }
    fn completion_state(&self) -> Result<Option<Bytes>> {
        self.ensure_active()?;
        match &self.snapshot {
            ProviderSnapshot::Single(snapshot) => snapshot.completion_state(),
            ProviderSnapshot::Shared(snapshot) => snapshot.completion_state(),
        }
    }
    async fn stop(&self) -> Result<()> {
        match &self.snapshot {
            ProviderSnapshot::Single(snapshot) => snapshot.stop().await,
            ProviderSnapshot::Shared(snapshot) => snapshot.stop().await,
        }
    }
}
