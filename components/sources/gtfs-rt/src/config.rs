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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_poll_interval_secs() -> u64 {
    30
}

fn default_timeout_secs() -> u64 {
    15
}

fn default_language() -> String {
    "en".to_string()
}

/// GTFS-RT feed categories supported by this source.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum GtfsRtFeedType {
    TripUpdates,
    VehiclePositions,
    Alerts,
}

impl GtfsRtFeedType {
    pub fn all() -> [Self; 3] {
        [
            GtfsRtFeedType::TripUpdates,
            GtfsRtFeedType::VehiclePositions,
            GtfsRtFeedType::Alerts,
        ]
    }

    pub fn key(&self) -> &'static str {
        match self {
            GtfsRtFeedType::TripUpdates => "trip_updates",
            GtfsRtFeedType::VehiclePositions => "vehicle_positions",
            GtfsRtFeedType::Alerts => "alerts",
        }
    }
}

/// Initial cursor behavior when no persisted feed cursor exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InitialCursorMode {
    /// Emit inserts for all elements in the first observed snapshot.
    StartFromBeginning,
    /// Use the first observed snapshot as a baseline and only emit subsequent changes.
    StartFromNow,
    /// Emit only elements whose effective timestamp is greater than this value (ms since epoch)
    /// on the first poll, then process all subsequent diffs normally.
    StartFromTimestamp(i64),
}

impl Default for InitialCursorMode {
    fn default() -> Self {
        Self::StartFromBeginning
    }
}

/// Source configuration for GTFS-RT polling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GtfsRtSourceConfig {
    pub trip_updates_url: Option<String>,
    pub vehicle_positions_url: Option<String>,
    pub alerts_url: Option<String>,
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub initial_cursor_mode: InitialCursorMode,
}

impl Default for GtfsRtSourceConfig {
    fn default() -> Self {
        Self {
            trip_updates_url: None,
            vehicle_positions_url: None,
            alerts_url: None,
            poll_interval_secs: default_poll_interval_secs(),
            headers: HashMap::new(),
            timeout_secs: default_timeout_secs(),
            language: default_language(),
            initial_cursor_mode: InitialCursorMode::default(),
        }
    }
}

impl GtfsRtSourceConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.trip_updates_url.is_none()
            && self.vehicle_positions_url.is_none()
            && self.alerts_url.is_none()
        {
            return Err(anyhow::anyhow!(
                "Validation error: at least one GTFS-RT feed URL must be configured"
            ));
        }

        if self.poll_interval_secs == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: poll_interval_secs must be greater than 0"
            ));
        }

        if self.timeout_secs == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: timeout_secs must be greater than 0"
            ));
        }

        if let InitialCursorMode::StartFromTimestamp(ts) = self.initial_cursor_mode {
            if ts < 0 {
                return Err(anyhow::anyhow!(
                    "Validation error: start_from_timestamp must be >= 0"
                ));
            }
        }

        Ok(())
    }

    pub fn configured_feeds(&self) -> Vec<(GtfsRtFeedType, &str)> {
        let mut feeds = Vec::new();
        if let Some(url) = self.trip_updates_url.as_deref() {
            feeds.push((GtfsRtFeedType::TripUpdates, url));
        }
        if let Some(url) = self.vehicle_positions_url.as_deref() {
            feeds.push((GtfsRtFeedType::VehiclePositions, url));
        }
        if let Some(url) = self.alerts_url.as_deref() {
            feeds.push((GtfsRtFeedType::Alerts, url));
        }
        feeds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_requires_feed() {
        let config = GtfsRtSourceConfig::default();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_accepts_one_feed() {
        let config = GtfsRtSourceConfig {
            vehicle_positions_url: Some("http://localhost:1234/vp.pb".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_configured_feeds_contains_only_configured() {
        let config = GtfsRtSourceConfig {
            trip_updates_url: Some("a".to_string()),
            alerts_url: Some("b".to_string()),
            ..Default::default()
        };

        let feeds = config.configured_feeds();
        assert_eq!(feeds.len(), 2);
        assert_eq!(feeds[0].0, GtfsRtFeedType::TripUpdates);
        assert_eq!(feeds[1].0, GtfsRtFeedType::Alerts);
    }
}
