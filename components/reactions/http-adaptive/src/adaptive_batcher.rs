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

use log::{debug, trace};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// Throughput levels for adaptive batching
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ThroughputLevel {
    Idle,   // < 1 msg/sec
    Low,    // 1-100 msgs/sec
    Medium, // 100-1,000 msgs/sec
    High,   // 1,000-10,000 msgs/sec
    Burst,  // > 10,000 msgs/sec
}

/// Configuration for adaptive batching
#[derive(Debug, Clone)]
pub struct AdaptiveBatchConfig {
    /// Maximum number of items in a batch
    pub max_batch_size: usize,
    /// Minimum number of items to consider efficient batching
    pub min_batch_size: usize,
    /// Maximum time to wait for batch to fill
    pub max_wait_time: Duration,
    /// Minimum time to wait (allows for message coalescing)
    pub min_wait_time: Duration,
    /// Window size for throughput calculation
    pub throughput_window: Duration,
    /// Enable adaptive parameter adjustment
    pub adaptive_enabled: bool,
}

impl Default for AdaptiveBatchConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 1000,
            min_batch_size: 10,
            max_wait_time: Duration::from_millis(100),
            min_wait_time: Duration::from_millis(1),
            throughput_window: Duration::from_secs(5),
            adaptive_enabled: true,
        }
    }
}

impl AdaptiveBatchConfig {
    /// Calculate recommended channel capacity for the internal batching channel
    ///
    /// Returns `max_batch_size × 5` to provide sufficient buffering for:
    /// - **Pipeline parallelism**: Next batch can accumulate while current batch is being processed
    /// - **Burst handling**: Absorbs temporary traffic spikes without triggering backpressure
    /// - **Throughput smoothing**: Reduces frequency of blocking on channel sends
    ///
    /// # Buffer Multiplier Rationale
    ///
    /// The 5x multiplier is chosen to balance memory usage and throughput:
    /// - **2-3x**: Minimum for effective pipelining (current + next batch)
    /// - **5x**: Good balance for most workloads (2-3 batches buffered)
    /// - **10x+**: For extreme burst scenarios (may use excessive memory)
    ///
    /// # Scaling Examples
    ///
    /// | max_batch_size | Channel Capacity | Memory Impact (1KB/event) |
    /// |----------------|------------------|---------------------------|
    /// | 100            | 500              | ~500 KB                   |
    /// | 1,000          | 5,000            | ~5 MB                     |
    /// | 5,000          | 25,000           | ~25 MB                    |
    pub fn recommended_channel_capacity(&self) -> usize {
        self.max_batch_size * 5
    }
}

/// Monitors throughput and provides traffic classification
pub struct ThroughputMonitor {
    window_size: Duration,
    events: Vec<(Instant, usize)>, // (timestamp, batch_size)
}

impl ThroughputMonitor {
    pub fn new(window_size: Duration) -> Self {
        Self {
            window_size,
            events: Vec::new(),
        }
    }

    pub fn record_batch(&mut self, batch_size: usize) {
        let now = Instant::now();
        self.events.push((now, batch_size));

        // Clean old events outside the window
        let cutoff = now - self.window_size;
        self.events.retain(|(timestamp, _)| *timestamp > cutoff);
    }

    pub fn get_throughput_level(&self) -> ThroughputLevel {
        if self.events.is_empty() {
            return ThroughputLevel::Idle;
        }

        let now = Instant::now();
        let window_start = now - self.window_size;

        // Calculate messages per second
        let total_messages: usize = self
            .events
            .iter()
            .filter(|(timestamp, _)| *timestamp > window_start)
            .map(|(_, size)| size)
            .sum();

        let elapsed = self.window_size.as_secs_f64();
        let msgs_per_sec = (total_messages as f64) / elapsed;

        match msgs_per_sec {
            x if x < 1.0 => ThroughputLevel::Idle,
            x if x < 100.0 => ThroughputLevel::Low,
            x if x < 1000.0 => ThroughputLevel::Medium,
            x if x < 10000.0 => ThroughputLevel::High,
            _ => ThroughputLevel::Burst,
        }
    }

    pub fn get_messages_per_second(&self) -> f64 {
        if self.events.is_empty() {
            return 0.0;
        }

        let now = Instant::now();
        let window_start = now - self.window_size;

        let total_messages: usize = self
            .events
            .iter()
            .filter(|(timestamp, _)| *timestamp > window_start)
            .map(|(_, size)| size)
            .sum();

        let elapsed = self.window_size.as_secs_f64();
        (total_messages as f64) / elapsed
    }
}

/// Adaptive batcher that adjusts batch size and timing based on throughput
pub struct AdaptiveBatcher<T> {
    receiver: mpsc::Receiver<T>,
    config: AdaptiveBatchConfig,
    monitor: ThroughputMonitor,
    current_batch_size: usize,
    current_wait_time: Duration,
}

