// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{ComponentId, SourceProgressSnapshot};
use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Arc};

pub const MAX_CONSUMER_PROGRESS_MEMBERS: usize = 256;

/// Separate observations of every required consumer's committed input.
///
/// There is deliberately no aggregate cursor or durability flag. A producer
/// interprets its own opaque positions, including partitioned positions, and
/// must not omit a member that has not yet established its checkpoint.
#[derive(Debug, Clone)]
pub struct ConsumerProgressSnapshot {
    pub consumers: BTreeMap<ComponentId, Arc<SourceProgressSnapshot>>,
}

impl ConsumerProgressSnapshot {
    pub fn ready(&self) -> bool {
        !self.consumers.is_empty()
            && self
                .consumers
                .values()
                .all(|state| state.ready && state.failure.is_none())
    }

    pub fn recovered(&self) -> bool {
        !self.consumers.is_empty()
            && self
                .consumers
                .values()
                .all(|state| state.recovered && state.failure.is_none())
    }

    fn check_failures(&self) -> anyhow::Result<()> {
        for (consumer, state) in &self.consumers {
            if let Some(failure) = &state.failure {
                anyhow::bail!("consumer {consumer} cannot provide recovery progress: {failure}");
            }
        }
        Ok(())
    }
}

/// Bounded observations without a forwarding task. Cancelling a wait must not
/// consume an update; snapshot reads mark the returned observations seen.
#[async_trait]
pub trait ConsumerProgressUpdates: Send + Sync {
    fn snapshot(&mut self) -> anyhow::Result<ConsumerProgressSnapshot>;
    async fn changed(&mut self) -> anyhow::Result<()>;
}

/// A read capability, not authority to acknowledge, remove or reset consumers.
/// Membership is fixed for this binding; replacing it requires host lifecycle
/// coordination rather than dropping the slowest member from an observation.
pub trait ConsumerProgressProvider: Send + Sync + 'static {
    fn graph_id(&self) -> &str;
    fn producer_id(&self) -> &ComponentId;
    fn snapshot(&self) -> anyhow::Result<ConsumerProgressSnapshot>;
    fn subscribe(&self) -> anyhow::Result<Box<dyn ConsumerProgressUpdates>>;
}

#[derive(Clone)]
pub struct ConsumerProgressView(Arc<dyn ConsumerProgressProvider>);

impl ConsumerProgressView {
    pub fn new(provider: Arc<dyn ConsumerProgressProvider>) -> Self {
        Self(provider)
    }

    pub fn graph_id(&self) -> &str {
        self.0.graph_id()
    }

    pub fn producer_id(&self) -> &ComponentId {
        self.0.producer_id()
    }

    pub fn same_binding(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn snapshot(&self) -> anyhow::Result<ConsumerProgressSnapshot> {
        self.0.snapshot()
    }

    pub fn subscribe(&self) -> anyhow::Result<Box<dyn ConsumerProgressUpdates>> {
        self.0.subscribe()
    }

    pub async fn wait_ready(&self) -> anyhow::Result<ConsumerProgressSnapshot> {
        self.wait_for(ConsumerProgressSnapshot::ready).await
    }

    pub async fn wait_recovered(&self) -> anyhow::Result<ConsumerProgressSnapshot> {
        self.wait_for(ConsumerProgressSnapshot::recovered).await
    }

    async fn wait_for(
        &self,
        predicate: impl Fn(&ConsumerProgressSnapshot) -> bool,
    ) -> anyhow::Result<ConsumerProgressSnapshot> {
        let mut updates = self.subscribe()?;
        loop {
            let snapshot = updates.snapshot()?;
            snapshot.check_failures()?;
            if predicate(&snapshot) {
                return Ok(snapshot);
            }
            updates.changed().await?;
        }
    }
}
