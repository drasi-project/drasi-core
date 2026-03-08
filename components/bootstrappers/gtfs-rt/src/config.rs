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

use drasi_source_gtfs_rt::GtfsRtFeedType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_timeout_secs() -> u64 {
    15
}

fn default_language() -> String {
    "en".to_string()
}

/// Bootstrap configuration for GTFS-RT feeds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GtfsRtBootstrapConfig {
    pub trip_updates_url: Option<String>,
    pub vehicle_positions_url: Option<String>,
    pub alerts_url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_language")]
    pub language: String,
}

impl Default for GtfsRtBootstrapConfig {
    fn default() -> Self {
        Self {
            trip_updates_url: None,
            vehicle_positions_url: None,
            alerts_url: None,
            headers: HashMap::new(),
            timeout_secs: default_timeout_secs(),
            language: default_language(),
        }
    }
}

impl GtfsRtBootstrapConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.trip_updates_url.is_none()
            && self.vehicle_positions_url.is_none()
            && self.alerts_url.is_none()
        {
            return Err(anyhow::anyhow!(
                "Validation error: at least one GTFS-RT feed URL must be configured"
            ));
        }

        if self.timeout_secs == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: timeout_secs must be greater than 0"
            ));
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
        assert!(GtfsRtBootstrapConfig::default().validate().is_err());
    }

    #[test]
    fn test_configured_feeds_contains_only_configured() {
        let config = GtfsRtBootstrapConfig {
            alerts_url: Some("https://example.com/alerts.pb".to_string()),
            ..Default::default()
        };
        let feeds = config.configured_feeds();
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].0, GtfsRtFeedType::Alerts);
    }
}
