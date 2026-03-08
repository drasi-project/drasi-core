#![allow(unexpected_cfgs)]
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

pub mod config;
pub mod descriptor;

pub use config::GtfsRtBootstrapConfig;

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{Element, SourceChange};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_source_gtfs_rt::mapping::{build_graph_snapshot, snapshot_insert_changes};
use drasi_source_gtfs_rt::GtfsRtFeedType;
use gtfs_realtime::FeedMessage;
use log::info;
use prost::Message;
use reqwest::Client;
use std::collections::HashMap;
use std::time::Duration;

/// GTFS-RT bootstrap provider.
pub struct GtfsRtBootstrapProvider {
    config: GtfsRtBootstrapConfig,
    client: Client,
}

impl GtfsRtBootstrapProvider {
    pub fn new(config: GtfsRtBootstrapConfig) -> Result<Self> {
        config.validate()?;

        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()?;

        Ok(Self { config, client })
    }

    pub fn builder() -> GtfsRtBootstrapProviderBuilder {
        GtfsRtBootstrapProviderBuilder::new()
    }
}

#[async_trait]
impl BootstrapProvider for GtfsRtBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        info!(
            "Starting GTFS-RT bootstrap for query '{}' on source '{}'",
            request.query_id, context.source_id
        );

        let feeds = fetch_feeds(&self.client, &self.config).await?;
        let snapshot = build_graph_snapshot(&feeds, &self.config.language);
        let changes = snapshot_insert_changes(&snapshot, &context.source_id)?;

        let mut sent = 0usize;

        for change in changes {
            if !should_send_change(&change, &request) {
                continue;
            }

            let event = BootstrapEvent {
                source_id: context.source_id.clone(),
                change,
                timestamp: chrono::Utc::now(),
                sequence: context.next_sequence(),
            };
            event_tx.send(event).await?;
            sent += 1;
        }

        info!(
            "Completed GTFS-RT bootstrap for query '{}' with {} events",
            request.query_id, sent
        );

        Ok(sent)
    }
}

fn should_send_change(change: &SourceChange, request: &BootstrapRequest) -> bool {
    if request.node_labels.is_empty() && request.relation_labels.is_empty() {
        return true;
    }

    let labels_match = |labels: &std::sync::Arc<[std::sync::Arc<str>]>, filters: &[String]| {
        filters.is_empty()
            || labels
                .iter()
                .any(|label| filters.iter().any(|filter| filter == label.as_ref()))
    };

    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => match element {
            Element::Node { metadata, .. } => labels_match(&metadata.labels, &request.node_labels),
            Element::Relation { metadata, .. } => {
                labels_match(&metadata.labels, &request.relation_labels)
            }
        },
        SourceChange::Delete { metadata } => {
            labels_match(&metadata.labels, &request.node_labels)
                || labels_match(&metadata.labels, &request.relation_labels)
        }
        SourceChange::Future { .. } => false,
    }
}

async fn fetch_feeds(
    client: &Client,
    config: &GtfsRtBootstrapConfig,
) -> Result<Vec<(GtfsRtFeedType, FeedMessage)>> {
    let mut feeds = Vec::new();

    for (feed_type, url) in config.configured_feeds() {
        let mut request = client.get(url);
        for (key, value) in &config.headers {
            request = request.header(key, value);
        }

        let response = request.send().await?.error_for_status()?;
        let bytes = response.bytes().await?;
        let feed = FeedMessage::decode(bytes.as_ref())?;
        feeds.push((feed_type, feed));
    }

    Ok(feeds)
}

/// Builder for `GtfsRtBootstrapProvider`.
pub struct GtfsRtBootstrapProviderBuilder {
    trip_updates_url: Option<String>,
    vehicle_positions_url: Option<String>,
    alerts_url: Option<String>,
    headers: HashMap<String, String>,
    timeout_secs: u64,
    language: String,
}

impl GtfsRtBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            trip_updates_url: None,
            vehicle_positions_url: None,
            alerts_url: None,
            headers: HashMap::new(),
            timeout_secs: 15,
            language: "en".to_string(),
        }
    }

    pub fn with_trip_updates_url(mut self, url: impl Into<String>) -> Self {
        self.trip_updates_url = Some(url.into());
        self
    }

    pub fn with_vehicle_positions_url(mut self, url: impl Into<String>) -> Self {
        self.vehicle_positions_url = Some(url.into());
        self
    }

    pub fn with_alerts_url(mut self, url: impl Into<String>) -> Self {
        self.alerts_url = Some(url.into());
        self
    }

    pub fn with_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }

    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    pub fn with_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = language.into();
        self
    }

    pub fn with_config(mut self, config: GtfsRtBootstrapConfig) -> Self {
        self.trip_updates_url = config.trip_updates_url;
        self.vehicle_positions_url = config.vehicle_positions_url;
        self.alerts_url = config.alerts_url;
        self.headers = config.headers;
        self.timeout_secs = config.timeout_secs;
        self.language = config.language;
        self
    }

    pub fn build(self) -> Result<GtfsRtBootstrapProvider> {
        let config = GtfsRtBootstrapConfig {
            trip_updates_url: self.trip_updates_url,
            vehicle_positions_url: self.vehicle_positions_url,
            alerts_url: self.alerts_url,
            headers: self.headers,
            timeout_secs: self.timeout_secs,
            language: self.language,
        };

        GtfsRtBootstrapProvider::new(config)
    }
}

impl Default for GtfsRtBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "gtfs-rt-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::GtfsRtBootstrapDescriptor],
);
