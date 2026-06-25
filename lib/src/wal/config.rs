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

//! Configuration types for Write-Ahead Log instances.

use super::error::WalError;

/// Minimum allowed value for [`WriteAheadLogConfig::max_events`].
///
/// Values below this would cause excessive eviction churn or make the WAL
/// effectively unusable for crash recovery.
pub const MIN_MAX_EVENTS: u64 = 16;

/// Per-source configuration supplied to [`WalProvider::register`](super::WalProvider::register).
#[derive(Debug, Clone)]
pub struct WriteAheadLogConfig {
    /// Maximum number of events retained in the WAL before the capacity policy triggers.
    pub max_events: u64,

    /// Policy to apply when the WAL reaches `max_events`.
    pub capacity_policy: CapacityPolicy,
}

/// Policy for handling new appends when the WAL is at capacity.
///
/// **`RejectIncoming`** propagates backpressure. For a transient source (e.g.,
/// HTTP webhook), this typically means returning 503 to the external producer,
/// which should retry. This preserves data safety but will cause the source to
/// appear "stuck" if the producer stops retrying or the consumer is permanently
/// stalled.
///
/// **`OverwriteOldest`** favors availability — keeps accepting new events by
/// evicting the oldest. Slow consumers may see gaps and trigger their recovery
/// policy. Choose this when availability matters more than no-loss replay.
///
/// Sources should choose based on their backpressure contract with upstream
/// producers.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CapacityPolicy {
    /// Reject the incoming event with [`WalError::CapacityExhausted`].
    #[default]
    RejectIncoming,

    /// Evict the oldest event(s) to make room for the new one.
    OverwriteOldest,
}

impl WriteAheadLogConfig {
    /// Validate the config. Returns [`WalError::InvalidConfig`] if `max_events`
    /// is below [`MIN_MAX_EVENTS`].
    pub fn validate(&self) -> Result<(), WalError> {
        if self.max_events < MIN_MAX_EVENTS {
            return Err(WalError::InvalidConfig(format!(
                "max_events must be at least {MIN_MAX_EVENTS}, got {}",
                self.max_events
            )));
        }
        Ok(())
    }
}

impl Default for WriteAheadLogConfig {
    fn default() -> Self {
        Self {
            max_events: 10_000,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
    }
}

/// Default max events for [`DurabilityConfig`].
fn default_max_events() -> u64 {
    10_000
}

/// User-facing durability configuration for transient sources.
///
/// When present and `enabled == true` on a transient source (HTTP, gRPC,
/// Application), the source will persist incoming events to a local WAL
/// before acknowledging the caller, enabling crash recovery and replay.
///
/// When absent or `enabled == false`, the source operates with zero
/// overhead — no WAL file is created.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DurabilityConfig {
    /// Whether WAL durability is enabled. Default: `false`.
    #[serde(default)]
    pub enabled: bool,

    /// Maximum number of events retained in the WAL before the capacity
    /// policy triggers. Default: 10,000.
    #[serde(default = "default_max_events")]
    pub max_events: u64,

    /// Policy to apply when the WAL reaches capacity.
    /// Default: [`CapacityPolicy::RejectIncoming`].
    #[serde(default)]
    pub capacity_policy: CapacityPolicy,
}

impl Default for DurabilityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_events: default_max_events(),
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
    }
}

impl DurabilityConfig {
    /// Returns `true` if durability is enabled.
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// Convert to the internal [`WriteAheadLogConfig`] used by `WalProvider::register()`.
    pub fn to_wal_config(&self) -> WriteAheadLogConfig {
        WriteAheadLogConfig {
            max_events: self.max_events,
            capacity_policy: self.capacity_policy,
        }
    }
}
