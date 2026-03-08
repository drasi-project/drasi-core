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

//! Open511 API polling client.

use crate::config::Open511SourceConfig;
use crate::models::{Open511Event, Open511EventsResponse};
use anyhow::Result;
use std::time::Duration;

/// API client used by source and bootstrap implementations.
#[derive(Clone)]
pub struct Open511ApiClient {
    config: Open511SourceConfig,
    client: reqwest::Client,
}

impl Open511ApiClient {
    /// Construct a new API client.
    pub fn new(config: Open511SourceConfig) -> Result<Self> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()?;
        Ok(Self { config, client })
    }

    /// Fetch all events using pagination.
    ///
    /// If `updated_since` is provided, an incremental query (`updated=>timestamp`) is used.
    pub async fn fetch_all_events(&self, updated_since: Option<&str>) -> Result<Vec<Open511Event>> {
        let mut events = Vec::new();
        let mut offset: usize = 0;
        let page_size = self.config.page_size;

        loop {
            let mut page = self
                .fetch_events_page(offset, page_size, updated_since)
                .await?;
            let page_count = page.events.len();

            events.append(&mut page.events);

            if page_count < page_size {
                break;
            }
            offset = offset
                .checked_add(page_size)
                .ok_or_else(|| anyhow::anyhow!("Open511 pagination offset overflow"))?;
        }

        Ok(events)
    }

    async fn fetch_events_page(
        &self,
        offset: usize,
        limit: usize,
        updated_since: Option<&str>,
    ) -> Result<Open511EventsResponse> {
        let url = format!("{}/events", self.config.base_url.trim_end_matches('/'));
        let query = build_query_params(&self.config, offset, limit, updated_since);

        let response = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .query(&query)
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(anyhow::anyhow!(
                "Open511 API rate limited request (HTTP 429). Body: {}",
                truncate_for_log(&body)
            ));
        }

        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Open511 API request failed with status {}. Body: {}",
                status,
                truncate_for_log(&body)
            ));
        }

        let parsed = serde_json::from_str::<Open511EventsResponse>(&body).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse Open511 events JSON response: {}. Body: {}",
                e,
                truncate_for_log(&body)
            )
        })?;

        Ok(parsed)
    }
}

fn truncate_for_log(body: &str) -> String {
    const MAX_LEN: usize = 512;
    if body.len() <= MAX_LEN {
        body.to_string()
    } else {
        format!("{}...", &body[..MAX_LEN])
    }
}

pub(crate) fn build_query_params(
    config: &Open511SourceConfig,
    offset: usize,
    limit: usize,
    updated_since: Option<&str>,
) -> Vec<(String, String)> {
    let mut params = vec![
        ("format".to_string(), "json".to_string()),
        ("offset".to_string(), offset.to_string()),
        ("limit".to_string(), limit.to_string()),
    ];

    if let Some(ref status) = config.status_filter {
        params.push(("status".to_string(), status.clone()));
    }
    if let Some(ref severity) = config.severity_filter {
        params.push(("severity".to_string(), severity.clone()));
    }
    if let Some(ref event_type) = config.event_type_filter {
        params.push(("event_type".to_string(), event_type.clone()));
    }
    if let Some(ref area_id) = config.area_id_filter {
        params.push(("area_id".to_string(), area_id.clone()));
    }
    if let Some(ref road_name) = config.road_name_filter {
        params.push(("road_name".to_string(), road_name.clone()));
    }
    if let Some(ref jurisdiction) = config.jurisdiction_filter {
        params.push(("jurisdiction".to_string(), jurisdiction.clone()));
    }
    if let Some(ref bbox) = config.bbox_filter {
        params.push(("bbox".to_string(), bbox.clone()));
    }

    if let Some(since) = updated_since {
        params.push(("updated".to_string(), format!(">{since}")));
    }

    params
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{InitialCursorBehavior, Open511SourceConfig};

    #[test]
    fn query_params_include_filters_and_updated() {
        let config = Open511SourceConfig {
            base_url: "https://api.open511.gov.bc.ca".to_string(),
            poll_interval_secs: 60,
            full_sweep_interval: 10,
            request_timeout_secs: 15,
            page_size: 500,
            status_filter: Some("ACTIVE".to_string()),
            severity_filter: Some("MAJOR".to_string()),
            event_type_filter: Some("INCIDENT".to_string()),
            area_id_filter: Some("drivebc.ca/3".to_string()),
            road_name_filter: Some("Highway 1".to_string()),
            jurisdiction_filter: Some("drivebc.ca".to_string()),
            bbox_filter: Some("-123.45,48.99,-122.45,49.49".to_string()),
            auto_delete_archived: false,
            initial_cursor_behavior: InitialCursorBehavior::StartFromBeginning,
        };

        let params = build_query_params(&config, 0, 500, Some("2026-01-01T00:00:00Z"));
        let map: std::collections::HashMap<String, String> = params.into_iter().collect();

        assert_eq!(map.get("format"), Some(&"json".to_string()));
        assert_eq!(map.get("status"), Some(&"ACTIVE".to_string()));
        assert_eq!(map.get("severity"), Some(&"MAJOR".to_string()));
        assert_eq!(
            map.get("updated"),
            Some(&">2026-01-01T00:00:00Z".to_string())
        );
    }
}
