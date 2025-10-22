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

use crate::channels::events::{ArcSourceEvent, PriorityQueueEvent};
use log::{debug, warn};
use std::collections::BinaryHeap;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

/// Thread-safe priority queue for ordering events from multiple sources by timestamp
pub struct PriorityQueue {
    /// Internal heap storing events (min-heap by timestamp)
    heap: Arc<Mutex<BinaryHeap<PriorityQueueEvent>>>,
    /// Notification mechanism for waiting on new events
    notify: Arc<Notify>,
    /// Maximum queue capacity (for backpressure)
    max_capacity: usize,
    /// Metrics
    metrics: Arc<Mutex<PriorityQueueMetrics>>,
}

#[derive(Debug, Clone, Default)]
pub struct PriorityQueueMetrics {
    pub total_enqueued: u64,
    pub total_dequeued: u64,
    pub current_depth: usize,
    pub max_depth_seen: usize,
    pub drops_due_to_capacity: u64,
}

impl PriorityQueue {
    /// Create a new priority queue with the specified maximum capacity
    pub fn new(max_capacity: usize) -> Self {
        Self {
            heap: Arc::new(Mutex::new(BinaryHeap::new())),
            notify: Arc::new(Notify::new()),
            max_capacity,
            metrics: Arc::new(Mutex::new(PriorityQueueMetrics::default())),
        }
    }

    /// Enqueue an event into the priority queue
    /// Returns true if enqueued, false if queue is at capacity
    pub async fn enqueue(&self, event: ArcSourceEvent) -> bool {
        let mut heap = self.heap.lock().await;

        // Check capacity
        if heap.len() >= self.max_capacity {
            warn!(
                "Priority queue at capacity ({}), dropping event from source: {}",
                self.max_capacity, event.source_id
            );
            let mut metrics = self.metrics.lock().await;
            metrics.drops_due_to_capacity += 1;
            drop(metrics);
            drop(heap);
            return false;
        }

        // Enqueue event
        heap.push(PriorityQueueEvent::new(event));

        // Update metrics
        let mut metrics = self.metrics.lock().await;
        metrics.total_enqueued += 1;
        metrics.current_depth = heap.len();
        if metrics.current_depth > metrics.max_depth_seen {
            metrics.max_depth_seen = metrics.current_depth;
        }
        drop(metrics);
        drop(heap);

        // Notify waiting dequeuers
        self.notify.notify_one();

        true
    }

    /// Dequeue the oldest event from the priority queue (non-blocking)
    /// Returns None if queue is empty
    pub async fn try_dequeue(&self) -> Option<ArcSourceEvent> {
        let mut heap = self.heap.lock().await;
        let event = heap.pop().map(|pq_event| pq_event.event);

        if event.is_some() {
            let mut metrics = self.metrics.lock().await;
            metrics.total_dequeued += 1;
            metrics.current_depth = heap.len();
        }

        event
    }

    /// Dequeue the oldest event, waiting if the queue is empty
    pub async fn dequeue(&self) -> ArcSourceEvent {
        loop {
            // Try to dequeue
            let mut heap = self.heap.lock().await;
            if let Some(pq_event) = heap.pop() {
                let event = pq_event.event;
                let mut metrics = self.metrics.lock().await;
                metrics.total_dequeued += 1;
                metrics.current_depth = heap.len();
                drop(metrics);
                drop(heap);
                return event;
            }
            drop(heap);

            // Wait for notification
            self.notify.notified().await;
        }
    }

    /// Get current queue depth
    pub async fn depth(&self) -> usize {
        let heap = self.heap.lock().await;
        heap.len()
    }

    /// Check if queue is empty
    pub async fn is_empty(&self) -> bool {
        let heap = self.heap.lock().await;
        heap.is_empty()
    }

    /// Get current metrics snapshot
    pub async fn metrics(&self) -> PriorityQueueMetrics {
        let metrics = self.metrics.lock().await;
        metrics.clone()
    }

    /// Reset metrics
    pub async fn reset_metrics(&self) {
        let mut metrics = self.metrics.lock().await;
        *metrics = PriorityQueueMetrics::default();
    }

    /// Drain all events from the queue (for shutdown)
    pub async fn drain(&self) -> Vec<ArcSourceEvent> {
        let mut heap = self.heap.lock().await;
        let events: Vec<ArcSourceEvent> = heap.drain().map(|pq_event| pq_event.event).collect();

        let mut metrics = self.metrics.lock().await;
        metrics.current_depth = 0;

        debug!("Drained {} events from priority queue", events.len());
        events
    }
}

impl Clone for PriorityQueue {
    fn clone(&self) -> Self {
        Self {
            heap: Arc::clone(&self.heap),
            notify: Arc::clone(&self.notify),
            max_capacity: self.max_capacity,
            metrics: Arc::clone(&self.metrics),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::events::{SourceEvent, SourceEventWrapper};
    use chrono::Utc;
    use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};

    fn create_test_event(source_id: &str, timestamp: chrono::DateTime<Utc>) -> ArcSourceEvent {
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
        pq.enqueue(event1.clone()).await;
        pq.enqueue(event3.clone()).await;
        pq.enqueue(event2.clone()).await;

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
}
