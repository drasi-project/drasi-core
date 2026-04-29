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

//! Open511 bootstrap provider.

pub mod descriptor;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::BootstrapEvent;
use drasi_source_open511::api::Open511ApiClient;
use drasi_source_open511::config::{InitialCursorBehavior, Open511SourceConfig};
use drasi_source_open511::mapping::{
    map_new_event, AFFECTS_ROAD_LABEL, AREA_LABEL, IN_AREA_LABEL, ROAD_EVENT_LABEL, ROAD_LABEL,
};
use std::collections::HashSet;

/// Open511 bootstrap provider configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Open511BootstrapConfig {
    pub base_url: String,
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_page_size")]
    pub page_size: usize,
    #[serde(default)]
    pub status_filter: Option<String>,
    #[serde(default)]
    pub severity_filter: Option<String>,
    #[serde(default)]
    pub event_type_filter: Option<String>,
    #[serde(default)]
    pub area_id_filter: Option<String>,
    #[serde(default)]
    pub road_name_filter: Option<String>,
    #[serde(default)]
    pub jurisdiction_filter: Option<String>,
    #[serde(default)]
    pub bbox_filter: Option<String>,
}

fn default_request_timeout_secs() -> u64 {
    15
}

fn default_page_size() -> usize {
    500
}

impl Default for Open511BootstrapConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            request_timeout_secs: default_request_timeout_secs(),
            page_size: default_page_size(),
            status_filter: Some("ACTIVE".to_string()),
            severity_filter: None,
            event_type_filter: None,
            area_id_filter: None,
            road_name_filter: None,
            jurisdiction_filter: None,
            bbox_filter: None,
        }
    }
}

impl Open511BootstrapConfig {
    pub fn validate(&self) -> Result<()> {
        if self.base_url.trim().is_empty() {
            return Err(anyhow::anyhow!("Open511 bootstrap base_url is required"));
        }
        if self.request_timeout_secs == 0 {
            return Err(anyhow::anyhow!("request_timeout_secs must be > 0"));
        }
        if self.page_size == 0 || self.page_size > 500 {
            return Err(anyhow::anyhow!("page_size must be between 1 and 500"));
        }
        Ok(())
    }
}

/// Bootstrap provider implementation for Open511.
pub struct Open511BootstrapProvider {
    config: Open511BootstrapConfig,
}

impl Open511BootstrapProvider {
    pub fn new(config: Open511BootstrapConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self { config })
    }

    pub fn builder() -> Open511BootstrapProviderBuilder {
        Open511BootstrapProviderBuilder::new()
    }
}

/// Builder for [`Open511BootstrapProvider`].
pub struct Open511BootstrapProviderBuilder {
    config: Open511BootstrapConfig,
}

impl Open511BootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            config: Open511BootstrapConfig::default(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.config.base_url = base_url.into();
        self
    }

    pub fn with_request_timeout_secs(mut self, request_timeout_secs: u64) -> Self {
        self.config.request_timeout_secs = request_timeout_secs;
        self
    }

    pub fn with_page_size(mut self, page_size: usize) -> Self {
        self.config.page_size = page_size;
        self
    }

    pub fn with_status_filter(mut self, status_filter: impl Into<String>) -> Self {
        self.config.status_filter = Some(status_filter.into());
        self
    }

    pub fn with_severity_filter(mut self, severity_filter: impl Into<String>) -> Self {
        self.config.severity_filter = Some(severity_filter.into());
        self
    }

    pub fn with_event_type_filter(mut self, event_type_filter: impl Into<String>) -> Self {
        self.config.event_type_filter = Some(event_type_filter.into());
        self
    }

    pub fn with_area_id_filter(mut self, area_id_filter: impl Into<String>) -> Self {
        self.config.area_id_filter = Some(area_id_filter.into());
        self
    }

    pub fn with_road_name_filter(mut self, road_name_filter: impl Into<String>) -> Self {
        self.config.road_name_filter = Some(road_name_filter.into());
        self
    }

    pub fn with_jurisdiction_filter(mut self, jurisdiction_filter: impl Into<String>) -> Self {
        self.config.jurisdiction_filter = Some(jurisdiction_filter.into());
        self
    }

    pub fn with_bbox_filter(mut self, bbox_filter: impl Into<String>) -> Self {
        self.config.bbox_filter = Some(bbox_filter.into());
        self
    }

    pub fn build(self) -> Result<Open511BootstrapProvider> {
        Open511BootstrapProvider::new(self.config)
    }
}

