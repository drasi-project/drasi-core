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

use crate::config::{GtfsRtFeedType, GtfsRtSourceConfig, InitialCursorMode};
use crate::mapping::{
    build_graph_snapshot, diff_graph_snapshots, snapshot_insert_changes, GraphSnapshot,
};
use anyhow::Result;
use drasi_core::models::SourceChange;
use gtfs_realtime::{FeedEntity, FeedMessage};
use log::warn;
use prost::Message;
use reqwest::Client;
use std::collections::HashMap;
use xxhash_rust::xxh64::xxh64;

#[derive(Debug, Clone)]
pub struct PollResult {
    pub changes: Vec<SourceChange>,
    pub feed_timestamps: HashMap<GtfsRtFeedType, u64>,
    pub feed_entity_counts: HashMap<GtfsRtFeedType, usize>,
    pub changed_feeds: Vec<GtfsRtFeedType>,
}

#[derive(Default)]
struct PollState {
    initialized: bool,
    initial_mode: InitialCursorMode,
    entity_hashes: HashMap<GtfsRtFeedType, HashMap<String, u64>>,
    graph_snapshot: GraphSnapshot,
    /// Cache of last successful FeedMessage per feed type, used to fill in
    /// for feeds that fail on a given poll cycle.
    cached_feeds: HashMap<GtfsRtFeedType, FeedMessage>,
}

struct FetchResult {
    feeds: Vec<(GtfsRtFeedType, FeedMessage)>,
    feed_timestamps: HashMap<GtfsRtFeedType, u64>,
    feed_entity_counts: HashMap<GtfsRtFeedType, usize>,
    entity_hashes: HashMap<GtfsRtFeedType, HashMap<String, u64>>,
}

pub struct GtfsRtPoller {
    config: GtfsRtSourceConfig,
    client: Client,
    state: PollState,
}

impl GtfsRtPoller {
    pub fn new(config: GtfsRtSourceConfig, client: Client) -> Self {
        Self {
            state: PollState {
                initial_mode: config.initial_cursor_mode.clone(),
                ..Default::default()
            },
            config,
            client,
        }
    }

    pub fn set_initial_mode(&mut self, mode: InitialCursorMode) {
        self.state.initial_mode = mode;
    }

    pub async fn poll_once(&mut self, source_id: &str) -> Result<PollResult> {
        let fetch = self.fetch_feeds().await?;

        // Cache successful feeds for future fallback
        for (feed_type, feed) in &fetch.feeds {
            self.state.cached_feeds.insert(*feed_type, feed.clone());
        }

        // Build the complete feed list: successful feeds + cached data for failed ones
        let complete_feeds = self.build_complete_feeds(&fetch.feeds);

        let changed_feeds = self.diff_changed_feeds(&fetch.entity_hashes);
        let has_entity_changes = !self.state.initialized || !changed_feeds.is_empty();

        let mut changes = Vec::new();

        if !self.state.initialized || has_entity_changes {
            let current_snapshot = build_graph_snapshot(&complete_feeds, &self.config.language);

            if !self.state.initialized {
                changes = match self.state.initial_mode {
                    InitialCursorMode::StartFromBeginning => {
                        snapshot_insert_changes(&current_snapshot, source_id)?
                    }
                    InitialCursorMode::StartFromNow => Vec::new(),
                    InitialCursorMode::StartFromTimestamp(ts) => {
                        let ts = ts.max(0) as u64;
                        snapshot_insert_changes(&current_snapshot, source_id)?
                            .into_iter()
                            .filter(|change| change.get_transaction_time() > ts)
                            .collect()
                    }
                };
            } else {
                changes =
                    diff_graph_snapshots(&self.state.graph_snapshot, &current_snapshot, source_id)?;
            }

            self.state.graph_snapshot = current_snapshot;
        }

        self.state.entity_hashes = fetch.entity_hashes;
        self.state.initialized = true;

        Ok(PollResult {
            changes,
            feed_timestamps: fetch.feed_timestamps,
            feed_entity_counts: fetch.feed_entity_counts,
            changed_feeds,
        })
    }

    /// Build a complete feed list that includes cached data for any feeds
    /// that failed during the current poll cycle.
    fn build_complete_feeds(
        &self,
        successful_feeds: &[(GtfsRtFeedType, FeedMessage)],
    ) -> Vec<(GtfsRtFeedType, FeedMessage)> {
        let successful_types: std::collections::HashSet<GtfsRtFeedType> =
            successful_feeds.iter().map(|(ft, _)| *ft).collect();

        let mut complete = successful_feeds.to_vec();

        // Add cached feeds for any configured feeds that failed this cycle
        for (feed_type, _) in self.config.configured_feeds() {
            if !successful_types.contains(&feed_type) {
                if let Some(cached) = self.state.cached_feeds.get(&feed_type) {
                    complete.push((feed_type, cached.clone()));
                }
            }
        }

        complete
    }

    fn diff_changed_feeds(
        &self,
        current_hashes: &HashMap<GtfsRtFeedType, HashMap<String, u64>>,
    ) -> Vec<GtfsRtFeedType> {
        let mut changed = Vec::new();

        for feed_type in GtfsRtFeedType::all() {
            let current = current_hashes.get(&feed_type);
            let previous = self.state.entity_hashes.get(&feed_type);
            if current != previous {
                changed.push(feed_type);
            }
        }

        changed
    }

