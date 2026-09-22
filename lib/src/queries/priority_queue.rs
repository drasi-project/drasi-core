// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Priority queue for source events - now uses generic priority queue implementation
//!
//! This module provides a type alias to the generic priority queue for source events.
//! The generic implementation is located in channels::priority_queue.

use crate::channels::events::SourceEventWrapper;
use crate::channels::{SourceControl, SourceEvent, Timestamped};
use std::{collections::HashMap, sync::Arc};

/// Priority queue specialized for SourceEvents
/// This is now a type alias to the generic priority queue implementation
pub type PriorityQueue = crate::channels::priority_queue::PriorityQueue<SourceEventWrapper>;

/// Re-export metrics type for compatibility
pub use crate::channels::priority_queue::PriorityQueueMetrics;

#[derive(Debug, Clone)]
struct RankedSourceEvent {
    event: Arc<SourceEventWrapper>,
    rank: usize,
    sequence: u64,
}

impl Timestamped for RankedSourceEvent {
    fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        self.event.timestamp
    }

    fn ordering_tie_breaker(&self) -> Option<(usize, u64)> {
        Some((self.rank, self.sequence))
    }
}

/// One bounded query inbox. Source ranks belong to this query, never to a shared source.
#[derive(Clone)]
pub(crate) struct QueryEventQueue {
    queue: crate::channels::priority_queue::PriorityQueue<RankedSourceEvent>,
    ranks: Arc<HashMap<String, usize>>,
}

