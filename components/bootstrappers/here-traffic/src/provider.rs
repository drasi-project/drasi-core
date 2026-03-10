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

//! HERE Traffic bootstrap provider implementation.

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use log::info;
use std::collections::HashSet;

use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::BootstrapEvent;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_source_here_traffic::mapping::{
    build_incident_change, build_relation_change, build_relations, build_segment_change, ChangeKind,
};
use drasi_source_here_traffic::{AuthMethod, Endpoint, HereTrafficConfig};
use drasi_source_here_traffic::{
    HereTrafficClient, TrafficIncidentSnapshot, TrafficSegmentSnapshot,
};

/// Bootstrap provider for HERE Traffic source.
pub struct HereTrafficBootstrapProvider {
    source_id: String,
    config: HereTrafficConfig,
    client: HereTrafficClient,
}

impl HereTrafficBootstrapProvider {
    pub fn new(source_id: impl Into<String>, config: HereTrafficConfig) -> Result<Self> {
        config.validate()?;
        let client = HereTrafficClient::new(config.auth.clone(), config.base_url.clone())?;
        Ok(Self {
            source_id: source_id.into(),
            config,
            client,
        })
    }

    pub fn builder() -> HereTrafficBootstrapProviderBuilder {
        HereTrafficBootstrapProviderBuilder::new()
    }
}

#[async_trait]
impl BootstrapProvider for HereTrafficBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: drasi_lib::channels::BootstrapEventSender,
        settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<usize> {
        info!(
            "HERE Traffic bootstrap starting for query '{}' (source '{}')",
            request.query_id, self.source_id
        );

        let (node_labels, relation_labels) = extract_labels(&request, settings);
        let wants_segments = label_requested(&node_labels, "TrafficSegment");
        let wants_incidents = label_requested(&node_labels, "TrafficIncident");
        let wants_relations = label_requested(&relation_labels, "AFFECTS");

        let mut segments: Vec<TrafficSegmentSnapshot> = Vec::new();
        let mut incidents: Vec<TrafficIncidentSnapshot> = Vec::new();
        let mut total_sent = 0usize;

        if self.config.endpoints.contains(&Endpoint::Flow) && wants_segments {
            let flow = self.client.get_flow(&self.config.bounding_box).await?;
            segments = drasi_source_here_traffic::mapping::extract_segments(flow);
            total_sent += send_changes(
                &self.source_id,
                context,
                &event_tx,
                segments
                    .iter()
                    .map(|segment| {
                        build_segment_change(&self.source_id, segment, ChangeKind::Insert)
                    })
                    .collect(),
            )
            .await?;
        }

        if self.config.endpoints.contains(&Endpoint::Incidents) && wants_incidents {
            let response = self.client.get_incidents(&self.config.bounding_box).await?;
            incidents = drasi_source_here_traffic::mapping::extract_incidents(response);
            total_sent += send_changes(
                &self.source_id,
                context,
                &event_tx,
                incidents
                    .iter()
                    .map(|incident| {
                        build_incident_change(&self.source_id, incident, ChangeKind::Insert)
                    })
                    .collect(),
            )
            .await?;
        }

        if wants_relations
            && self.config.endpoints.contains(&Endpoint::Flow)
            && self.config.endpoints.contains(&Endpoint::Incidents)
            && !segments.is_empty()
            && !incidents.is_empty()
        {
            let incident_map = incidents
                .iter()
                .map(|incident| (incident.id.clone(), incident.clone()))
                .collect();
            let segment_map = segments
                .iter()
                .map(|segment| (segment.id.clone(), segment.clone()))
                .collect();
            let relations = build_relations(&incident_map, &segment_map, &self.config);
            let relation_changes = relations
                .values()
                .map(|relation| {
                    build_relation_change(&self.source_id, relation, ChangeKind::Insert)
                })
                .collect();
            total_sent +=
                send_changes(&self.source_id, context, &event_tx, relation_changes).await?;
        }

        info!(
            "HERE Traffic bootstrap completed for query '{}', sent {total_sent} events",
            request.query_id
        );

        Ok(total_sent)
    }
}