impl<T> AdaptiveBatcher<T> {
    pub fn new(receiver: mpsc::Receiver<T>, config: AdaptiveBatchConfig) -> Self {
        let monitor = ThroughputMonitor::new(config.throughput_window);
        Self {
            receiver,
            current_batch_size: config.min_batch_size,
            current_wait_time: config.min_wait_time,
            monitor,
            config,
        }
    }

    /// Adjust batching parameters based on current throughput
    fn adapt_parameters(&mut self) {
        if !self.config.adaptive_enabled {
            return;
        }

        let level = self.monitor.get_throughput_level();
        let msgs_per_sec = self.monitor.get_messages_per_second();

        match level {
            ThroughputLevel::Idle => {
                // Optimize for latency - send immediately
                self.current_batch_size = self.config.min_batch_size;
                self.current_wait_time = self.config.min_wait_time;
            }
            ThroughputLevel::Low => {
                // Small batches, minimal wait
                self.current_batch_size =
                    (self.config.min_batch_size * 2).min(self.config.max_batch_size);
                self.current_wait_time = Duration::from_millis(1).max(self.config.min_wait_time);
            }
            ThroughputLevel::Medium => {
                // Moderate batching
                self.current_batch_size =
                    ((self.config.max_batch_size - self.config.min_batch_size) / 4
                        + self.config.min_batch_size)
                        .min(self.config.max_batch_size);
                self.current_wait_time = Duration::from_millis(10)
                    .max(self.config.min_wait_time)
                    .min(self.config.max_wait_time);
            }
            ThroughputLevel::High => {
                // Larger batches for efficiency
                self.current_batch_size =
                    ((self.config.max_batch_size - self.config.min_batch_size) / 2
                        + self.config.min_batch_size)
                        .min(self.config.max_batch_size);
                self.current_wait_time = Duration::from_millis(25)
                    .max(self.config.min_wait_time)
                    .min(self.config.max_wait_time);
            }
            ThroughputLevel::Burst => {
                // Maximum throughput mode
                self.current_batch_size = self.config.max_batch_size;
                self.current_wait_time = Duration::from_millis(50)
                    .max(self.config.min_wait_time)
                    .min(self.config.max_wait_time);
            }
        }

        trace!(
            "Adapted batching parameters - Level: {:?}, Rate: {:.1} msgs/sec, Batch: {}, Wait: {:?}",
            level, msgs_per_sec, self.current_batch_size, self.current_wait_time
        );
    }

