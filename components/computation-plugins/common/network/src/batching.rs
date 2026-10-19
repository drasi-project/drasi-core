// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_contracts::SinkBatching;
pub use drasi_http_common::adaptive_batcher::AdaptiveBatchConfig;
use drasi_http_common::adaptive_batcher::ThroughputMonitor;

pub struct AdaptivePolicy {
    config: AdaptiveBatchConfig,
    monitor: ThroughputMonitor,
    max_bytes: usize,
}

impl AdaptivePolicy {
    pub fn new(config: AdaptiveBatchConfig, max_bytes: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            config.min_batch_size > 0
                && config.min_batch_size <= config.max_batch_size
                && config.max_batch_size <= 65_536
                && !config.throughput_window.is_zero()
                && !config.min_wait_time.is_zero()
                && config.min_wait_time <= config.max_wait_time
                && config.max_wait_time <= std::time::Duration::from_secs(60)
                && config.throughput_window <= std::time::Duration::from_secs(3600),
            "invalid native adaptive batching bounds"
        );
        let policy = Self {
            monitor: ThroughputMonitor::new(config.throughput_window),
            config,
            max_bytes,
        };
        policy.current().validate()?;
        Ok(policy)
    }

    pub fn current(&self) -> SinkBatching {
        let (operations, delay) = self.config.parameters(self.monitor.get_throughput_level());
        SinkBatching {
            max_inputs: self.config.max_batch_size.min(1024),
            max_operations: operations,
            max_bytes: self.max_bytes,
            max_delay_ms: delay.as_millis() as u64,
        }
    }

    pub fn record(&mut self, operations: usize) {
        self.monitor.record_batch(operations);
    }
}
