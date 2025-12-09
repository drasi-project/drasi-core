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

use super::events::Timestamped;
use log::{debug, trace};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fmt::Debug;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

/// Wrapper for priority queue events with timestamp-based ordering
#[derive(Clone)]
struct PriorityQueueEvent<T>
where
    T: Timestamped + Clone + Send + Sync + 'static,
{
    event: Arc<T>,
}

impl<T> PriorityQueueEvent<T>
where
    T: Timestamped + Clone + Send + Sync + 'static,
{
    fn new(event: Arc<T>) -> Self {
        Self { event }
    }
}

// Implement ordering for priority queue (oldest events first - min-heap)
impl<T> PartialEq for PriorityQueueEvent<T>
where
    T: Timestamped + Clone + Send + Sync + 'static,
{
    fn eq(&self, other: &Self) -> bool {
        self.event.timestamp() == other.event.timestamp()
    }
}

impl<T> Eq for PriorityQueueEvent<T> where T: Timestamped + Clone + Send + Sync + 'static {}

impl<T> PartialOrd for PriorityQueueEvent<T>
where
    T: Timestamped + Clone + Send + Sync + 'static,
{
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for PriorityQueueEvent<T>
where
    T: Timestamped + Clone + Send + Sync + 'static,
{
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering for min-heap behavior (oldest first)
        other.event.timestamp().cmp(&self.event.timestamp())
    }
}

/// Metrics for monitoring priority queue performance (lock-free using atomics)
#[derive(Debug)]
pub struct PriorityQueueMetrics {
    pub total_enqueued: AtomicU64,
    pub total_dequeued: AtomicU64,
    pub current_depth: AtomicUsize,
    pub max_depth_seen: AtomicUsize,
    pub drops_due_to_capacity: AtomicU64,
    /// Number of times enqueue_wait() blocked waiting for capacity
    pub blocked_enqueue_count: AtomicU64,
}

impl PriorityQueueMetrics {
    /// Get a snapshot of the metrics as a MetricsSnapshot for easy access
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            total_enqueued: self.total_enqueued.load(AtomicOrdering::Relaxed),
            total_dequeued: self.total_dequeued.load(AtomicOrdering::Relaxed),
            current_depth: self.current_depth.load(AtomicOrdering::Relaxed),
            max_depth_seen: self.max_depth_seen.load(AtomicOrdering::Relaxed),
            drops_due_to_capacity: self.drops_due_to_capacity.load(AtomicOrdering::Relaxed),
            blocked_enqueue_count: self.blocked_enqueue_count.load(AtomicOrdering::Relaxed),
        }
    }
}

/// Non-atomic snapshot of priority queue metrics for easy reading
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub total_enqueued: u64,
    pub total_dequeued: u64,
    pub current_depth: usize,
    pub max_depth_seen: usize,
    pub drops_due_to_capacity: u64,
    /// Number of times enqueue_wait() blocked waiting for capacity
    pub blocked_enqueue_count: u64,
}

impl Default for PriorityQueueMetrics {
    fn default() -> Self {
        Self {
            total_enqueued: AtomicU64::new(0),
            total_dequeued: AtomicU64::new(0),
            current_depth: AtomicUsize::new(0),
            max_depth_seen: AtomicUsize::new(0),
            drops_due_to_capacity: AtomicU64::new(0),
            blocked_enqueue_count: AtomicU64::new(0),
        }
    }
}

