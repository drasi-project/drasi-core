// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional coherent snapshot/live handover, independent of historical replay.
//! Providers serialize opening the tail with capture of their private snapshot.
//! The shared hub retains only subsequent envelopes, never a second dataset.

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use async_trait::async_trait;
use tokio::{sync::Notify, time::Instant};

use crate::{
    BinaryEnvelopeCodec, ChangeEnvelope, GraphProducerProgress, OutputCursor, OutputReadError,
    OutputSnapshot,
};

type ReadResult<T> = Result<T, OutputReadError>;

/// Bounds for one non-renewing snapshot/live lease. Byte accounting includes
/// the complete binary envelope, including metadata and lineage.
#[derive(Debug, Clone, Copy)]
pub struct OutputFollowLimits {
    pub max_envelopes: NonZeroUsize,
    pub max_bytes: NonZeroUsize,
    pub lifetime: Duration,
}

impl OutputFollowLimits {
    pub fn validate(&self) -> ReadResult<()> {
        if self.lifetime.is_zero() || Instant::now().checked_add(self.lifetime).is_none() {
            return Err(OutputReadError::invalid("invalid output follow lifetime"));
        }
        Ok(())
    }
}

/// A bounded live tail. Cancelling `next` must not consume an envelope.
/// Dropping the handle synchronously releases its retention obligation; it
/// must not leave a detached worker or require an asynchronous cleanup call.
#[async_trait]
pub trait OutputFollow: Send + Sync {
    async fn next(&mut self) -> ReadResult<ChangeEnvelope>;
}

pub struct OutputSnapshotFollow {
    snapshot: OutputSnapshot,
    follow: Box<dyn OutputFollow>,
}

impl OutputSnapshotFollow {
    pub fn new(snapshot: OutputSnapshot, follow: Box<dyn OutputFollow>) -> Self {
        Self { snapshot, follow }
    }

    pub fn snapshot(&self) -> &OutputSnapshot {
        &self.snapshot
    }

    pub fn into_parts(self) -> (OutputSnapshot, Box<dyn OutputFollow>) {
        (self.snapshot, self.follow)
    }
}

/// Captures a complete snapshot and its exclusive follow position together.
/// Every subsequent committed output is retained within the requested bounds,
/// or the tail fails explicitly. This does not promise crash durability.
#[async_trait]
pub trait SnapshotFollowProvider: Send + Sync {
    async fn snapshot_and_follow(
        &self,
        limits: OutputFollowLimits,
    ) -> ReadResult<OutputSnapshotFollow>;
}

#[derive(Clone)]
enum TailFailure {
    Expired,
    Retired,
    Gap {
        requested: u64,
        latest: u64,
    },
    EpochChanged {
        requested: OutputCursor,
        current: OutputCursor,
    },
    Invalid(String),
}

impl TailFailure {
    fn error(&self) -> OutputReadError {
        match self {
            Self::Expired => OutputReadError::LeaseExpired,
            Self::Retired => OutputReadError::Retired,
            Self::Gap { requested, latest } => OutputReadError::Gap {
                requested: *requested,
                oldest: None,
                latest: *latest,
            },
            Self::EpochChanged { requested, current } => OutputReadError::EpochChanged {
                requested: Box::new(requested.producer().clone()),
                current: Box::new(current.producer().clone()),
            },
            Self::Invalid(message) => OutputReadError::invalid(message.clone()),
        }
    }
}

struct TailState {
    cursor: OutputCursor,
    head: OutputCursor,
    schema: crate::SchemaDescriptor,
    queued: VecDeque<(ChangeEnvelope, usize)>,
    bytes: usize,
    failure: Option<TailFailure>,
}

impl TailState {
    fn fail(&mut self, failure: TailFailure) {
        self.failure = Some(failure);
        self.queued.clear();
        self.bytes = 0;
    }
}

struct Tail {
    state: Mutex<TailState>,
    changed: Notify,
    deadline: Instant,
    limits: OutputFollowLimits,
}

impl Tail {
    fn lock(&self) -> ReadResult<std::sync::MutexGuard<'_, TailState>> {
        self.state
            .lock()
            .map_err(|_| OutputReadError::Failed(anyhow::anyhow!("output follow state poisoned")))
    }
}

struct TailReader(Arc<Tail>);

