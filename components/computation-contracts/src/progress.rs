// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{ComponentId, StreamId};
use async_trait::async_trait;
use drasi_core_types::interface::SourceCheckpoint;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceProgressKey {
    Source(String),
    Stream(StreamId),
}

/// A read-only view of an owning query or durable middleware's committed input
/// progress. Persistence comes from that component's actual checkpoint provider,
/// not from this watch channel. A non-immediate owner requires an asserted,
/// validated path through identity-preserving stateless components.
#[derive(Debug, Clone, Default)]
pub struct SourceProgressSnapshot {
    pub ready: bool,
    /// All source resume/reset preparation completed for this generation. Live
    /// inputs may now buffer, but evaluation still waits for bootstrap readiness.
    pub admitting: bool,
    pub recovered: bool,
    pub bootstrap_complete: bool,
    pub persistent: bool,
    pub reset_generation: u64,
    pub checkpoints: BTreeMap<SourceProgressKey, SourceCheckpoint>,
    /// Consumed transport positions are not source resume cursors. Replayed
    /// logical input can advance these without advancing `checkpoints`.
    pub transport_sequences: BTreeMap<StreamId, u64>,
    pub failure: Option<Arc<str>>,
}

/// A subscription to committed progress. Reads mark the returned state observed;
/// changes may coalesce, but cancelling `changed` must not consume an update.
#[async_trait]
pub trait SourceProgressUpdates: Send + Sync {
    fn snapshot(&mut self) -> anyhow::Result<Arc<SourceProgressSnapshot>>;
    async fn changed(&mut self) -> anyhow::Result<()>;
}

#[async_trait]
impl SourceProgressUpdates for watch::Receiver<Arc<SourceProgressSnapshot>> {
    fn snapshot(&mut self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        Ok(self.borrow_and_update().clone())
    }

    async fn changed(&mut self) -> anyhow::Result<()> {
        Ok(watch::Receiver::changed(self).await?)
    }
}

/// Read-only progress supplied by another boundary, such as a plugin host.
/// Implementations must surface revocation and read errors, not return a default
/// snapshot. Synchronous reads must be bounded and perform no storage or network
/// I/O. Subscriptions own their observation state without forwarding tasks.
pub trait SourceProgressProvider: Send + Sync + 'static {
    fn graph_id(&self) -> &str;
    fn component_id(&self) -> &ComponentId;
    fn snapshot(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>>;
    fn subscribe(&self) -> anyhow::Result<Box<dyn SourceProgressUpdates>>;
}

/// Read-only progress capability, without access to the host's transaction owner.
///
/// ```compile_fail
/// use drasi_computation_contracts::SourceProgressView;
/// fn obtain_local_owner(view: &SourceProgressView) {
///     let _ = view.local_owner();
/// }
/// ```
#[derive(Clone)]
pub struct SourceProgressView(Arc<dyn SourceProgressProvider>);

impl SourceProgressView {
    pub fn new(provider: Arc<dyn SourceProgressProvider>) -> Self {
        Self(provider)
    }

    pub fn graph_id(&self) -> &str {
        self.0.graph_id()
    }
    pub fn component_id(&self) -> &ComponentId {
        self.0.component_id()
    }
    pub fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn snapshot(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        self.0.snapshot()
    }
    pub fn subscribe(&self) -> anyhow::Result<Box<dyn SourceProgressUpdates>> {
        self.0.subscribe()
    }
    pub async fn wait_recovered(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        self.wait_for(|snapshot| snapshot.recovered).await
    }
    pub async fn wait_ready(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        self.wait_for(|snapshot| snapshot.ready).await
    }
    async fn wait_for(
        &self,
        predicate: impl Fn(&SourceProgressSnapshot) -> bool,
    ) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        let mut updates = self.subscribe()?;
        loop {
            let snapshot = updates.snapshot()?;
            if let Some(failure) = &snapshot.failure {
                anyhow::bail!("{failure}");
            }
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            updates.changed().await?;
        }
    }
}

impl SourceProgressProvider for SourceProgressView {
    fn graph_id(&self) -> &str {
        self.0.graph_id()
    }
    fn component_id(&self) -> &ComponentId {
        self.0.component_id()
    }
    fn snapshot(&self) -> anyhow::Result<Arc<SourceProgressSnapshot>> {
        self.0.snapshot()
    }
    fn subscribe(&self) -> anyhow::Result<Box<dyn SourceProgressUpdates>> {
        self.0.subscribe()
    }
}
