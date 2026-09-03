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

use crate::channels::events::StampedSourceEvent;

/// Priority queue specialized for SourceEvents
/// This is now a type alias to the generic priority queue implementation
pub type PriorityQueue = crate::channels::priority_queue::PriorityQueue<StampedSourceEvent>;

/// Re-export metrics type for compatibility
pub use crate::channels::priority_queue::PriorityQueueMetrics;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::events::{SourceEvent, SourceEventDraft, StampedSourceEvent};
    use chrono::Utc;
    use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
    use std::sync::Arc;

    fn create_test_event(
        source_id: &str,
        timestamp: chrono::DateTime<Utc>,
    ) -> Arc<StampedSourceEvent> {
        create_test_event_seq(source_id, timestamp, 0)
    }

    fn create_test_event_seq(
        source_id: &str,
        timestamp: chrono::DateTime<Utc>,
        sequence: u64,
    ) -> Arc<StampedSourceEvent> {
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

        Arc::new(StampedSourceEvent::stamp(
            SourceEventDraft::new(
                source_id.to_string(),
                SourceEvent::Change(change),
                timestamp,
            ),
            sequence,
        ))
    }

    // Issue #814 Scenario 1/2: same-timestamp events from a single source must
    // dequeue in ascending `sequence` order regardless of enqueue order, so the
    // query evaluates them in emission order (and the sequence dedup high-water
    // never moves backward).
    #[tokio::test]
    async fn test_same_timestamp_dequeues_in_sequence_order() {
        let pq = PriorityQueue::new(100);
        let now = Utc::now();

        // Enqueue out of order; identical timestamp and source rank.
        pq.enqueue(create_test_event_seq("s1", now, 3), 0).await;
        pq.enqueue(create_test_event_seq("s1", now, 1), 0).await;
        pq.enqueue(create_test_event_seq("s1", now, 2), 0).await;

        assert_eq!(pq.try_dequeue().await.unwrap().sequence, 1);
        assert_eq!(pq.try_dequeue().await.unwrap().sequence, 2);
        assert_eq!(pq.try_dequeue().await.unwrap().sequence, 3);
    }

    // Issue #814 Scenario 3/4: two sources emit same-timestamp events. The lower
    // source rank dequeues first; within a source, sequence orders the events.
    #[tokio::test]
    async fn test_same_timestamp_multi_source_ordered_by_rank_then_sequence() {
        let pq = PriorityQueue::new(100);
        let now = Utc::now();

        // Interleave enqueue order across the two sources.
        pq.enqueue(create_test_event_seq("sourceB", now, 11), 1)
            .await;
        pq.enqueue(create_test_event_seq("sourceA", now, 21), 0)
            .await;
        pq.enqueue(create_test_event_seq("sourceB", now, 10), 1)
            .await;
        pq.enqueue(create_test_event_seq("sourceA", now, 20), 0)
            .await;

        // rank 0 (sourceA) first, in sequence order, then rank 1 (sourceB).
        let d1 = pq.try_dequeue().await.unwrap();
        assert_eq!((d1.source_id.as_str(), d1.sequence), ("sourceA", 20));
        let d2 = pq.try_dequeue().await.unwrap();
        assert_eq!((d2.source_id.as_str(), d2.sequence), ("sourceA", 21));
        let d3 = pq.try_dequeue().await.unwrap();
        assert_eq!((d3.source_id.as_str(), d3.sequence), ("sourceB", 10));
        let d4 = pq.try_dequeue().await.unwrap();
        assert_eq!((d4.source_id.as_str(), d4.sequence), ("sourceB", 11));
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
        pq.enqueue(event1, 0).await;
        pq.enqueue(event3, 0).await;
        pq.enqueue(event2, 0).await;

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
        assert!(pq.enqueue(event1, 0).await);
        assert!(pq.enqueue(event2, 0).await);

        // Should reject when at capacity
        assert!(!pq.enqueue(event3, 0).await);

        // Metrics should reflect the drop
        let metrics = pq.metrics().await;
        assert_eq!(metrics.drops_due_to_capacity, 1);
        assert_eq!(metrics.total_enqueued, 2);
    }

    #[tokio::test]
    async fn test_priority_queue_metrics() {
        let pq = PriorityQueue::new(100);

        let now = Utc::now();
        pq.enqueue(create_test_event("source1", now), 0).await;
        pq.enqueue(create_test_event("source2", now), 0).await;

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
            pq_clone.enqueue(event, 0).await;
        });

        // This should block until the event arrives
        let event = pq.dequeue().await;
        assert_eq!(event.source_id, "source1");
    }

    #[tokio::test]
    async fn test_drain() {
        let pq = PriorityQueue::new(100);

        let now = Utc::now();
        pq.enqueue(create_test_event("source1", now), 0).await;
        pq.enqueue(create_test_event("source2", now), 0).await;
        pq.enqueue(create_test_event("source3", now), 0).await;

        let drained = pq.drain().await;
        assert_eq!(drained.len(), 3);
        assert!(pq.is_empty().await);
    }
}