impl Default for Open511BootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BootstrapProvider for Open511BootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: drasi_lib::channels::BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        let source_config = Open511SourceConfig {
            base_url: self.config.base_url.clone(),
            poll_interval_secs: 60,
            full_sweep_interval: 10,
            request_timeout_secs: self.config.request_timeout_secs,
            page_size: self.config.page_size,
            status_filter: self.config.status_filter.clone(),
            severity_filter: self.config.severity_filter.clone(),
            event_type_filter: self.config.event_type_filter.clone(),
            area_id_filter: self.config.area_id_filter.clone(),
            road_name_filter: self.config.road_name_filter.clone(),
            jurisdiction_filter: self.config.jurisdiction_filter.clone(),
            bbox_filter: self.config.bbox_filter.clone(),
            auto_delete_archived: false,
            initial_cursor_behavior: InitialCursorBehavior::StartFromBeginning,
        };

        let api_client = Open511ApiClient::new(source_config)?;
        let events = api_client.fetch_all_events(None).await?;

        let requested_nodes: HashSet<String> = request.node_labels.into_iter().collect();
        let requested_relations: HashSet<String> = request.relation_labels.into_iter().collect();

        let mut known_areas = HashSet::new();
        let mut sent = 0usize;

        for event in events {
            let changes = map_new_event(&event, &context.source_id, &mut known_areas);
            for change in changes {
                if !should_send_change(&change, &requested_nodes, &requested_relations) {
                    continue;
                }

                event_tx
                    .send(BootstrapEvent {
                        source_id: context.source_id.clone(),
                        change,
                        timestamp: Utc::now(),
                        sequence: context.next_sequence(),
                    })
                    .await?;
                sent = sent.saturating_add(1);
            }
        }

        Ok(sent)
    }
}

fn should_send_change(
    change: &drasi_core::models::SourceChange,
    requested_nodes: &HashSet<String>,
    requested_relations: &HashSet<String>,
) -> bool {
    // Empty sets mean "no label constraints" in bootstrap requests.
    if requested_nodes.is_empty() && requested_relations.is_empty() {
        return true;
    }

    let label_matches = |label: &str, set: &HashSet<String>| set.is_empty() || set.contains(label);

    match change {
        drasi_core::models::SourceChange::Insert { element }
        | drasi_core::models::SourceChange::Update { element } => match element {
            drasi_core::models::Element::Node { metadata, .. } => metadata
                .labels
                .iter()
                .any(|label| label_matches(label, requested_nodes)),
            drasi_core::models::Element::Relation { metadata, .. } => metadata
                .labels
                .iter()
                .any(|label| label_matches(label, requested_relations)),
        },
        drasi_core::models::SourceChange::Delete { metadata } => {
            metadata.labels.iter().any(|label| {
                label_matches(label, requested_nodes) || label_matches(label, requested_relations)
            })
        }
        drasi_core::models::SourceChange::Future { .. } => false,
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "open511-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::Open511BootstrapDescriptor],
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn builder_requires_base_url() {
        let result = Open511BootstrapProvider::builder().build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_creates_provider() {
        let provider = Open511BootstrapProvider::builder()
            .with_base_url("https://api.open511.gov.bc.ca")
            .build()
            .expect("provider should build");

        assert_eq!(provider.config.status_filter, Some("ACTIVE".to_string()));
    }

    #[test]
    fn label_filter_detects_known_labels() {
        let mut node_labels = HashSet::new();
        node_labels.insert(ROAD_EVENT_LABEL.to_string());
        let relation_labels = HashSet::new();

        let event = drasi_core::models::Element::Node {
            metadata: drasi_core::models::ElementMetadata {
                reference: drasi_core::models::ElementReference::new("s1", "e1"),
                labels: Arc::from(vec![Arc::from(ROAD_EVENT_LABEL)]),
                effective_from: 0,
            },
            properties: drasi_core::models::ElementPropertyMap::new(),
        };
        let change = drasi_core::models::SourceChange::Insert { element: event };
        assert!(should_send_change(&change, &node_labels, &relation_labels));
    }

    #[test]
    fn label_constants_are_non_empty() {
        assert!(!ROAD_EVENT_LABEL.is_empty());
        assert!(!ROAD_LABEL.is_empty());
        assert!(!AREA_LABEL.is_empty());
        assert!(!AFFECTS_ROAD_LABEL.is_empty());
        assert!(!IN_AREA_LABEL.is_empty());
    }
}
