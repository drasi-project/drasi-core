// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{GraphTelemetry, TelemetryError};
use metrics::{Counter, Gauge};
use std::sync::{Arc, Mutex, Weak};

/// Optional physical-owner observations, independent of per-edge inspection.
/// Providers update this at their existing serialized mutation boundaries.
#[derive(Debug, Default)]
pub struct QueueObservations {
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    known: bool,
    depth: usize,
    peak: usize,
    binding: Weak<QueueBinding>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            known: true,
            depth: 0,
            peak: 0,
            binding: Weak::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct QueueBinding {
    owner: Weak<QueueObservations>,
    identity: (String, String, String),
    depth: Gauge,
    peak: Gauge,
    blocked: Counter,
    drops: Counter,
}

impl QueueObservations {
    pub(crate) fn unknown() -> Self {
        Self {
            state: Mutex::new(State {
                known: false,
                ..Default::default()
            }),
        }
    }
    pub(crate) fn bind(
        self: &Arc<Self>,
        telemetry: &GraphTelemetry,
        queue_id: &str,
    ) -> Result<Option<Arc<QueueBinding>>, TelemetryError> {
        let Some(scope) = telemetry.scope() else {
            return Ok(None);
        };
        if !telemetry.policy().metrics_enabled() {
            return Ok(None);
        }
        let identity = (
            scope.instance_id().to_owned(),
            scope.graph_id().to_owned(),
            queue_id.to_owned(),
        );
        let mut state = self.state.lock().map_err(|_| TelemetryError::Poisoned)?;
        if let Some(binding) = state.binding.upgrade() {
            if binding.identity != identity {
                return Err(TelemetryError::Invalid(
                    "one physical queue cannot have conflicting telemetry identities".into(),
                ));
            }
            return Ok(Some(binding));
        }
        let metric = |name| telemetry.register(name, vec![("queue_id".into(), queue_id.into())]);
        let binding = Arc::new(QueueBinding {
            owner: Arc::downgrade(self),
            identity,
            depth: metric("drasi.queue.depth")?.gauge()?,
            peak: metric("drasi.queue.depth_max")?.gauge()?,
            blocked: metric("drasi.queue.blocked_enqueues")?.counter()?,
            drops: metric("drasi.queue.drops")?.counter()?,
        });
        state.peak = state.depth;
        if state.known {
            binding.depth.set(state.depth as f64);
            binding.peak.set(state.peak as f64);
        }
        state.binding = Arc::downgrade(&binding);
        Ok(Some(binding))
    }

    fn update(&self, work: impl FnOnce(&mut State)) {
        let mut state = self.state.lock().unwrap_or_else(|error| {
            log::error!("Physical queue telemetry state poisoned: {error}");
            error.into_inner()
        });
        work(&mut state);
    }

    /// Known pending obligations, not retained history or page-cache occupancy.
    pub fn set_depth(&self, depth: usize) {
        self.update(|state| {
            state.known = true;
            state.depth = depth;
            state.peak = state.peak.max(depth);
            if let Some(binding) = state.binding.upgrade() {
                binding.depth.set(depth as f64);
                binding.peak.set(state.peak as f64);
            }
        });
    }

    pub(crate) fn enqueued(&self) {
        self.update(|state| {
            state.depth += 1;
            state.peak = state.peak.max(state.depth);
            if let Some(binding) = state.binding.upgrade() {
                binding.depth.set(state.depth as f64);
                binding.peak.set(state.peak as f64);
            }
        });
    }

    pub(crate) fn dequeued(&self, discarded: usize) {
        self.update(|state| {
            let count = discarded.max(1);
            state.depth = state.depth.saturating_sub(count);
            if let Some(binding) = state.binding.upgrade() {
                binding.depth.set(state.depth as f64);
                binding.drops.increment(discarded as u64);
            }
        });
    }

    pub fn blocked(&self) {
        self.update(|state| {
            if let Some(binding) = state.binding.upgrade() {
                binding.blocked.increment(1);
            }
        });
    }

    pub fn discarded(&self, count: usize) {
        if count == 0 {
            return;
        }
        self.update(|state| {
            if let Some(binding) = state.binding.upgrade() {
                binding.drops.increment(count as u64);
            }
        });
    }
}

impl Drop for QueueBinding {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            let state = owner.state.lock().unwrap_or_else(|error| {
                log::error!("Revoking poisoned queue telemetry: {error}");
                error.into_inner()
            });
            if !std::ptr::eq(state.binding.as_ptr(), self) {
                return;
            }
            self.depth.set(0.0);
            self.peak.set(0.0);
            return;
        }
        self.depth.set(0.0);
        self.peak.set(0.0);
    }
}

pub(crate) fn pending_depth(head: u64, oldest: Option<u64>, progress: u64) -> usize {
    oldest.map_or(0, |oldest| {
        head.saturating_sub(oldest.saturating_sub(1).max(progress)) as usize
    })
}