    /// Collect the next batch of items
    pub async fn next_batch(&mut self) -> Option<Vec<T>> {
        let mut batch = Vec::new();

        // First, wait for at least one message
        match self.receiver.recv().await {
            Some(item) => batch.push(item),
            None => return None, // Channel closed
        }

        // Adapt parameters based on recent throughput
        self.adapt_parameters();

        // Now we have at least one message, decide how to batch
        let deadline = Instant::now() + self.current_wait_time;

        // Try to collect more messages up to current batch size
        while batch.len() < self.current_batch_size {
            // Check if more messages are immediately available
            match self.receiver.try_recv() {
                Ok(item) => {
                    batch.push(item);
                    // If we have a good batch size, consider sending
                    if batch.len() >= self.current_batch_size / 2 {
                        // Check if channel has many waiting messages (burst detection)
                        let pending = self.estimate_pending();
                        if pending > self.current_batch_size * 2 {
                            // Many messages waiting, fill the batch completely
                            while batch.len() < self.current_batch_size {
                                match self.receiver.try_recv() {
                                    Ok(item) => batch.push(item),
                                    Err(_) => break,
                                }
                            }
                            break;
                        }
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    // No immediate messages, wait up to deadline
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }

                    match timeout(remaining, self.receiver.recv()).await {
                        Ok(Some(item)) => batch.push(item),
                        Ok(None) => break, // Channel closed
                        Err(_) => break,   // Timeout
                    }
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        // Record batch for throughput monitoring
        self.monitor.record_batch(batch.len());

        debug!(
            "Adaptive batch collected - Size: {}, Target: {}, Wait: {:?}, Level: {:?}",
            batch.len(),
            self.current_batch_size,
            self.current_wait_time,
            self.monitor.get_throughput_level()
        );

        Some(batch)
    }

    /// Estimate number of pending messages (heuristic)
    fn estimate_pending(&self) -> usize {
        // Since we can't peek at the channel without consuming messages,
        // we use throughput level as a heuristic
        let throughput = self.monitor.get_throughput_level();
        match throughput {
            ThroughputLevel::Burst => 100,
            ThroughputLevel::High => 50,
            ThroughputLevel::Medium => 20,
            ThroughputLevel::Low => 5,
            ThroughputLevel::Idle => 0,
        }
    }
}

/// Simple non-adaptive batcher for comparison/fallback
#[allow(dead_code)]
pub struct FixedBatcher<T> {
    receiver: mpsc::Receiver<T>,
    batch_size: usize,
    timeout: Duration,
}

#[allow(dead_code)]
impl<T> FixedBatcher<T> {
    pub fn new(receiver: mpsc::Receiver<T>, batch_size: usize, timeout: Duration) -> Self {
        Self {
            receiver,
            batch_size,
            timeout,
        }
    }

    pub async fn next_batch(&mut self) -> Option<Vec<T>> {
        let mut batch = Vec::new();
        let deadline = Instant::now() + self.timeout;

        while batch.len() < self.batch_size {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() && !batch.is_empty() {
                break;
            }

            match timeout(remaining, self.receiver.recv()).await {
                Ok(Some(item)) => batch.push(item),
                Ok(None) => {
                    if batch.is_empty() {
                        return None;
                    }
                    break;
                }
                Err(_) => break,
            }
        }

        if batch.is_empty() {
            None
        } else {
            Some(batch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::sleep;

    #[test]
    fn test_recommended_channel_capacity() {
        // Default config: max_batch_size = 1000
        let config = AdaptiveBatchConfig::default();
        assert_eq!(
            config.recommended_channel_capacity(),
            5000,
            "Default config should recommend 5000 capacity (1000 × 5)"
        );

        // Small batch size
        let small_config = AdaptiveBatchConfig {
            max_batch_size: 100,
            min_batch_size: 10,
            max_wait_time: Duration::from_millis(100),
            min_wait_time: Duration::from_millis(1),
            throughput_window: Duration::from_secs(5),
            adaptive_enabled: true,
        };
        assert_eq!(
            small_config.recommended_channel_capacity(),
            500,
            "Small batch (100) should recommend 500 capacity (100 × 5)"
        );

        // Large batch size
        let large_config = AdaptiveBatchConfig {
            max_batch_size: 5000,
            min_batch_size: 100,
            max_wait_time: Duration::from_millis(500),
            min_wait_time: Duration::from_millis(1),
            throughput_window: Duration::from_secs(10),
            adaptive_enabled: true,
        };
        assert_eq!(
            large_config.recommended_channel_capacity(),
            25000,
            "Large batch (5000) should recommend 25000 capacity (5000 × 5)"
        );

        // Very small batch
        let tiny_config = AdaptiveBatchConfig {
            max_batch_size: 10,
            min_batch_size: 1,
            max_wait_time: Duration::from_millis(10),
            min_wait_time: Duration::from_millis(1),
            throughput_window: Duration::from_secs(1),
            adaptive_enabled: true,
        };
        assert_eq!(
            tiny_config.recommended_channel_capacity(),
            50,
            "Tiny batch (10) should recommend 50 capacity (10 × 5)"
        );
    }

    #[tokio::test]
    async fn test_throughput_monitor() {
        let mut monitor = ThroughputMonitor::new(Duration::from_secs(1));

        // Initially idle
        assert_eq!(monitor.get_throughput_level(), ThroughputLevel::Idle);

        // Add some events
        monitor.record_batch(10);
        sleep(Duration::from_millis(100)).await;
        monitor.record_batch(10);

        // Should be low throughput (20 msgs in 1 sec window)
        assert_eq!(monitor.get_throughput_level(), ThroughputLevel::Low);
    }

    #[tokio::test]
    async fn test_adaptive_batcher_low_traffic() {
        let (tx, rx) = mpsc::channel(100);
        let config = AdaptiveBatchConfig::default();
        let mut batcher = AdaptiveBatcher::new(rx, config);

        // Send a few messages slowly
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();

        let batch = batcher.next_batch().await.unwrap();
        assert!(batch.len() <= 10); // Should batch small for low traffic
    }

    #[tokio::test]
    async fn test_adaptive_batcher_burst() {
        let (tx, rx) = mpsc::channel(1000);
        let mut config = AdaptiveBatchConfig::default();
        // Start with slightly higher defaults for testing
        config.min_batch_size = 20;
        config.max_batch_size = 100;
        let mut batcher = AdaptiveBatcher::new(rx, config);

        // Send many messages quickly to simulate burst
        for i in 0..100 {
            tx.send(i).await.unwrap();
        }

        // First batch will use initial parameters
        let batch1 = batcher.next_batch().await.unwrap();
        assert!(batch1.len() >= 20); // Should at least hit min batch size

        // Send more messages for second batch
        for i in 100..200 {
            tx.send(i).await.unwrap();
        }

        // Second batch should adapt to burst pattern
        let batch2 = batcher.next_batch().await.unwrap();
        assert!(batch2.len() >= batch1.len()); // Should batch same or larger after observing burst
    }
}