/// Thread-safe generic priority queue for ordering events by timestamp
///
/// # Backpressure and Dispatch Modes
///
/// This priority queue supports two enqueue strategies:
///
/// ## Non-blocking (`enqueue()`)
/// - Returns immediately with `false` if queue is full
/// - Drops events when at capacity
/// - Suitable for **Broadcast dispatch mode** to prevent deadlock
/// - Events may be lost when consumers are slow
///
/// ## Blocking (`enqueue_wait()`)
/// - Waits until space is available before enqueuing
/// - Never drops events - provides backpressure
/// - **ONLY use with Channel dispatch mode** (isolated channels per subscriber)
/// - **DO NOT use with Broadcast mode** - will cause system-wide deadlock
///
/// # Usage with Dispatch Modes
///
/// ```ignore
/// // Channel mode (isolated channels) - SAFE to use blocking enqueue
/// if dispatch_mode == DispatchMode::Channel {
///     priority_queue.enqueue_wait(event).await;  // Blocks until space available
/// } else {
///     // Broadcast mode (shared channel) - MUST use non-blocking
///     if !priority_queue.enqueue(event).await {
///         warn!("Dropped event - queue at capacity");
///     }
/// }
/// ```
pub struct PriorityQueue<T>
where
    T: Timestamped + Clone + Send + Sync + 'static,
{
    /// Internal heap storing events (min-heap by timestamp)
    heap: Arc<Mutex<BinaryHeap<PriorityQueueEvent<T>>>>,
    /// Notification mechanism for waiting on new events
    notify: Arc<Notify>,
    /// Maximum queue capacity (for backpressure)
    max_capacity: usize,
    /// Metrics (using atomic operations for lock-free updates)
    metrics: Arc<PriorityQueueMetrics>,
}

