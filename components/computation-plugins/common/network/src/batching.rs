// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The one adaptive batching configuration and policy shared by every native
//! network plugin that batches: the HTTP and gRPC sinks (host collection
//! limits) and the HTTP source's volatile ingress prefetch.
//!
//! Every plugin publishes the same `adaptive` object
//! ([`AdaptiveBatchSettings`]), validates it with the same rule
//! ([`AdaptiveBatchSettings::validate`]) and derives its parameters from the
//! same throughput tiers. The only difference between users is the largest
//! batch they can hold: [`MAX_BATCH_SIZE`] for sinks, the configured
//! `ingressCapacity` for a source.

use drasi_computation_contracts::SinkBatching;
pub use drasi_http_common::adaptive_batcher::AdaptiveBatchConfig;
use drasi_http_common::adaptive_batcher::ThroughputMonitor;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Largest adaptive batch, in operations; the host's operation bound.
pub const MAX_BATCH_SIZE: usize = SinkBatching::MAX_OPERATIONS;
/// Largest adaptive wait, in milliseconds; the host's delay bound.
pub const MAX_WAIT_MS: u64 = SinkBatching::MAX_DELAY_MS;
/// Largest throughput observation window, in milliseconds (one hour).
pub const MAX_WINDOW_MS: u64 = 3_600_000;
/// Default smallest batch target, used at idle throughput.
pub const DEFAULT_MIN_BATCH_SIZE: usize = 10;
/// Default largest batch target, used at burst throughput.
pub const DEFAULT_MAX_BATCH_SIZE: usize = 1000;
/// Default wait at idle throughput. One millisecond lets concurrent arrivals
/// coalesce without delaying a sparse stream.
pub const DEFAULT_MIN_WAIT_MS: u64 = 1;
/// Default upper bound on any tier's wait.
pub const DEFAULT_MAX_WAIT_MS: u64 = 100;
/// Default throughput observation window.
pub const DEFAULT_WINDOW_MS: u64 = 5000;

fn min_batch_size() -> usize {
    DEFAULT_MIN_BATCH_SIZE
}
fn max_batch_size() -> usize {
    DEFAULT_MAX_BATCH_SIZE
}
fn min_wait_ms() -> u64 {
    DEFAULT_MIN_WAIT_MS
}
fn max_wait_ms() -> u64 {
    DEFAULT_MAX_WAIT_MS
}
fn window_ms() -> u64 {
    DEFAULT_WINDOW_MS
}

/// The published `adaptive` object. Its presence enables adaptive batching;
/// every field has a default and the serialized form always carries all of
/// them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdaptiveBatchSettings {
    /// Batch target at idle throughput.
    #[serde(default = "min_batch_size")]
    pub min_batch_size: usize,
    /// Batch target at burst throughput.
    #[serde(default = "max_batch_size")]
    pub max_batch_size: usize,
    /// Wait at idle throughput; busier tiers use 1, 10, 25 and 50 ms,
    /// clamped to `minWaitMs..=maxWaitMs`.
    #[serde(default = "min_wait_ms")]
    pub min_wait_ms: u64,
    /// Upper bound on any tier's wait.
    #[serde(default = "max_wait_ms")]
    pub max_wait_ms: u64,
    /// Throughput observation window.
    #[serde(default = "window_ms")]
    pub window_ms: u64,
}

impl Default for AdaptiveBatchSettings {
    fn default() -> Self {
        Self {
            min_batch_size: DEFAULT_MIN_BATCH_SIZE,
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            min_wait_ms: DEFAULT_MIN_WAIT_MS,
            max_wait_ms: DEFAULT_MAX_WAIT_MS,
            window_ms: DEFAULT_WINDOW_MS,
        }
    }
}

/// Why an `adaptive` object was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdaptiveSettingsError {
    #[error("adaptive.minBatchSize must be in 1..=adaptive.maxBatchSize ({max}), got {min}")]
    BatchSizeInverted { min: usize, max: usize },
    #[error("adaptive.maxBatchSize ({max}) exceeds the limit {limit} ({limit_name})")]
    BatchSizeTooLarge {
        max: usize,
        limit: usize,
        limit_name: &'static str,
    },
    #[error("adaptive.minWaitMs must be in 1..=adaptive.maxWaitMs ({max}), got {min}")]
    WaitInverted { min: u64, max: u64 },
    #[error("adaptive.maxWaitMs must be at most {MAX_WAIT_MS}, got {0}")]
    WaitTooLong(u64),
    #[error("adaptive.windowMs must be in 1..={MAX_WINDOW_MS}, got {0}")]
    WindowOutOfRange(u64),
}

/// The batch-size limit of one user of the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchLimit {
    pub value: usize,
    /// What the limit is, for error messages (for example `ingressCapacity`).
    pub name: &'static str,
}