    async fn fetch_feeds(&self) -> Result<FetchResult> {
        let mut feeds = Vec::new();
        let mut feed_timestamps = HashMap::new();
        let mut feed_entity_counts = HashMap::new();
        let mut entity_hashes = HashMap::new();
        let mut successes = 0usize;

        for (feed_type, url) in self.config.configured_feeds() {
            match self.fetch_single_feed(feed_type, url).await {
                Ok(feed) => {
                    feed_timestamps.insert(
                        feed_type,
                        feed.header.timestamp.unwrap_or(0).saturating_mul(1000),
                    );
                    feed_entity_counts.insert(feed_type, feed.entity.len());
                    entity_hashes.insert(feed_type, compute_entity_hashes(&feed.entity));
                    feeds.push((feed_type, feed));
                    successes += 1;
                }
                Err(err) => {
                    warn!(
                        "Failed to fetch GTFS-RT {} feed from '{}': {err}",
                        feed_type.key(),
                        url
                    );
                    // Carry forward previous hashes so this feed is treated as unchanged
                    if let Some(prev) = self.state.entity_hashes.get(&feed_type) {
                        entity_hashes.insert(feed_type, prev.clone());
                    }
                }
            }
        }

        if successes == 0 {
            anyhow::bail!("All configured GTFS-RT feeds failed to fetch");
        }

        Ok(FetchResult {
            feeds,
            feed_timestamps,
            feed_entity_counts,
            entity_hashes,
        })
    }

    async fn fetch_single_feed(&self, feed_type: GtfsRtFeedType, url: &str) -> Result<FeedMessage> {
        let mut request = self.client.get(url);
        for (key, value) in &self.config.headers {
            request = request.header(key, value);
        }

        let response = request.send().await?.error_for_status()?;
        let bytes = response.bytes().await?;
        let feed = FeedMessage::decode(bytes.as_ref())
            .map_err(|err| anyhow::anyhow!("Failed to decode {} feed: {err}", feed_type.key()))?;
        Ok(feed)
    }
}

fn compute_entity_hashes(entities: &[FeedEntity]) -> HashMap<String, u64> {
    entities
        .iter()
        .map(|entity| {
            let bytes = entity.encode_to_vec();
            let hash = xxh64(&bytes, 0);
            (entity.id.clone(), hash)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InitialCursorMode;
    use gtfs_realtime::{FeedEntity, FeedHeader, Position, VehicleDescriptor, VehiclePosition};

    fn make_feed(entity_id: &str, lat: f32) -> FeedMessage {
        FeedMessage {
            header: FeedHeader {
                gtfs_realtime_version: "2.0".to_string(),
                incrementality: None,
                timestamp: Some(1_720_000_000),
                feed_version: None,
            },
            entity: vec![FeedEntity {
                id: entity_id.to_string(),
                is_deleted: Some(false),
                trip_update: None,
                vehicle: Some(VehiclePosition {
                    trip: None,
                    vehicle: Some(VehicleDescriptor {
                        id: Some("veh-1".to_string()),
                        label: None,
                        license_plate: None,
                        wheelchair_accessible: None,
                    }),
                    position: Some(Position {
                        latitude: lat,
                        longitude: -104.9,
                        bearing: None,
                        odometer: None,
                        speed: None,
                    }),
                    current_stop_sequence: None,
                    stop_id: None,
                    current_status: None,
                    timestamp: Some(1_720_000_010),
                    congestion_level: None,
                    occupancy_status: None,
                    occupancy_percentage: None,
                    multi_carriage_details: vec![],
                }),
                alert: None,
                shape: None,
                stop: None,
                trip_modifications: None,
            }],
        }
    }

    #[tokio::test]
    async fn test_compute_entity_hashes_changes_when_entity_changes() {
        let feed_a = make_feed("entity-1", 39.7);
        let feed_b = make_feed("entity-1", 39.8);

        let hashes_a = compute_entity_hashes(&feed_a.entity);
        let hashes_b = compute_entity_hashes(&feed_b.entity);

        assert_ne!(hashes_a.get("entity-1"), hashes_b.get("entity-1"));
    }

    #[tokio::test]
    async fn test_start_from_timestamp_filters_initial_changes() {
        let config = GtfsRtSourceConfig {
            vehicle_positions_url: Some("http://localhost".to_string()),
            initial_cursor_mode: InitialCursorMode::StartFromTimestamp(9_999_999_999_999),
            ..Default::default()
        };
        let client = Client::builder().build().unwrap();
        let mut poller = GtfsRtPoller::new(config, client);

        // Avoid HTTP in this test by directly initializing state.
        poller.state.initialized = false;
        poller.state.initial_mode = InitialCursorMode::StartFromTimestamp(9_999_999_999_999);

        let snapshot = build_graph_snapshot(
            &[(
                GtfsRtFeedType::VehiclePositions,
                make_feed("entity-1", 39.7),
            )],
            "en",
        );
        let changes = snapshot_insert_changes(&snapshot, "source").unwrap();
        let filtered = changes
            .into_iter()
            .filter(|change| change.get_transaction_time() > 9_999_999_999_999)
            .collect::<Vec<_>>();

        assert!(filtered.is_empty());
    }
}
