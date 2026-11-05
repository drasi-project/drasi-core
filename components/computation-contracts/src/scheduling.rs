// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Transaction-owned scheduled work and committed deadline observations.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use drasi_core_types::interface::FutureQueue;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::WakeupSource;

/// Maximum UTF-8 byte length of a component-local scheduled-work key.
pub const MAX_SCHEDULED_KEY_BYTES: usize = 256;
/// Maximum opaque payload retained with one request.
pub const MAX_SCHEDULED_PAYLOAD_BYTES: usize = 65_536;
/// Maximum outstanding requests belonging to one component.
pub const MAX_SCHEDULED_REQUESTS: usize = 4096;
/// Maximum encoded scheduling checkpoint, including request metadata.
pub const MAX_SCHEDULED_STATE_BYTES: usize = 1024 * 1024;

/// A request in the component's own processing transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledRequest {
    /// Stable, component-local business key; requesting an occupied key fails.
    pub key: String,
    /// Physical eligibility time, in milliseconds since the Unix epoch.
    pub due_time_ms: u64,
    /// Component-defined state needed to perform the work after reconstruction.
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
}

impl ScheduledRequest {
    /// Validate the shared request bounds before touching storage.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_key(&self.key)?;
        anyhow::ensure!(
            self.payload.len() <= MAX_SCHEDULED_PAYLOAD_BYTES,
            "scheduled payload exceeds its byte limit"
        );
        Ok(())
    }
}

/// The exact incarnation of a request. Replacement assigns a fresh identity.
///
/// The random identity also prevents a rolled-back request from matching a
/// later request that reused the same component-local key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledWorkToken {
    /// The component-local request key.
    pub key: String,
    /// The request incarnation, assigned by the transaction service.
    pub request_id: Uuid,
}

impl ScheduledWorkToken {
    /// Reject malformed or uninitialized identities.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_key(&self.key)?;
        anyhow::ensure!(
            !self.request_id.is_nil(),
            "scheduled request identity is nil"
        );
        Ok(())
    }
}

/// Persisted work returned by request, lookup, replacement or a due-work claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledWork {
    /// Identity required by replacement, cancellation and completion.
    pub token: ScheduledWorkToken,
    /// Physical eligibility time, in milliseconds since the Unix epoch.
    pub due_time_ms: u64,
    /// The requesting component's bounded opaque payload.
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
}

impl ScheduledWork {
    /// Construct a fresh incarnation after validating a request.
    pub fn try_new(request: ScheduledRequest) -> anyhow::Result<Self> {
        request.validate()?;
        Ok(Self {
            token: ScheduledWorkToken {
                key: request.key,
                request_id: Uuid::new_v4(),
            },
            due_time_ms: request.due_time_ms,
            payload: request.payload,
        })
    }

    /// Validate recovered or received work without changing its identity.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.token.validate()?;
        anyhow::ensure!(
            self.payload.len() <= MAX_SCHEDULED_PAYLOAD_BYTES,
            "scheduled payload exceeds its byte limit"
        );
        Ok(())
    }
}

/// Validate a component-local scheduling key.
pub fn validate_scheduled_key(key: &str) -> anyhow::Result<()> {
    validate_key(key)
}

fn validate_key(key: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !key.is_empty() && key.len() <= MAX_SCHEDULED_KEY_BYTES && !key.contains('\0'),
        "scheduled key must contain 1..={MAX_SCHEDULED_KEY_BYTES} bytes without NUL"
    );
    Ok(())
}

/// Physical eligibility time, separate from an input's logical event time.
pub trait PhysicalClock: Send + Sync {
    /// Return milliseconds since the Unix epoch.
    fn now(&self) -> anyhow::Result<u64>;
}

/// The production wall clock used when no explicit clock is supplied.
#[derive(Default)]
pub struct SystemPhysicalClock;

impl PhysicalClock for SystemPhysicalClock {
    fn now(&self) -> anyhow::Result<u64> {
        u64::try_from(chrono::Utc::now().timestamp_millis())
            .map_err(|_| anyhow::anyhow!("clock before epoch"))
    }
}

struct Schedule {
    deadline: RwLock<Option<u64>>,
    clock: Arc<dyn PhysicalClock>,
}

/// A deadline hint published only after the actual queue transaction commits.
///
/// This object owns no storage and starts no task. The Core future queue owns
/// requests, and the graph polls the returned wakeup source.
#[derive(Clone)]
pub struct CommittedSchedule(Arc<Schedule>);