impl QueryEventQueue {
    pub(crate) fn new<'a>(
        capacity: usize,
        sources: impl IntoIterator<Item = &'a str>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(capacity > 0, "query input capacity must be nonzero");
        let mut ranks = HashMap::new();
        for (rank, source) in sources.into_iter().enumerate() {
            anyhow::ensure!(
                source != crate::sources::future_queue_source::FUTURE_QUEUE_SOURCE_ID,
                "the scheduled-work source ID is reserved"
            );
            anyhow::ensure!(
                ranks.insert(source.to_owned(), rank).is_none(),
                "duplicate source subscription '{source}'"
            );
        }
        let scheduled_rank = ranks.len();
        ranks.insert(
            crate::sources::future_queue_source::FUTURE_QUEUE_SOURCE_ID.to_owned(),
            scheduled_rank,
        );
        Ok(Self {
            queue: crate::channels::priority_queue::PriorityQueue::new(capacity),
            ranks: Arc::new(ranks),
        })
    }

    fn ranked(
        &self,
        event: Arc<SourceEventWrapper>,
    ) -> anyhow::Result<Option<Arc<RankedSourceEvent>>> {
        if matches!(
            event.event,
            SourceEvent::Control(SourceControl::Subscription { .. })
        ) {
            return Ok(None);
        }
        let rank = *self
            .ranks
            .get(&event.source_id)
            .ok_or_else(|| anyhow::anyhow!("undeclared query source '{}'", event.source_id))?;
        let sequence = event.sequence;
        Ok(Some(Arc::new(RankedSourceEvent {
            event,
            rank,
            sequence,
        })))
    }

    pub(crate) async fn enqueue(&self, event: Arc<SourceEventWrapper>) -> anyhow::Result<bool> {
        match self.ranked(event)? {
            Some(event) => Ok(self.queue.enqueue(event).await),
            None => Ok(true),
        }
    }

    pub(crate) async fn enqueue_wait(&self, event: Arc<SourceEventWrapper>) -> anyhow::Result<()> {
        if let Some(event) = self.ranked(event)? {
            self.queue.enqueue_wait(event).await;
        }
        Ok(())
    }

    pub(crate) async fn dequeue(&self) -> Arc<SourceEventWrapper> {
        self.queue.dequeue().await.event.clone()
    }

    pub(crate) async fn drain(&self) -> Vec<Arc<SourceEventWrapper>> {
        self.queue
            .drain()
            .await
            .into_iter()
            .map(|event| event.event.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::events::{SourceEvent, SourceEventWrapper};
    use chrono::Utc;
    use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
    use std::sync::Arc;

    fn create_test_event(
        source_id: &str,
        timestamp: chrono::DateTime<Utc>,
    ) -> Arc<SourceEventWrapper> {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(source_id, "test-node-1"),
                    labels: Default::default(),
                    effective_from: 0,
                },
                properties: Default::default(),
            },
        };

        Arc::new(SourceEventWrapper::new(
            source_id.to_string(),
            SourceEvent::Change(change),
            timestamp,
            1,
        ))
    }

    #[tokio::test]
    async fn test_priority_queue_ordering() {
        let pq = PriorityQueue::new(100);

        // Create events with different timestamps
        let now = Utc::now();
        let event1 = create_test_event("source1", now);
        let event2 = create_test_event("source2", now - chrono::Duration::seconds(5));
        let event3 = create_test_event("source3", now + chrono::Duration::seconds(5));

        // Enqueue in random order
        pq.enqueue(event1).await;
        pq.enqueue(event3).await;
        pq.enqueue(event2).await;

        // Dequeue should return oldest first
        let dequeued1 = pq.try_dequeue().await.unwrap();
        assert_eq!(dequeued1.source_id, "source2"); // Oldest

        let dequeued2 = pq.try_dequeue().await.unwrap();
        assert_eq!(dequeued2.source_id, "source1"); // Middle

        let dequeued3 = pq.try_dequeue().await.unwrap();
        assert_eq!(dequeued3.source_id, "source3"); // Newest
    }

    #[tokio::test]
    async fn test_priority_queue_capacity() {
        let pq = PriorityQueue::new(2);

        let now = Utc::now();
        let event1 = create_test_event("source1", now);
        let event2 = create_test_event("source2", now);
        let event3 = create_test_event("source3", now);

        // Enqueue up to capacity
        assert!(pq.enqueue(event1).await);
        assert!(pq.enqueue(event2).await);

        // Should reject when at capacity
        assert!(!pq.enqueue(event3).await);

        // Metrics should reflect the drop
        let metrics = pq.metrics().await;
        assert_eq!(metrics.drops_due_to_capacity, 1);
        assert_eq!(metrics.total_enqueued, 2);
    }

    #[tokio::test]
    async fn test_priority_queue_metrics() {
        let pq = PriorityQueue::new(100);

        let now = Utc::now();
        pq.enqueue(create_test_event("source1", now)).await;
        pq.enqueue(create_test_event("source2", now)).await;

        let metrics = pq.metrics().await;
        assert_eq!(metrics.total_enqueued, 2);
        assert_eq!(metrics.current_depth, 2);
        assert_eq!(metrics.max_depth_seen, 2);

        pq.try_dequeue().await;

        let metrics = pq.metrics().await;
        assert_eq!(metrics.total_dequeued, 1);
        assert_eq!(metrics.current_depth, 1);
    }

    #[tokio::test]
    async fn test_blocking_dequeue() {
        let pq = PriorityQueue::new(100);
        let pq_clone = pq.clone();

        // Spawn a task that will enqueue after a delay
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            let event = create_test_event("source1", Utc::now());
            pq_clone.enqueue(event).await;
        });

        // This should block until the event arrives
        let event = pq.dequeue().await;
        assert_eq!(event.source_id, "source1");
    }

    #[tokio::test]
    async fn test_drain() {
        let pq = PriorityQueue::new(100);

        let now = Utc::now();
        pq.enqueue(create_test_event("source1", now)).await;
        pq.enqueue(create_test_event("source2", now)).await;
        pq.enqueue(create_test_event("source3", now)).await;

        let drained = pq.drain().await;
        assert_eq!(drained.len(), 3);
        assert!(pq.is_empty().await);
    }

    #[tokio::test]
    async fn ranked_query_queue_uses_declaration_order_then_source_sequence() {
        let timestamp = chrono::DateTime::from_timestamp_millis(2_000).unwrap();
        let frames: Vec<_> = [
            ("a-source", 2, timestamp),
            ("z-source", 2, timestamp),
            ("a-source", 1, timestamp),
            ("z-source", 1, timestamp),
            ("earlier", 1, timestamp - chrono::Duration::milliseconds(1)),
        ]
        .into_iter()
        .map(|(source, sequence, time)| {
            let mut event = create_test_event(source, time);
            Arc::make_mut(&mut event).sequence = sequence;
            event
        })
        .collect();
        for (sources, expected) in [
            (["z-source", "a-source", "earlier"], [4, 3, 1, 2, 0]),
            (["a-source", "z-source", "earlier"], [4, 2, 0, 3, 1]),
        ] {
            let queue = QueryEventQueue::new(frames.len(), sources).unwrap();
            for event in &frames {
                queue.enqueue_wait(event.clone()).await.unwrap();
            }
            for index in expected {
                assert!(Arc::ptr_eq(&queue.dequeue().await, &frames[index]));
            }
        }
    }

    #[tokio::test]
    async fn ranked_query_inputs_require_unique_declared_sources() {
        let future = crate::sources::future_queue_source::FUTURE_QUEUE_SOURCE_ID;
        assert!(QueryEventQueue::new(0, ["source"]).is_err());
        assert!(QueryEventQueue::new(1, ["source", "source"]).is_err());
        assert!(QueryEventQueue::new(1, [future]).is_err());
        let queue = QueryEventQueue::new(1, ["source"]).unwrap();
        assert!(queue
            .enqueue(create_test_event("source", Utc::now()))
            .await
            .unwrap());
        assert_eq!(queue.dequeue().await.sequence, 1);
        let error = queue
            .enqueue(create_test_event("unknown", Utc::now()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("undeclared query source"));
    }
}