pub(crate) fn discarded_pending(oldest: Option<u64>, retain_from: u64, progress: u64) -> usize {
    oldest.map_or(0, |oldest| {
        retain_from.saturating_sub(oldest.max(progress.saturating_add(1))) as usize
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computation::v1::*;
    use std::{
        collections::BTreeMap,
        num::NonZeroUsize,
        sync::atomic::{AtomicU64, Ordering},
    };

    #[derive(Default)]
    struct Count(AtomicU64);
    impl metrics::CounterFn for Count {
        fn increment(&self, value: u64) {
            self.0.fetch_add(value, Ordering::Relaxed);
        }
        fn absolute(&self, value: u64) {
            self.0.fetch_max(value, Ordering::Relaxed);
        }
    }

    fn recording(owner: &Arc<QueueObservations>) -> (Arc<QueueBinding>, Arc<Count>, Arc<Count>) {
        let blocked = Arc::new(Count::default());
        let drops = Arc::new(Count::default());
        let binding = Arc::new(QueueBinding {
            owner: Arc::downgrade(owner),
            identity: ("test".into(), "graph".into(), "queue".into()),
            depth: Gauge::noop(),
            peak: Gauge::noop(),
            blocked: Counter::from_arc(blocked.clone()),
            drops: Counter::from_arc(drops.clone()),
        });
        owner.state.lock().unwrap().binding = Arc::downgrade(&binding);
        (binding, blocked, drops)
    }

    fn envelope(sequence: u64) -> ChangeEnvelope {
        use drasi_core::models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        };
        GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: sequence,
                    },
                    properties: ElementPropertyMap::new(),
                },
            },
            StreamId::try_new("source/out").unwrap(),
            sequence,
            None,
        )
        .unwrap()
    }

    fn depth(owner: &QueueObservations) -> usize {
        owner.state.lock().unwrap().depth
    }

    #[tokio::test]
    async fn retained_history_and_pending_eviction_are_distinct() {
        let store =
            MemoryEnvelopeStore::new(NonZeroUsize::new(2).unwrap(), RetentionPolicy::PruneOldest);
        let owner = store.observations().unwrap();
        let (_lease, _, drops) = recording(&owner);
        let generation = store.acquire_generation().unwrap();
        store.append(generation, &envelope(1)).await.unwrap();
        store.append(generation, &envelope(2)).await.unwrap();
        assert_eq!(depth(&owner), 2);
        store.acknowledge(generation, 1).await.unwrap();
        assert_eq!(depth(&owner), 1);
        store.append(generation, &envelope(3)).await.unwrap();
        assert_eq!(
            drops.0.load(Ordering::Relaxed),
            0,
            "completed history is not dropped work"
        );
        store.append(generation, &envelope(4)).await.unwrap();
        assert_eq!(depth(&owner), 2);
        assert_eq!(drops.0.load(Ordering::Relaxed), 1);
        store.acknowledge(generation, 4).await.unwrap();
        assert_eq!(depth(&owner), 0);
        assert!(
            store.next(generation, 2).await.unwrap().is_some(),
            "retained history still exists"
        );
    }

    #[tokio::test]
    async fn retained_capacity_wait_counts_once_even_after_spurious_wakes() {
        let store = Arc::new(MemoryEnvelopeStore::new(
            NonZeroUsize::new(1).unwrap(),
            RetentionPolicy::Backpressure,
        ));
        let owner = store.observations().unwrap();
        let (_lease, blocked, _) = recording(&owner);
        let pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).unwrap();
        let sender = pipe.sender();
        sender.send(envelope(1)).await.unwrap();
        let waiting = tokio::task::unconstrained(sender.send(envelope(2)));
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        store.notify().notify_waiters();
        assert!(futures::poll!(&mut waiting).is_pending());
        assert_eq!(blocked.0.load(Ordering::Relaxed), 1);
        assert_eq!(depth(&owner), 1);
    }

    #[tokio::test]
    async fn multicast_acknowledgement_disconnect_and_retirement_use_unique_pending_records() {
        let channel = QosChannel::volatile(QosChannelDefinition {
            stream: StreamId::try_new("source/out").unwrap(),
            capacity: NonZeroUsize::new(3).unwrap(),
            durable: false,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([
                ("one".into(), SubscriptionStart::Earliest),
                ("two".into(), SubscriptionStart::Earliest),
                ("three".into(), SubscriptionStart::Earliest),
            ]),
        })
        .unwrap();
        let resource = ResourceId::try_new("channel").unwrap();
        let mut pipes: Vec<_> = ["one", "two", "three"]
            .into_iter()
            .map(|subscriber| {
                channel
                    .definition()
                    .pipe(resource.clone(), subscriber)
                    .create_with_resources(&BTreeMap::from([(
                        resource.clone(),
                        channel.resource(),
                    )]))
                    .unwrap()
            })
            .collect();
        let owner = pipes[0].control.observations().unwrap();
        let (_lease, _, drops) = recording(&owner);
        channel.publish(&envelope(1)).await.unwrap();
        channel.publish(&envelope(2)).await.unwrap();
        assert_eq!(depth(&owner), 2);
        let mut receivers: Vec<_> = pipes
            .iter_mut()
            .map(|pipe| pipe.pipe.take_receiver().unwrap())
            .collect();
        for (index, receiver) in receivers.iter_mut().enumerate() {
            receiver
                .receive()
                .await
                .unwrap()
                .unwrap()
                .into_parts()
                .2
                .unwrap()
                .complete(HandlingOutcome::Handled)
                .await
                .unwrap();
            assert_eq!(depth(&owner), if index == 2 { 1 } else { 2 });
        }
        for receiver in receivers.iter_mut().take(2) {
            receiver
                .receive()
                .await
                .unwrap()
                .unwrap()
                .into_parts()
                .2
                .unwrap()
                .complete(HandlingOutcome::Handled)
                .await
                .unwrap();
        }
        pipes[2].control.cancel();
        assert_eq!(
            depth(&owner),
            1,
            "disconnection does not retire an obligation"
        );
        channel.retire("three").await.unwrap();
        assert_eq!(depth(&owner), 0);
        assert_eq!(
            drops.0.load(Ordering::Relaxed),
            0,
            "retirement is not a lossy eviction"
        );
    }

    #[test]
    fn queue_identity_conflicts_are_explicit_and_unbinding_allows_reuse() {
        let telemetry = crate::observability::TelemetryOptions::new(
            crate::observability::TelemetryProfile::Basic,
        )
        .with_instance_id("instance")
        .with_metric_context(
            crate::observability::MetricRegistrationContext::new(
                Arc::new(metrics::NoopRecorder),
                Default::default(),
            )
            .unwrap(),
        )
        .resolve("graph", None)
        .unwrap();
        let owner = Arc::new(QueueObservations::default());
        let first = owner.bind(&telemetry, "queue-one").unwrap().unwrap();
        let same = owner.bind(&telemetry, "queue-one").unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &same));
        assert!(owner.bind(&telemetry, "queue-two").is_err());
        drop(first);
        drop(same);
        let replacement = owner.bind(&telemetry, "queue-two").unwrap().unwrap();
        assert_eq!(replacement.identity.2, "queue-two");
    }

    #[cfg(feature = "computation-rocksdb-tests")]
    #[tokio::test]
    async fn paged_retained_reconstruction_restores_current_depth_not_historical_counters() {
        use drasi_core::{computation::ComputationIndexProvider, interface::OutboxPageLimits};
        use drasi_index_rocksdb::{
            computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
        };
        let directory = tempfile::TempDir::new().unwrap();
        let open = || async {
            let provider = RocksDbComputationProvider::new(
                directory.path(),
                RocksIndexOptions::new(
                    false,
                    false,
                    RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).unwrap(),
                ),
            );
            let indexes = provider.create_indexes("graph", "retained").await.unwrap();
            let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).unwrap());
            codec.register_schema(GraphChangeCodec::schema()).unwrap();
            IndexedEnvelopeStore::try_new_with_options(
                indexes,
                Arc::new(codec),
                "events",
                NonZeroUsize::new(8).unwrap(),
                RetentionPolicy::Backpressure,
                IndexedJournalOptions {
                    max_bytes: None,
                    page_limits: Some(OutboxPageLimits {
                        max_records: NonZeroUsize::new(1).unwrap(),
                        max_bytes: NonZeroUsize::new(4096).unwrap(),
                    }),
                },
            )
            .unwrap()
        };
        let store = open().await;
        let generation = store.acquire_generation().unwrap();
        let owner = store.observations().unwrap();
        let (lease, _, _) = recording(&owner);
        for sequence in 1..=4 {
            store.append(generation, &envelope(sequence)).await.unwrap();
        }
        store.acknowledge(generation, 1).await.unwrap();
        assert_eq!(depth(&owner), 3);
        store.shutdown().await.unwrap();
        drop(lease);
        drop(store);
        let reopened = open().await;
        let generation = reopened.acquire_generation().unwrap();
        let owner = reopened.observations().unwrap();
        let (_lease, blocked, drops) = recording(&owner);
        assert_eq!(reopened.progress(generation).await.unwrap(), 1);
        assert_eq!(depth(&owner), 3);
        for after in 1..=3 {
            assert!(reopened.next(generation, after).await.unwrap().is_some());
        }
        assert_eq!(
            depth(&owner),
            3,
            "page-cache movement does not change backlog"
        );
        assert_eq!(blocked.0.load(Ordering::Relaxed), 0);
        assert_eq!(drops.0.load(Ordering::Relaxed), 0);
        reopened.shutdown().await.unwrap();
    }
}