impl BatchLimit {
    /// The host's operation bound, the limit of every batching sink.
    pub const SINK: Self = Self {
        value: MAX_BATCH_SIZE,
        name: "host batch operation bound",
    };

    /// A volatile source's ingress queue bound.
    pub fn ingress(capacity: usize) -> Self {
        Self {
            value: capacity.min(MAX_BATCH_SIZE),
            name: "ingressCapacity",
        }
    }
}

impl AdaptiveBatchSettings {
    /// The one validator: batch and wait bounds ordered and positive, the
    /// batch no larger than `limit`, wait and window within the host bounds.
    pub fn validate(&self, limit: BatchLimit) -> Result<(), AdaptiveSettingsError> {
        if self.min_batch_size == 0 || self.min_batch_size > self.max_batch_size {
            return Err(AdaptiveSettingsError::BatchSizeInverted {
                min: self.min_batch_size,
                max: self.max_batch_size,
            });
        }
        if self.max_batch_size > limit.value {
            return Err(AdaptiveSettingsError::BatchSizeTooLarge {
                max: self.max_batch_size,
                limit: limit.value,
                limit_name: limit.name,
            });
        }
        if self.min_wait_ms == 0 || self.min_wait_ms > self.max_wait_ms {
            return Err(AdaptiveSettingsError::WaitInverted {
                min: self.min_wait_ms,
                max: self.max_wait_ms,
            });
        }
        if self.max_wait_ms > MAX_WAIT_MS {
            return Err(AdaptiveSettingsError::WaitTooLong(self.max_wait_ms));
        }
        if !(1..=MAX_WINDOW_MS).contains(&self.window_ms) {
            return Err(AdaptiveSettingsError::WindowOutOfRange(self.window_ms));
        }
        Ok(())
    }

    /// Validates against `limit` and returns the runtime tier configuration.
    pub fn batch_config(
        &self,
        limit: BatchLimit,
    ) -> Result<AdaptiveBatchConfig, AdaptiveSettingsError> {
        self.validate(limit)?;
        Ok(AdaptiveBatchConfig {
            max_batch_size: self.max_batch_size,
            min_batch_size: self.min_batch_size,
            max_wait_time: Duration::from_millis(self.max_wait_ms),
            min_wait_time: Duration::from_millis(self.min_wait_ms),
            throughput_window: Duration::from_millis(self.window_ms),
            adaptive_enabled: true,
        })
    }

    /// The published description of the nested object, for a plugin's
    /// `adaptive` configuration field.
    pub fn field_description(purpose: &str, limit: &str) -> String {
        format!(
            "{purpose} An object with minBatchSize (default {DEFAULT_MIN_BATCH_SIZE}), \
             maxBatchSize (default {DEFAULT_MAX_BATCH_SIZE}, at most {limit}), minWaitMs \
             (default {DEFAULT_MIN_WAIT_MS}), maxWaitMs (default {DEFAULT_MAX_WAIT_MS}, at most \
             {MAX_WAIT_MS}) and windowMs (default {DEFAULT_WINDOW_MS}, at most {MAX_WINDOW_MS}); \
             unknown keys are rejected. Absent disables adaptive batching."
        )
    }
}

/// A sink's live adaptive host-batching declaration.
pub struct AdaptivePolicy {
    config: AdaptiveBatchConfig,
    monitor: ThroughputMonitor,
    max_bytes: usize,
}

impl AdaptivePolicy {
    /// Builds a sink policy from the published settings and the sink's host
    /// collection byte bound.
    pub fn from_settings(
        settings: &AdaptiveBatchSettings,
        max_bytes: usize,
    ) -> anyhow::Result<Self> {
        let config = settings.batch_config(BatchLimit::SINK)?;
        let policy = Self {
            monitor: ThroughputMonitor::new(config.throughput_window),
            config,
            max_bytes,
        };
        policy.current().validate()?;
        Ok(policy)
    }

    /// The host batching declaration for the current throughput tier.
    pub fn current(&self) -> SinkBatching {
        let (operations, delay) = self.config.parameters(self.monitor.get_throughput_level());
        SinkBatching {
            max_inputs: self.config.max_batch_size.min(SinkBatching::MAX_INPUTS),
            max_operations: operations,
            max_bytes: self.max_bytes,
            max_delay_ms: delay.as_millis() as u64,
        }
    }

    /// The largest batch this policy can request.
    pub fn max_batch_size(&self) -> usize {
        self.config.max_batch_size
    }

