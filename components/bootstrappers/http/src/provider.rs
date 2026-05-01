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

//! HTTP Bootstrap Provider implementation.
//!
//! Fetches initial state from HTTP REST APIs and emits graph elements
//! through the bootstrap event channel.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use log::{debug, error, info, warn};
use reqwest::Client;
use std::time::Duration;

use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::BootstrapEvent;

use crate::auth::{self, ResolvedAuth};
use crate::config::{EndpointConfig, HttpBootstrapConfig, HttpMethod};
use crate::content_parser::{self, ContentType};
use crate::pagination::{self, NextPage, Paginator};
use crate::response;
use crate::template_engine::TemplateEngine;

/// Maximum number of pages to fetch per endpoint to prevent infinite loops.
const MAX_PAGES: u64 = 10_000;

/// HTTP Bootstrap Provider that fetches data from REST APIs.
pub struct HttpBootstrapProvider {
    config: HttpBootstrapConfig,
    client: Client,
    resolved_auths: Vec<Option<ResolvedAuth>>,
}

impl HttpBootstrapProvider {
    /// Create a new HTTP bootstrap provider from configuration.
    pub fn new(config: HttpBootstrapConfig) -> Result<Self> {
        let timeout = Duration::from_secs(config.timeout_seconds);
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .context("Failed to build HTTP client")?;

        // Resolve authentication for each endpoint
        let mut resolved_auths = Vec::new();
        for endpoint in &config.endpoints {
            let auth = match &endpoint.auth {
                Some(auth_config) => Some(
                    auth::resolve_auth(auth_config, &client)
                        .context("Failed to resolve authentication")?,
                ),
                None => None,
            };
            resolved_auths.push(auth);
        }

        Ok(Self {
            config,
            client,
            resolved_auths,
        })
    }

    /// Fetch all pages from a single endpoint and emit bootstrap events.
    async fn fetch_endpoint(
        &self,
        endpoint: &EndpointConfig,
        auth: &Option<ResolvedAuth>,
        context: &BootstrapContext,
        request: &BootstrapRequest,
        event_tx: &drasi_lib::channels::BootstrapEventSender,
    ) -> Result<u64> {
        let engine = TemplateEngine::new();
        let mut total_sent: u64 = 0;

        // Determine content type override
        let content_type_override = endpoint
            .response
            .content_type
            .as_ref()
            .map(ContentType::from_override);

        // Set up pagination
        let mut paginator: Option<Box<dyn Paginator>> = endpoint
            .pagination
            .as_ref()
            .map(pagination::create_paginator);

        // Get initial pagination params
        let initial_params: Vec<(String, String)> = paginator
            .as_ref()
            .map(|p| p.initial_params())
            .unwrap_or_default();

        let mut current_url = endpoint.url.clone();
        let mut current_params = initial_params;
        let mut page_num = 0u64;
        let mut last_url_and_params: Option<(String, Vec<(String, String)>)> = None;

        loop {
            page_num += 1;

            // Prevent infinite loops
            if page_num > MAX_PAGES {
                warn!(
                    "Reached maximum page limit ({MAX_PAGES}) for endpoint '{}', stopping pagination",
                    endpoint.url
                );
                break;
            }

            // Detect cycles: same URL + same params as last request
            let current_key = (current_url.clone(), current_params.clone());
            if let Some(ref last) = last_url_and_params {
                if *last == current_key {
                    warn!(
                        "Pagination cycle detected for endpoint '{}', stopping",
                        endpoint.url
                    );
                    break;
                }
            }
            last_url_and_params = Some(current_key);

            debug!("Fetching page {page_num} from endpoint: {}", endpoint.url);

            // Make the HTTP request with retries
            let (response_text, response_headers) = self
                .fetch_with_retry(&current_url, endpoint, auth, &current_params)
                .await
                .with_context(|| {
                    format!(
                        "Failed to fetch from endpoint '{}' (page {page_num})",
                        endpoint.url
                    )
                })?;

            // Determine content type from override or response header
            let ct = content_type_override.clone().unwrap_or_else(|| {
                let header_value = response_headers
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok());
                ContentType::from_header(header_value)
            });

            // Parse response body using the correct content type
            let parsed_body =
                content_parser::parse_body(&response_text, &ct).with_context(|| {
                    format!("Failed to parse response from '{}' as {ct:?}", endpoint.url)
                })?;

            // Extract items
            let items = response::extract_items(&parsed_body, &endpoint.response.items_path)?;
            let items_count = items.len();

            debug!(
                "Extracted {items_count} items from page {page_num} of {}",
                endpoint.url
            );

            // If no items, we're done
            if items_count == 0 {
                break;
            }

            // Map items to elements and emit events
            let element_results = response::map_items_to_elements(
                &items,
                &endpoint.response.mappings,
                &context.source_id,
                &engine,
            );

            for result in element_results {
                match result {
                    Ok(element) => {
                        // Filter by requested labels
                        if !should_include_element(&element, request) {
                            continue;
                        }

                        let source_change = SourceChange::Insert { element };
                        let sequence = context.next_sequence();

                        let bootstrap_event = BootstrapEvent {
                            source_id: context.source_id.clone(),
                            change: source_change,
                            timestamp: Utc::now(),
                            sequence,
                        };

                        event_tx
                            .send(bootstrap_event)
                            .await
                            .context("Failed to send bootstrap event")?;

                        total_sent += 1;
                    }
                    Err(e) => {
                        warn!("Failed to map item to element: {e}");
                    }
                }
            }

            // Check pagination for next page
            match paginator.as_mut() {
                Some(ref mut pag) => {
                    match pag.next_page(&parsed_body, &response_headers, items_count)? {
                        Some(NextPage::QueryParams(params)) => {
                            current_params = params;
                        }
                        Some(NextPage::NewUrl(url)) => {
                            current_url = url;
                            current_params = Vec::new();
                        }
                        None => break,
                    }
                }
                None => break, // No pagination, single page only
            }
        }