impl Default for CommittedSchedule {
    fn default() -> Self {
        Self::with_clock(Arc::new(SystemPhysicalClock))
    }
}

impl CommittedSchedule {
    /// Use an explicit physical clock without changing logical event timestamps.
    pub fn with_clock(clock: Arc<dyn PhysicalClock>) -> Self {
        Self(Arc::new(Schedule {
            deadline: RwLock::new(None),
            clock,
        }))
    }

    /// Publish the earliest deadline after successful storage commit.
    pub fn publish(&self, deadline: Option<u64>) -> anyhow::Result<()> {
        *self
            .0
            .deadline
            .write()
            .map_err(|_| anyhow::anyhow!("schedule observation poisoned"))? = deadline;
        Ok(())
    }

    /// Read the last published committed deadline, never staged queue changes.
    pub fn next_due(&self) -> anyhow::Result<Option<u64>> {
        Ok(*self
            .0
            .deadline
            .read()
            .map_err(|_| anyhow::anyhow!("schedule observation poisoned"))?)
    }

    /// Read the same physical clock used by this observation's wakeup policy.
    pub fn physical_time(&self) -> anyhow::Result<u64> {
        self.0.clock.now()
    }

    /// Observe deadlines from the existing graph-owned wakeup operation.
    pub fn wakeup_source(&self) -> Arc<dyn WakeupSource> {
        Arc::new(FutureWakeup {
            deadline: Deadline::Committed(self.clone()),
        })
    }
}

enum Deadline {
    Queue(Arc<dyn FutureQueue>),
    Committed(CommittedSchedule),
}

/// The shared linked-query, native-query and native scheduled-work wakeup policy.
pub struct FutureWakeup {
    deadline: Deadline,
}

impl FutureWakeup {
    /// Observe a queue whose provider exposes committed deadline reads.
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

    fn physical_time(&self) -> anyhow::Result<u64> {
        match &self.deadline {
            Deadline::Queue(_) => SystemPhysicalClock.now(),
            Deadline::Committed(schedule) => schedule.physical_time(),
        }
    }
}

#[async_trait]
impl WakeupSource for FutureWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        loop {
            if let Some(due) = self.next_due().await? {
                let now = self.physical_time()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    struct Clock(AtomicU64);
    impl PhysicalClock for Clock {
        fn now(&self) -> anyhow::Result<u64> {
            Ok(self.0.load(Ordering::Acquire))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn scheduling_observation_uses_the_physical_clock_without_consuming_work(
    ) -> anyhow::Result<()> {
        let clock = Arc::new(Clock(AtomicU64::new(1000)));
        let schedule = CommittedSchedule::with_clock(clock.clone());
        let source = schedule.wakeup_source();
        let mut wait = Box::pin(source.wait());
        assert!(futures::poll!(&mut wait).is_pending());
        schedule.publish(Some(2000))?;
        tokio::time::advance(Duration::from_millis(25)).await;
        assert!(futures::poll!(&mut wait).is_pending());
        clock.0.store(500, Ordering::Release);
        tokio::time::advance(Duration::from_millis(25)).await;
        assert!(
            futures::poll!(&mut wait).is_pending(),
            "backward time must not fire"
        );
        clock.0.store(2000, Ordering::Release);
        tokio::time::advance(Duration::from_millis(25)).await;
        wait.await?;
        assert_eq!(
            schedule.next_due()?,
            Some(2000),
            "observations never complete work"
        );
        assert!(source.has_pending().await?);
        schedule.publish(None)?;
        assert!(!source.has_pending().await?);
        Ok(())
    }

    #[test]
    fn scheduling_requests_are_bounded_and_incarnations_are_distinct() -> anyhow::Result<()> {
        let request = ScheduledRequest {
            key: "key".into(),
            due_time_ms: 0,
            payload: vec![1],
        };
        let first = ScheduledWork::try_new(request.clone())?;
        let second = ScheduledWork::try_new(request.clone())?;
        assert_ne!(first.token, second.token);
        first.validate()?;
        for key in [
            "".into(),
            "bad\0key".into(),
            "a".repeat(MAX_SCHEDULED_KEY_BYTES + 1),
        ] {
            assert!(ScheduledRequest {
                key,
                ..request.clone()
            }
            .validate()
            .is_err());
        }
        assert!(ScheduledRequest {
            payload: vec![0; MAX_SCHEDULED_PAYLOAD_BYTES + 1],
            ..request
        }
        .validate()
        .is_err());
        Ok(())
    }
}
