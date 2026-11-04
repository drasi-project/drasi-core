// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::Utc;
use drasi_computation_contracts::WakeupSource;
use drasi_core::interface::FutureQueue;

/// A scheduling hint published after its actual queue transaction commits.
/// The Core future queue, not this observation, owns and consumes due work.
#[derive(Clone, Default)]
pub struct QuerySchedule(Arc<RwLock<Option<u64>>>);

impl QuerySchedule {
    pub fn publish(&self, deadline: Option<u64>) -> anyhow::Result<()> {
        *self
            .0
            .write()
            .map_err(|_| anyhow::anyhow!("query schedule observation poisoned"))? = deadline;
        Ok(())
    }

    pub fn next_due(&self) -> anyhow::Result<Option<u64>> {
        Ok(*self
            .0
            .read()
            .map_err(|_| anyhow::anyhow!("query schedule observation poisoned"))?)
    }

    pub fn wakeup_source(&self) -> Arc<dyn WakeupSource> {
        Arc::new(FutureWakeup {
            deadline: Deadline::Committed(self.clone()),
        })
    }
}

enum Deadline {
    Queue(Arc<dyn FutureQueue>),
    Committed(QuerySchedule),
}

/// The existing query wakeup policy, shared by linked and native adapters.
pub struct FutureWakeup {
    deadline: Deadline,
}

impl FutureWakeup {
    pub fn for_queue(queue: Arc<dyn FutureQueue>) -> Self {
        Self {
            deadline: Deadline::Queue(queue),
        }
    }

    async fn next_due(&self) -> anyhow::Result<Option<u64>> {
        match &self.deadline {
            Deadline::Queue(queue) => Ok(queue.peek_due_time().await?),
            Deadline::Committed(schedule) => schedule.next_due(),
        }
    }
}

#[async_trait]
impl WakeupSource for FutureWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        loop {
            if let Some(due) = self.next_due().await? {
                let now = u64::try_from(Utc::now().timestamp_millis())
                    .map_err(|_| anyhow::anyhow!("clock before epoch"))?;
                if due <= now {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis((due - now).min(25))).await;
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
    }

    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(self.next_due().await?.is_some())
    }
}