/// Builder for HERE Traffic bootstrap provider.
pub struct HereTrafficBootstrapProviderBuilder {
    source_id: String,
    config: HereTrafficConfig,
}

impl HereTrafficBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            source_id: "here-traffic-bootstrap".to_string(),
            config: HereTrafficConfig::new("", ""),
        }
    }

    pub fn with_source_id(mut self, source_id: impl Into<String>) -> Self {
        self.source_id = source_id.into();
        self
    }

    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.config.auth = AuthMethod::ApiKey {
            api_key: api_key.into(),
        };
        self
    }

    pub fn with_oauth(
        mut self,
        access_key_id: impl Into<String>,
        access_key_secret: impl Into<String>,
    ) -> Self {
        self.config.auth = AuthMethod::OAuth {
            access_key_id: access_key_id.into(),
            access_key_secret: access_key_secret.into(),
            token_url: "https://account.api.here.com/oauth2/token".to_string(),
        };
        self
    }

    pub fn with_bounding_box(mut self, bounding_box: impl Into<String>) -> Self {
        self.config.bounding_box = bounding_box.into();
        self
    }

    pub fn with_endpoints(mut self, endpoints: Vec<Endpoint>) -> Self {
        self.config.endpoints = endpoints;
        self
    }

    pub fn with_incident_match_distance_meters(mut self, distance: f64) -> Self {
        self.config.incident_match_distance_meters = distance;
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.config.base_url = base_url.into();
        self
    }

    pub fn build(self) -> Result<HereTrafficBootstrapProvider> {
        match &self.config.auth {
            AuthMethod::ApiKey { api_key } => {
                if api_key.trim().is_empty() {
                    return Err(anyhow::anyhow!(
                        "api_key is required (or use with_oauth for OAuth auth)"
                    ));
                }
            }
            AuthMethod::OAuth {
                access_key_id,
                access_key_secret,
                ..
            } => {
                if access_key_id.trim().is_empty() || access_key_secret.trim().is_empty() {
                    return Err(anyhow::anyhow!(
                        "access_key_id and access_key_secret are both required for OAuth"
                    ));
                }
            }
        }
        if self.config.bounding_box.trim().is_empty() {
            return Err(anyhow::anyhow!("bounding_box is required"));
        }
        HereTrafficBootstrapProvider::new(self.source_id, self.config)
    }
}

impl Default for HereTrafficBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

async fn send_changes(
    source_id: &str,
    context: &BootstrapContext,
    event_tx: &drasi_lib::channels::BootstrapEventSender,
    changes: Vec<SourceChange>,
) -> Result<usize> {
    let mut count = 0;
    for change in changes {
        let sequence = context.next_sequence();
        let event = BootstrapEvent {
            source_id: source_id.to_string(),
            change,
            timestamp: Utc::now(),
            sequence,
        };
        event_tx
            .send(event)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send bootstrap event: {e}"))?;
        count += 1;
    }
    Ok(count)
}

fn extract_labels(
    request: &BootstrapRequest,
    settings: Option<&SourceSubscriptionSettings>,
) -> (HashSet<String>, HashSet<String>) {
    if let Some(settings) = settings {
        return (
            settings.nodes.iter().cloned().collect(),
            settings.relations.iter().cloned().collect(),
        );
    }

    (
        request.node_labels.iter().cloned().collect(),
        request.relation_labels.iter().cloned().collect(),
    )
}

fn label_requested(labels: &HashSet<String>, label: &str) -> bool {
    labels.is_empty() || labels.contains(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_requires_fields() {
        let result = HereTrafficBootstrapProvider::builder().build();
        assert!(result.is_err());
    }

    #[test]
    fn test_builder_success() {
        let provider = HereTrafficBootstrapProvider::builder()
            .with_api_key("test")
            .with_bounding_box("52.5,13.3,52.6,13.5")
            .build()
            .unwrap();

        assert_eq!(provider.source_id, "here-traffic-bootstrap");
    }
}