    pub fn record(&mut self, operations: usize) {
        self.monitor.record_batch(operations);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_are_published_in_full_and_unknown_keys_are_refused() {
        let settings: AdaptiveBatchSettings = serde_json::from_value(json!({})).unwrap();
        assert_eq!(settings, AdaptiveBatchSettings::default());
        assert_eq!(
            serde_json::to_value(&settings).unwrap(),
            json!({"minBatchSize": 10, "maxBatchSize": 1000, "minWaitMs": 1, "maxWaitMs": 100, "windowMs": 5000})
        );
        for stale in [
            json!({"adaptiveMaxBatchSize": 10}),
            json!({"adaptiveWindowSize": 10}),
            json!({"max_batch_size": 10}),
            json!({"enabled": true}),
        ] {
            assert!(
                serde_json::from_value::<AdaptiveBatchSettings>(stale.clone()).is_err(),
                "{stale}"
            );
        }
    }

    #[test]
    fn one_validator_applies_the_same_bounds_with_the_users_limit() {
        let ok = AdaptiveBatchSettings::default();
        assert!(ok.validate(BatchLimit::SINK).is_ok());
        assert!(ok.validate(BatchLimit::ingress(1000)).is_ok());
        assert_eq!(
            ok.validate(BatchLimit::ingress(999)),
            Err(AdaptiveSettingsError::BatchSizeTooLarge {
                max: 1000,
                limit: 999,
                limit_name: "ingressCapacity"
            })
        );
        let with = |change: fn(&mut AdaptiveBatchSettings)| {
            let mut settings = AdaptiveBatchSettings::default();
            change(&mut settings);
            settings.validate(BatchLimit::SINK)
        };
        assert!(matches!(
            with(|s| s.min_batch_size = 0),
            Err(AdaptiveSettingsError::BatchSizeInverted { .. })
        ));
        assert!(matches!(
            with(|s| s.min_batch_size = 1001),
            Err(AdaptiveSettingsError::BatchSizeInverted { .. })
        ));
        assert!(matches!(
            with(|s| s.max_batch_size = MAX_BATCH_SIZE + 1),
            Err(AdaptiveSettingsError::BatchSizeTooLarge { .. })
        ));
        assert!(with(|s| s.max_batch_size = MAX_BATCH_SIZE).is_ok());
        assert!(matches!(
            with(|s| s.min_wait_ms = 0),
            Err(AdaptiveSettingsError::WaitInverted { .. })
        ));
        assert!(matches!(
            with(|s| s.min_wait_ms = 101),
            Err(AdaptiveSettingsError::WaitInverted { .. })
        ));
        assert_eq!(
            with(|s| s.max_wait_ms = MAX_WAIT_MS + 1),
            Err(AdaptiveSettingsError::WaitTooLong(MAX_WAIT_MS + 1))
        );
        assert!(with(|s| s.max_wait_ms = MAX_WAIT_MS).is_ok());
        assert_eq!(
            with(|s| s.window_ms = 0),
            Err(AdaptiveSettingsError::WindowOutOfRange(0))
        );
        assert_eq!(
            with(|s| s.window_ms = MAX_WINDOW_MS + 1),
            Err(AdaptiveSettingsError::WindowOutOfRange(MAX_WINDOW_MS + 1))
        );
    }

    #[test]
    fn settings_map_to_identical_runtime_tiers() {
        let settings = AdaptiveBatchSettings {
            min_batch_size: 4,
            max_batch_size: 2000,
            min_wait_ms: 3,
            max_wait_ms: 40,
            window_ms: 2500,
        };
        let config = settings.batch_config(BatchLimit::SINK).unwrap();
        assert_eq!(config.min_batch_size, 4);
        assert_eq!(config.max_batch_size, 2000);
        assert_eq!(config.min_wait_time, Duration::from_millis(3));
        assert_eq!(config.max_wait_time, Duration::from_millis(40));
        assert_eq!(config.throughput_window, Duration::from_millis(2500));
        assert!(settings.batch_config(BatchLimit::ingress(1999)).is_err());
    }

    #[test]
    fn sink_policy_starts_idle_and_caps_inputs_at_the_host_bound() {
        let settings = AdaptiveBatchSettings {
            max_batch_size: 5000,
            ..AdaptiveBatchSettings::default()
        };
        let policy = AdaptivePolicy::from_settings(&settings, 1 << 20).unwrap();
        assert_eq!(
            policy.current(),
            SinkBatching {
                max_inputs: SinkBatching::MAX_INPUTS,
                max_operations: DEFAULT_MIN_BATCH_SIZE,
                max_bytes: 1 << 20,
                max_delay_ms: DEFAULT_MIN_WAIT_MS,
            }
        );
        assert_eq!(policy.max_batch_size(), 5000);
        assert!(AdaptivePolicy::from_settings(&settings, 0).is_err());
        let too_large = AdaptiveBatchSettings {
            max_batch_size: MAX_BATCH_SIZE + 1,
            ..AdaptiveBatchSettings::default()
        };
        assert!(AdaptivePolicy::from_settings(&too_large, 1 << 20).is_err());
    }
}