#[async_trait]
impl OutputFollow for TailReader {
    async fn next(&mut self) -> ReadResult<ChangeEnvelope> {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.0.lock()?;
                if state.failure.is_none() && Instant::now() >= self.0.deadline {
                    state.fail(TailFailure::Expired);
                }
                if let Some(failure) = &state.failure {
                    return Err(failure.error());
                }
                if let Some((envelope, bytes)) = state.queued.pop_front() {
                    state.bytes -= bytes;
                    let progress = GraphProducerProgress::from_envelope(&envelope)
                        .map_err(OutputReadError::Failed)?
                        .ok_or_else(|| OutputReadError::invalid("output follow lost progress"))?;
                    state.cursor = state.cursor.at_sequence(progress.sequence());
                    return Ok(envelope);
                }
            }
            // A timeout is observed through the same terminal state on the
            // next iteration, so subsequent reads cannot revive an expired lease.
            let _ = tokio::time::timeout_at(self.0.deadline, changed).await;
        }
    }
}

/// Opt-in envelope retention, with a maximum number of concurrent leases and
/// per-lease provider ceilings. It owns no tasks or current-state store.
///
/// Call `open` under the same serialization boundary used to capture the
/// snapshot, and `publish` under the producer's committed-state boundary.
/// A full/expired reader fails independently; it never blocks the producer.
pub struct OutputFollowHub {
    leases: Vec<Weak<Tail>>,
    max_leases: NonZeroUsize,
    ceiling: OutputFollowLimits,
}

impl OutputFollowHub {
    pub fn new(max_leases: NonZeroUsize, ceiling: OutputFollowLimits) -> ReadResult<Self> {
        ceiling.validate()?;
        Ok(Self {
            leases: Vec::new(),
            max_leases,
            ceiling,
        })
    }

    pub fn open(
        &mut self,
        snapshot: OutputSnapshot,
        limits: OutputFollowLimits,
    ) -> ReadResult<OutputSnapshotFollow> {
        limits.validate()?;
        if limits.max_envelopes > self.ceiling.max_envelopes
            || limits.max_bytes > self.ceiling.max_bytes
            || limits.lifetime > self.ceiling.lifetime
        {
            return Err(OutputReadError::invalid(
                "output follow request exceeds provider limits",
            ));
        }
        self.prune()?;
        if self.leases.len() >= self.max_leases.get() {
            return Err(OutputReadError::NotReady(
                "output follow lease capacity reached".into(),
            ));
        }
        let tail = Arc::new(Tail {
            state: Mutex::new(TailState {
                cursor: snapshot.cursor().clone(),
                head: snapshot.cursor().clone(),
                schema: snapshot.schema().clone(),
                queued: VecDeque::new(),
                bytes: 0,
                failure: None,
            }),
            changed: Notify::new(),
            deadline: Instant::now() + limits.lifetime,
            limits,
        });
        self.leases.push(Arc::downgrade(&tail));
        Ok(OutputSnapshotFollow::new(
            snapshot,
            Box::new(TailReader(tail)),
        ))
    }

    fn prune(&mut self) -> ReadResult<()> {
        let mut index = 0;
        while index < self.leases.len() {
            let retain = if let Some(tail) = self.leases[index].upgrade() {
                let mut state = tail.lock()?;
                if state.failure.is_none() && Instant::now() >= tail.deadline {
                    state.fail(TailFailure::Expired);
                    tail.changed.notify_one();
                }
                state.failure.is_none()
            } else {
                false
            };
            if retain {
                index += 1;
            } else {
                self.leases.swap_remove(index);
            }
        }
        Ok(())
    }

    pub fn publish(&mut self, envelope: &ChangeEnvelope) -> ReadResult<()> {
        self.prune()?;
        if self.leases.is_empty() {
            return Ok(());
        }
        let bytes = BinaryEnvelopeCodec::encoded_size(envelope)
            .map_err(|error| OutputReadError::Failed(error.into()))?;
        let progress = GraphProducerProgress::from_envelope(envelope)
            .map_err(OutputReadError::Failed)?
            .ok_or_else(|| OutputReadError::invalid("follow output has no producer progress"))?;
        let current = OutputCursor::try_new(progress.identity().clone(), progress.sequence())?;
        for lease in &self.leases {
            let Some(tail) = lease.upgrade() else {
                continue;
            };
            let mut state = tail.lock()?;
            let failure = if state.head.producer() != current.producer() {
                Some(TailFailure::EpochChanged {
                    requested: state.head.clone(),
                    current: current.clone(),
                })
            } else if envelope.changes().schema() != &state.schema
                || envelope.system().stream() != current.producer().stream()
                || current.sequence() <= state.head.sequence()
            {
                Some(TailFailure::Invalid(
                    "follow output has an invalid schema, stream or revision".into(),
                ))
            } else if state.queued.len() == tail.limits.max_envelopes.get()
                || bytes > tail.limits.max_bytes.get().saturating_sub(state.bytes)
            {
                Some(TailFailure::Gap {
                    requested: state.cursor.sequence(),
                    latest: current.sequence(),
                })
            } else {
                None
            };
            if let Some(failure) = failure {
                state.fail(failure);
            } else {
                state.head = current.clone();
                state.bytes += bytes;
                state.queued.push_back((envelope.clone(), bytes));
            }
            tail.changed.notify_one();
        }
        Ok(())
    }

