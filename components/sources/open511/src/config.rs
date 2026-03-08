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

//! Configuration types for Open511 source polling.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Initial cursor behavior when no state exists in the state store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InitialCursorBehavior {
    /// Start by performing a full sweep and emit inserts for all current events.
    #[default]
    StartFromBeginning,
    /// Seed local snapshot from current API state, then only emit future changes.
    StartFromNow,
    /// Start with incremental polling from a fixed timestamp in milliseconds.
    StartFromTimestamp { timestamp_millis: i64 },
}

/// Open511 source configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Open511SourceConfig {
    /// Open511 API base URL (for example: `https://api.open511.gov.bc.ca`).
    pub base_url: String,
    /// Poll interval in seconds.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Perform a full sweep every N poll cycles.
    #[serde(default = "default_full_sweep_interval")]
    pub full_sweep_interval: u32,
    /// HTTP request timeout in seconds.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Page size for paginated polling.
    #[serde(default = "default_page_size")]
    pub page_size: usize,
    /// Optional status filter (for example: `ACTIVE`).
    #[serde(default)]
    pub status_filter: Option<String>,
    /// Optional severity filter (for example: `MAJOR`).
    #[serde(default)]
    pub severity_filter: Option<String>,
    /// Optional event type filter (for example: `INCIDENT`).
    #[serde(default)]
    pub event_type_filter: Option<String>,
    /// Optional area ID filter.
    #[serde(default)]
    pub area_id_filter: Option<String>,
    /// Optional road name filter.
    #[serde(default)]
    pub road_name_filter: Option<String>,
    /// Optional jurisdiction filter.
    #[serde(default)]
    pub jurisdiction_filter: Option<String>,
    /// Optional bounding box (`xmin,ymin,xmax,ymax`).
    #[serde(default)]
    pub bbox_filter: Option<String>,
    /// When true, events that transition to `ARCHIVED` status are automatically
    /// deleted from the graph. Off by default.
    #[serde(default)]
    pub auto_delete_archived: bool,
    /// Initial cursor behavior when no persisted state exists.
    #[serde(default)]
    pub initial_cursor_behavior: InitialCursorBehavior,
}

fn default_poll_interval_secs() -> u64 {
    60
}

fn default_full_sweep_interval() -> u32 {
    10
}

fn default_request_timeout_secs() -> u64 {
    15
}

fn default_page_size() -> usize {
    500
}

impl Default for Open511SourceConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            poll_interval_secs: default_poll_interval_secs(),
            full_sweep_interval: default_full_sweep_interval(),
            request_timeout_secs: default_request_timeout_secs(),
            page_size: default_page_size(),
            status_filter: Some("ACTIVE".to_string()),
            severity_filter: None,
            event_type_filter: None,
            area_id_filter: None,
            road_name_filter: None,
            jurisdiction_filter: None,
            bbox_filter: None,
            auto_delete_archived: false,
            initial_cursor_behavior: InitialCursorBehavior::default(),
        }
    }
}

impl Open511SourceConfig {
    /// Validate configuration fields.
    pub fn validate(&self) -> Result<()> {
        if self.base_url.trim().is_empty() {
            return Err(anyhow::anyhow!("Open511 base_url is required"));
        }

        if self.poll_interval_secs == 0 {
            return Err(anyhow::anyhow!("poll_interval_secs must be > 0"));
        }

        if self.full_sweep_interval == 0 {
            return Err(anyhow::anyhow!("full_sweep_interval must be > 0"));
        }

        if self.request_timeout_secs == 0 {
            return Err(anyhow::anyhow!("request_timeout_secs must be > 0"));
        }

        if self.page_size == 0 || self.page_size > 500 {
            return Err(anyhow::anyhow!("page_size must be between 1 and 500"));
        }

        if let InitialCursorBehavior::StartFromTimestamp { timestamp_millis } =
            self.initial_cursor_behavior
        {
            if timestamp_millis < 0 {
                return Err(anyhow::anyhow!(
                    "initial_cursor_behavior.start_from_timestamp must be >= 0"
                ));
            }
        }

        Ok(())
    }
}