        info!(
            "Completed fetching from endpoint '{}': {} pages, {} elements",
            endpoint.url, page_num, total_sent
        );

        Ok(total_sent)
    }

    /// Fetch a URL with retry logic.
    async fn fetch_with_retry(
        &self,
        url: &str,
        endpoint: &EndpointConfig,
        auth: &Option<ResolvedAuth>,
        query_params: &[(String, String)],
    ) -> Result<(String, reqwest::header::HeaderMap)> {
        let max_retries = self.config.max_retries;
        let retry_delay = Duration::from_millis(self.config.retry_delay_ms);

        let mut last_error = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay = retry_delay.saturating_mul(attempt);
                debug!("Retry attempt {attempt} after {delay:?} delay");
                tokio::time::sleep(delay).await;
            }

            match self.make_request(url, endpoint, auth, query_params).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    warn!(
                        "Request to endpoint failed (attempt {}/{}): {}",
                        attempt + 1,
                        max_retries + 1,
                        e
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Request failed with no error details")))
    }

    /// Make a single HTTP request.
    /// Returns raw response text and headers for proper content-type handling.
    async fn make_request(
        &self,
        url: &str,
        endpoint: &EndpointConfig,
        auth: &Option<ResolvedAuth>,
        query_params: &[(String, String)],
    ) -> Result<(String, reqwest::header::HeaderMap)> {
        // Build request
        let mut builder = match endpoint.method {
            HttpMethod::Get => self.client.get(url),
            HttpMethod::Post => self.client.post(url),
            HttpMethod::Put => self.client.put(url),
        };

        // Add headers
        for (key, value) in &endpoint.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }

        // Add query parameters
        if !query_params.is_empty() {
            builder = builder.query(query_params);
        }

        // Add request body
        if let Some(ref body) = endpoint.body {
            builder = builder.json(body);
        }

        // Apply auth
        if let Some(ref resolved_auth) = auth {
            builder = auth::apply_auth(builder, resolved_auth).await?;
        }

        // Send request
        let response = builder.send().await.context("HTTP request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read error response".to_string());
            return Err(anyhow::anyhow!(
                "HTTP request returned error status {status}: {body}"
            ));
        }

        let headers = response.headers().clone();
        let body_text = response
            .text()
            .await
            .context("Failed to read response body")?;

        Ok((body_text, headers))
    }
}

/// Check if an element should be included based on the bootstrap request's label filters.
fn should_include_element(
    element: &drasi_core::models::Element,
    request: &BootstrapRequest,
) -> bool {
    match element {
        drasi_core::models::Element::Node { metadata, .. } => {
            if request.node_labels.is_empty() {
                return true;
            }
            metadata
                .labels
                .iter()
                .any(|l| request.node_labels.contains(&l.to_string()))
        }
        drasi_core::models::Element::Relation { metadata, .. } => {
            if request.relation_labels.is_empty() {
                return true;
            }
            metadata
                .labels
                .iter()
                .any(|l| request.relation_labels.contains(&l.to_string()))
        }
    }
}

#[async_trait]
impl BootstrapProvider for HttpBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: drasi_lib::channels::BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        info!(
            "Starting HTTP bootstrap for query {} from source {}",
            request.query_id, context.source_id
        );

        let mut total_events: u64 = 0;

        for (i, endpoint) in self.config.endpoints.iter().enumerate() {
            let auth = &self.resolved_auths[i];

            match self
                .fetch_endpoint(endpoint, auth, context, &request, &event_tx)
                .await
            {
                Ok(count) => {
                    total_events += count;
                }
                Err(e) => {
                    error!(
                        "Failed to bootstrap from endpoint '{}': {}",
                        endpoint.url, e
                    );
                    return Err(e);
                }
            }
        }

        info!(
            "Completed HTTP bootstrap for query {}: {} total elements",
            request.query_id, total_events
        );

        Ok(BootstrapResult {
            event_count: total_events as usize,
            last_sequence: None,
            sequences_aligned: false,
        })
    }
}