impl<T> PriorityQueue<T>
where
    T: Timestamped + Clone + Send + Sync + Debug + 'static,
{
    /// Create a new priority queue with the specified maximum capacity
    pub fn new(max_capacity: usize) -> Self {
        Self {
            heap: Arc::new(Mutex::new(BinaryHeap::new())),
            notify: Arc::new(Notify::new()),
            max_capacity,
            metrics: Arc::new(PriorityQueueMetrics::default()),
        }
    }

    /// Enqueue an event into the priority queue
    /// Returns true if enqueued, false if queue is at capacity
    pub async fn enqueue(&self, event: Arc<T>) -> bool {
        let mut heap = self.heap.lock().await;

        // Check capacity
        if heap.len() >= self.max_capacity {
            let previous = self
                .metrics
                .drops_due_to_capacity
                .fetch_add(1, AtomicOrdering::Relaxed);
            let total_drops = previous + 1;

            if total_drops == 1 || total_drops % 100 == 0 {
                debug!(
                    "Priority queue at capacity ({}); {} events dropped so far",
                    self.max_capacity, total_drops
                );
            } else {
                trace!(
                    "Priority queue drop (capacity {}): {:?}",
                    self.max_capacity,
                    event
                );
            }
            drop(heap);
            return false;
        }

        // Enqueue event
        heap.push(PriorityQueueEvent::new(event));

        // Update metrics using atomic operations (lock-free)
        self.metrics
            .total_enqueued
            .fetch_add(1, AtomicOrdering::Relaxed);
        let current_depth = heap.len();
        self.metrics
            .current_depth
            .store(current_depth, AtomicOrdering::Relaxed);

        // Update max_depth_seen if needed (using compare-exchange loop)
        let mut max_seen = self.metrics.max_depth_seen.load(AtomicOrdering::Relaxed);
        while current_depth > max_seen {
            match self.metrics.max_depth_seen.compare_exchange_weak(
                max_seen,
                current_depth,
                AtomicOrdering::Relaxed,
                AtomicOrdering::Relaxed,
            ) {
                Ok(_) => break,
                Err(x) => max_seen = x,
            }
        }

        drop(heap);

        // Notify waiting dequeuers
        self.notify.notify_one();

        true
    }

    /// Enqueue an event into the priority queue, waiting if at capacity
    ///
    /// This method blocks until there is space available in the queue.
    /// It should only be used with Channel dispatch mode to prevent message loss.
    ///
    /// WARNING: Do NOT use with Broadcast dispatch mode - will cause deadlock!
    /// In broadcast mode, use the non-blocking `enqueue()` method instead.
    pub async fn enqueue_wait(&self, event: Arc<T>) {
        loop {
            let mut heap = self.heap.lock().await;

            // Check if there's capacity
            if heap.len() < self.max_capacity {
                // Space available - enqueue the event
                heap.push(PriorityQueueEvent::new(event));

                // Update metrics using atomic operations (lock-free)
                self.metrics
                    .total_enqueued
                    .fetch_add(1, AtomicOrdering::Relaxed);
                let current_depth = heap.len();
                self.metrics
                    .current_depth
                    .store(current_depth, AtomicOrdering::Relaxed);

                // Update max_depth_seen if needed (using compare-exchange loop)
                let mut max_seen = self.metrics.max_depth_seen.load(AtomicOrdering::Relaxed);
                while current_depth > max_seen {
                    match self.metrics.max_depth_seen.compare_exchange_weak(
                        max_seen,
                        current_depth,
                        AtomicOrdering::Relaxed,
                        AtomicOrdering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(x) => max_seen = x,
                    }
                }

                drop(heap);

                // Notify waiting dequeuers
                self.notify.notify_one();

                return;
            }

            // Queue is full - increment blocked count and wait
            let blocked_count = self
                .metrics
                .blocked_enqueue_count
                .fetch_add(1, AtomicOrdering::Relaxed)
                + 1;

            // Log blocking event periodically (every 100th block or first time)
            if blocked_count == 1 || blocked_count % 100 == 0 {
                debug!(
                    "Priority queue enqueue blocked (capacity {}); blocked {} times so far",
                    self.max_capacity, blocked_count
                );
            }

            // Drop lock and wait for notification
            drop(heap);

            // Wait for dequeue to create space
            self.notify.notified().await;
        }
    }

    /// Dequeue the oldest event from the priority queue (non-blocking)
    /// Returns None if queue is empty
    pub async fn try_dequeue(&self) -> Option<Arc<T>> {
        let mut heap = self.heap.lock().await;
        let event = heap.pop().map(|pq_event| pq_event.event);

        if event.is_some() {
            self.metrics
                .total_dequeued
                .fetch_add(1, AtomicOrdering::Relaxed);
            self.metrics
                .current_depth
                .store(heap.len(), AtomicOrdering::Relaxed);
            drop(heap);

            // Notify waiting enqueuers that space is available
            self.notify.notify_one();
        }

        event
    }

    /// Dequeue the oldest event, waiting if the queue is empty
    pub async fn dequeue(&self) -> Arc<T> {
        loop {
            // Try to dequeue
            let mut heap = self.heap.lock().await;
            if let Some(pq_event) = heap.pop() {
                let event = pq_event.event;
                self.metrics
                    .total_dequeued
                    .fetch_add(1, AtomicOrdering::Relaxed);
                self.metrics
                    .current_depth
                    .store(heap.len(), AtomicOrdering::Relaxed);
                drop(heap);

                // Notify waiting enqueuers that space is available
                self.notify.notify_one();

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
    pub async fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Reset metrics
    pub async fn reset_metrics(&self) {
        self.metrics
            .total_enqueued
            .store(0, AtomicOrdering::Relaxed);
        self.metrics
            .total_dequeued
            .store(0, AtomicOrdering::Relaxed);
        self.metrics.current_depth.store(0, AtomicOrdering::Relaxed);
        self.metrics
            .max_depth_seen
            .store(0, AtomicOrdering::Relaxed);
        self.metrics
            .drops_due_to_capacity
            .store(0, AtomicOrdering::Relaxed);
        self.metrics
            .blocked_enqueue_count
            .store(0, AtomicOrdering::Relaxed);
    }

    /// Drain all events from the queue (for shutdown)
    pub async fn drain(&self) -> Vec<Arc<T>> {
        let mut heap = self.heap.lock().await;
        let events: Vec<Arc<T>> = heap.drain().map(|pq_event| pq_event.event).collect();

        self.metrics.current_depth.store(0, AtomicOrdering::Relaxed);

        debug!("Drained {} events from priority queue", events.len());
        events
    }
}

impl<T> Clone for PriorityQueue<T>
where
    T: Timestamped + Clone + Send + Sync + 'static,
{
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
    use chrono::Utc;

    #[derive(Debug, Clone)]
    struct TestEvent {
        id: String,
        timestamp: chrono::DateTime<Utc>,
    }

    impl Timestamped for TestEvent {
        fn timestamp(&self) -> chrono::DateTime<Utc> {
            self.timestamp
        }
    }

    fn create_test_event(id: &str, timestamp: chrono::DateTime<Utc>) -> Arc<TestEvent> {
        Arc::new(TestEvent {
            id: id.to_string(),
            timestamp,
        })
    }

    #[tokio::test]
    async fn test_priority_queue_ordering() {
        let pq = PriorityQueue::new(100);

        // Create events with different timestamps
        let now = Utc::now();
        let event1 = create_test_event("event1", now);
        let event2 = create_test_event("event2", now - chrono::Duration::seconds(5));
        let event3 = create_test_event("event3", now + chrono::Duration::seconds(5));

        // Enqueue in random order
        pq.enqueue(event1).await;
        pq.enqueue(event3).await;
        pq.enqueue(event2).await;

        // Dequeue should return oldest first
        let dequeued1 = pq.try_dequeue().await.unwrap();
        assert_eq!(dequeued1.id, "event2"); // Oldest

        let dequeued2 = pq.try_dequeue().await.unwrap();
        assert_eq!(dequeued2.id, "event1"); // Middle

        let dequeued3 = pq.try_dequeue().await.unwrap();
        assert_eq!(dequeued3.id, "event3"); // Newest
    }

    #[tokio::test]
    async fn test_priority_queue_capacity() {
        let pq = PriorityQueue::new(2);

        let now = Utc::now();
        let event1 = create_test_event("event1", now);
        let event2 = create_test_event("event2", now);
        let event3 = create_test_event("event3", now);

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
        pq.enqueue(create_test_event("event1", now)).await;
        pq.enqueue(create_test_event("event2", now)).await;

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
            let event = create_test_event("event1", Utc::now());
            pq_clone.enqueue(event).await;
        });

        // This should block until the event arrives
        let event = pq.dequeue().await;
        assert_eq!(event.id, "event1");
    }

    #[tokio::test]
    async fn test_drain() {
        let pq = PriorityQueue::new(100);

        let now = Utc::now();
        pq.enqueue(create_test_event("event1", now)).await;
        pq.enqueue(create_test_event("event2", now)).await;
        pq.enqueue(create_test_event("event3", now)).await;

        let drained = pq.drain().await;
        assert_eq!(drained.len(), 3);
        assert!(pq.is_empty().await);
    }

    #[tokio::test]
    async fn test_enqueue_wait_blocks_when_full() {
        let pq = PriorityQueue::new(2);
        let now = Utc::now();

        // Fill queue to capacity
        pq.enqueue_wait(create_test_event("event1", now)).await;
        pq.enqueue_wait(create_test_event("event2", now)).await;

        // Verify queue is at capacity
        assert_eq!(pq.depth().await, 2);

        // Try to enqueue with a timeout - should block
        let pq_clone = pq.clone();
        let event3 = create_test_event("event3", now);
        let enqueue_task = tokio::spawn(async move {
            pq_clone.enqueue_wait(event3).await;
            "enqueued"
        });

        // Give it a moment to block
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Task should still be pending (blocked)
        assert!(!enqueue_task.is_finished());

        // Dequeue one item to make space
        pq.try_dequeue().await;

        // Now the blocked enqueue should complete
        let result =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), enqueue_task).await;

        assert!(result.is_ok(), "enqueue_wait should have unblocked");
        assert_eq!(pq.depth().await, 2); // Should be back at capacity
    }

    #[tokio::test]
    async fn test_enqueue_wait_unblocks_on_dequeue() {
        let pq = PriorityQueue::new(1);
        let now = Utc::now();

        // Fill queue
        pq.enqueue_wait(create_test_event("event1", now)).await;

        // Spawn task that will block on enqueue
        let pq_clone = pq.clone();
        let event2 = create_test_event("event2", now);
        let enqueue_task = tokio::spawn(async move {
            pq_clone.enqueue_wait(event2).await;
        });

        // Wait a bit to ensure it's blocked
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Dequeue to create space (this should notify the blocked enqueuer)
        let dequeued = pq.try_dequeue().await;
        assert!(dequeued.is_some());

        // The enqueue should complete now
        let result =
            tokio::time::timeout(tokio::time::Duration::from_millis(100), enqueue_task).await;

        assert!(result.is_ok(), "enqueue_wait should have been notified");
        assert_eq!(pq.depth().await, 1);
    }

    #[tokio::test]
    async fn test_enqueue_wait_multiple_waiters() {
        let pq = PriorityQueue::new(1);
        let now = Utc::now();

        // Fill queue
        pq.enqueue_wait(create_test_event("event1", now)).await;

        // Spawn multiple tasks that will block
        let mut tasks = vec![];
        for i in 2..=4 {
            let pq_clone = pq.clone();
            let event = create_test_event(&format!("event{}", i), now);
            let task = tokio::spawn(async move {
                pq_clone.enqueue_wait(event).await;
                i
            });
            tasks.push(task);
        }

        // Wait to ensure all are blocked
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Dequeue items one by one and verify waiting tasks complete
        for expected_id in 2..=4 {
            pq.try_dequeue().await;

            // One task should complete
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            let completed_count = tasks.iter().filter(|t| t.is_finished()).count();
            assert_eq!(
                completed_count,
                expected_id - 1,
                "Expected {} tasks to complete",
                expected_id - 1
            );
        }

        // Wait for all tasks to finish
        for task in tasks {
            tokio::time::timeout(tokio::time::Duration::from_millis(100), task)
                .await
                .expect("Task should complete")
                .expect("Task should not panic");
        }
    }

    #[tokio::test]
    async fn test_enqueue_wait_metrics() {
        let pq = PriorityQueue::new(2);
        let now = Utc::now();

        // Fill queue
        pq.enqueue_wait(create_test_event("event1", now)).await;
        pq.enqueue_wait(create_test_event("event2", now)).await;

        // Reset metrics to test blocked count
        pq.reset_metrics().await;

        // Spawn task that will block
        let pq_clone = pq.clone();
        let event3 = create_test_event("event3", now);
        let enqueue_task = tokio::spawn(async move {
            pq_clone.enqueue_wait(event3).await;
        });

        // Wait for it to block
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Check metrics - blocked count should be at least 1
        // Note: It might be higher if notify wakes it up but queue is still full
        let metrics = pq.metrics().await;
        assert!(
            metrics.blocked_enqueue_count >= 1,
            "Should have blocked at least once, got {}",
            metrics.blocked_enqueue_count
        );

        // Dequeue to unblock
        pq.try_dequeue().await;

        // Wait for enqueue to complete
        tokio::time::timeout(tokio::time::Duration::from_millis(100), enqueue_task)
            .await
            .expect("Task should complete")
            .expect("Task should not panic");

        // Metrics should show the enqueue completed
        let final_metrics = pq.metrics().await;
        assert_eq!(final_metrics.total_enqueued, 1);
        assert!(final_metrics.blocked_enqueue_count >= 1);
    }

    #[tokio::test]
    async fn test_enqueue_wait_vs_enqueue_behavior() {
        let pq = PriorityQueue::new(2);
        let now = Utc::now();

        // Fill queue
        pq.enqueue(create_test_event("event1", now)).await;
        pq.enqueue(create_test_event("event2", now)).await;

        // Non-blocking enqueue should fail
        let result = pq.enqueue(create_test_event("event3", now)).await;
        assert!(
            !result,
            "Non-blocking enqueue should return false when full"
        );

        // Check that drop counter increased
        let metrics = pq.metrics().await;
        assert_eq!(metrics.drops_due_to_capacity, 1);

        // Blocking enqueue_wait should succeed (after dequeue)
        let pq_clone = pq.clone();
        let event4 = create_test_event("event4", now);
        let enqueue_task = tokio::spawn(async move {
            pq_clone.enqueue_wait(event4).await;
        });

        // Dequeue to make space
        pq.try_dequeue().await;

        // Blocking enqueue should complete
        tokio::time::timeout(tokio::time::Duration::from_millis(100), enqueue_task)
            .await
            .expect("enqueue_wait should complete")
            .expect("Task should not panic");

        assert_eq!(pq.depth().await, 2);
    }
}
