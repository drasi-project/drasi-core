use std::sync::Arc;

use chrono::{DateTime, Utc};
use drasi_lib::channels::{events::Timestamped, priority_queue::PriorityQueue};

#[derive(Clone, Debug)]
struct Event {
    id: usize,
    timestamp: DateTime<Utc>,
}

impl Timestamped for Event {
    fn timestamp(&self) -> DateTime<Utc> {
        self.timestamp
    }
}

#[tokio::test]
async fn equal_timestamps_preserve_fifo_across_clones_and_metric_resets() {
    let queue = PriorityQueue::new(8);
    let other = queue.clone();
    let timestamp = Utc::now();
    for id in 0..8 {
        let event = Arc::new(Event { id, timestamp });
        if id % 2 == 0 {
            assert!(queue.enqueue(event).await);
        } else {
            other.enqueue_wait(event).await;
        }
        if id == 3 {
            queue.reset_metrics().await;
        }
    }
    for id in 0..8 {
        assert_eq!(queue.try_dequeue().await.unwrap().id, id);
    }
}

#[tokio::test]
async fn timestamp_precedence_is_preserved_with_fifo_ties() {
    let queue = PriorityQueue::new(4);
    let earlier = Utc::now();
    let later = earlier + chrono::Duration::seconds(1);
    for (id, timestamp) in [(0, later), (1, earlier), (2, earlier), (3, later)] {
        queue.enqueue_wait(Arc::new(Event { id, timestamp })).await;
    }
    for id in [1, 2, 0, 3] {
        assert_eq!(queue.dequeue().await.id, id);
    }
}