    /// Stop/reset invalidates all existing leases, even if the producer later
    /// resumes with the same epoch. A new capture is required.
    pub fn retire(&mut self) -> ReadResult<()> {
        let mut failures = Vec::new();
        let mut index = 0;
        while index < self.leases.len() {
            if let Some(tail) = self.leases[index].upgrade() {
                match tail.lock() {
                    Ok(mut state) => {
                        state.fail(TailFailure::Retired);
                        tail.changed.notify_one();
                    }
                    Err(error) => {
                        failures.push(anyhow::Error::new(error));
                        index += 1;
                        continue;
                    }
                };
            }
            self.leases.swap_remove(index);
        }
        if !failures.is_empty() {
            return Err(OutputReadError::Failed(
                crate::OperationFailures::new("retiring output follow leases", failures).into(),
            ));
        }
        Ok(())
    }
}

impl Drop for OutputFollowHub {
    fn drop(&mut self) {
        if let Err(error) = self.retire() {
            log::error!("Cannot retire output follow leases: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ComponentId, GraphChangeCodec, GraphProducerIdentity, StreamId};
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };

    fn limits() -> OutputFollowLimits {
        OutputFollowLimits {
            max_envelopes: NonZeroUsize::new(2).unwrap(),
            max_bytes: NonZeroUsize::new(16_384).unwrap(),
            lifetime: Duration::from_secs(10),
        }
    }

    fn producer(epoch: u128) -> GraphProducerIdentity {
        GraphProducerIdentity::for_epoch(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("producer").unwrap(),
            StreamId::try_new("producer/out").unwrap(),
            uuid::Uuid::from_u128(epoch),
            false,
        )
        .unwrap()
    }

    fn snapshot() -> OutputSnapshot {
        OutputSnapshot::try_new(
            OutputCursor::try_new(producer(1), 4).unwrap(),
            "dataset",
            GraphChangeCodec::schema().descriptor().clone(),
            im::HashMap::new(),
        )
        .unwrap()
    }

    fn event(sequence: u64, epoch: u128) -> ChangeEnvelope {
        let mut envelope = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("dataset", "a"),
                        labels: vec!["Item".into()].into(),
                        effective_from: 1,
                    },
                    properties: ElementPropertyMap::from(serde_json::json!({"value": sequence})),
                },
            },
            producer(epoch).stream().clone(),
            sequence,
            None,
        )
        .unwrap();
        GraphProducerProgress::annotate(&mut envelope, &producer(epoch), sequence).unwrap();
        envelope
    }

    fn hub() -> OutputFollowHub {
        OutputFollowHub::new(NonZeroUsize::new(2).unwrap(), limits()).unwrap()
    }

    #[tokio::test]
    async fn changes_are_retained_while_snapshot_is_consumed() {
        let mut hub = hub();
        let opened = hub.open(snapshot(), limits()).unwrap();
        hub.publish(&event(5, 1)).unwrap();
        hub.publish(&event(6, 1).reemit(1).unwrap()).unwrap();
        let (snapshot, mut follow) = opened.into_parts();
        assert_eq!(snapshot.cursor().sequence(), 4);
        assert!(snapshot.records().is_empty());
        for expected in [5, 6] {
            let output = follow.next().await.unwrap();
            assert_eq!(
                GraphProducerProgress::from_envelope(&output)
                    .unwrap()
                    .unwrap()
                    .sequence(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn overflowing_one_reader_does_not_block_another() {
        let mut hub = hub();
        let (_, mut slow) = hub.open(snapshot(), limits()).unwrap().into_parts();
        let (_, mut fast) = hub.open(snapshot(), limits()).unwrap().into_parts();
        for sequence in [5, 6, 7] {
            hub.publish(&event(sequence, 1)).unwrap();
            assert_eq!(fast.next().await.unwrap().system().sequence(), sequence);
        }
        assert!(matches!(
            slow.next().await,
            Err(OutputReadError::Gap {
                requested: 4,
                latest: 7,
                ..
            })
        ));
        assert!(matches!(
            slow.next().await,
            Err(OutputReadError::Gap { .. })
        ));
    }

    #[tokio::test]
    async fn full_envelope_byte_limit_is_enforced() {
        let mut hub = hub();
        let envelope = event(5, 1);
        let bytes = BinaryEnvelopeCodec::encoded_size(&envelope).unwrap();
        let exact = OutputFollowLimits {
            max_bytes: NonZeroUsize::new(bytes).unwrap(),
            ..limits()
        };
        let short = OutputFollowLimits {
            max_bytes: NonZeroUsize::new(bytes - 1).unwrap(),
            ..limits()
        };
        let (_, mut accepted) = hub.open(snapshot(), exact).unwrap().into_parts();
        let (_, mut rejected) = hub.open(snapshot(), short).unwrap().into_parts();
        hub.publish(&envelope).unwrap();
        assert!(accepted.next().await.is_ok());
        assert!(matches!(
            rejected.next().await,
            Err(OutputReadError::Gap { .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_wakes_a_pending_reader_and_releases_capacity() {
        let mut hub = OutputFollowHub::new(NonZeroUsize::new(1).unwrap(), limits()).unwrap();
        let (_, mut follow) = hub.open(snapshot(), limits()).unwrap().into_parts();
        assert!(matches!(
            hub.open(snapshot(), limits()),
            Err(OutputReadError::NotReady(_))
        ));
        assert!(matches!(
            follow.next().await,
            Err(OutputReadError::LeaseExpired)
        ));
        assert!(hub.open(snapshot(), limits()).is_ok());
    }

    #[tokio::test]
    async fn cancelling_next_does_not_consume_and_dropping_releases_lease() {
        let mut hub = OutputFollowHub::new(NonZeroUsize::new(1).unwrap(), limits()).unwrap();
        let (_, mut follow) = hub.open(snapshot(), limits()).unwrap().into_parts();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), follow.next())
                .await
                .is_err()
        );
        hub.publish(&event(5, 1)).unwrap();
        assert_eq!(follow.next().await.unwrap().system().sequence(), 5);
        drop(follow);
        assert!(hub.open(snapshot(), limits()).is_ok());
    }

    #[tokio::test]
    async fn reset_and_stop_invalidate_pending_leases() {
        let mut hub = hub();
        let (_, mut follow) = hub.open(snapshot(), limits()).unwrap().into_parts();
        hub.publish(&event(5, 2)).unwrap();
        assert!(matches!(
            follow.next().await,
            Err(OutputReadError::EpochChanged { .. })
        ));
        let (_, mut follow) = hub.open(snapshot(), limits()).unwrap().into_parts();
        hub.retire().unwrap();
        assert!(matches!(follow.next().await, Err(OutputReadError::Retired)));
        let (_, mut follow) = hub.open(snapshot(), limits()).unwrap().into_parts();
        drop(hub);
        assert!(matches!(follow.next().await, Err(OutputReadError::Retired)));
    }

    #[tokio::test]
    async fn invalid_revisions_are_not_delivered() {
        let mut hub = hub();
        let (_, mut follow) = hub.open(snapshot(), limits()).unwrap().into_parts();
        hub.publish(&event(4, 1)).unwrap();
        assert!(matches!(
            follow.next().await,
            Err(OutputReadError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn poisoned_lease_does_not_hide_failed_cleanup_or_strand_other_readers() {
        let mut hub = hub();
        let (_, mut poisoned) = hub.open(snapshot(), limits()).unwrap().into_parts();
        let (_, mut healthy) = hub.open(snapshot(), limits()).unwrap().into_parts();
        let tail = hub.leases[0].upgrade().unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = tail.state.lock().unwrap();
            panic!("poison one lease");
        }));
        assert!(result.is_err());
        assert!(hub.retire().is_err());
        assert!(matches!(
            healthy.next().await,
            Err(OutputReadError::Retired)
        ));
        assert!(matches!(
            poisoned.next().await,
            Err(OutputReadError::Failed(_))
        ));
        assert_eq!(hub.leases.len(), 1);
        assert!(hub.retire().is_err());
    }

    #[test]
    fn unsupported_limits_are_rejected_before_opening() {
        let mut hub = hub();
        assert!(matches!(
            hub.open(
                snapshot(),
                OutputFollowLimits {
                    lifetime: Duration::ZERO,
                    ..limits()
                }
            ),
            Err(OutputReadError::Invalid(_))
        ));
        assert!(matches!(
            hub.open(
                snapshot(),
                OutputFollowLimits {
                    max_envelopes: NonZeroUsize::new(3).unwrap(),
                    ..limits()
                }
            ),
            Err(OutputReadError::Invalid(_))
        ));
    }
}
