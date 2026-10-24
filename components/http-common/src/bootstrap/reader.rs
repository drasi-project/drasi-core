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

//! Shared REST snapshot reader. Callers own event admission and cancellation.

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use reqwest::Client;
use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use drasi_core_types::models::SourceChange;

use super::auth::{self, ResolvedAuth};
use super::config::{EndpointConfig, HttpBootstrapConfig, HttpMethod, OperationType};
use super::content_parser::{self, ContentType};
use super::pagination::{self, NextPage, Paginator};
use super::response;
use super::template_engine::TemplateEngine;

/// Default maximum number of pages to fetch per endpoint to prevent infinite loops.
const DEFAULT_MAX_PAGES: u64 = 10_000;

/// Maximum backoff delay (60 seconds) to prevent unbounded sleep.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

/// Safely truncate a string to at most `max_chars` characters without panicking
/// on multi-byte UTF-8 boundaries.
fn safe_truncate(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        return s;
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A resolved endpoint bundling config with its resolved auth.
struct ResolvedEndpoint {
    config: EndpointConfig,
    auth: Option<ResolvedAuth>,
}

pub struct SnapshotRequest<'a> {
    pub source_id: &'a str,
    pub node_labels: &'a [String],
    pub relation_labels: &'a [String],
}

/// Fetches and maps the same REST protocol for both bootstrap execution models.
pub struct HttpSnapshotReader {
    config: HttpBootstrapConfig,
    client: Client,
    endpoints: Vec<ResolvedEndpoint>,
    engine: TemplateEngine,
    response_limit: Option<usize>,
}

impl HttpSnapshotReader {
    pub fn validate_bounded_config(config: &HttpBootstrapConfig) -> Result<()> {
        config.validate()?;
        for endpoint in &config.endpoints {
            let url = reqwest::Url::parse(&endpoint.url)?;
            anyhow::ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none(),
                "HTTP bootstrap endpoint must not contain credentials or a fragment"
            );
            for (name, value) in &endpoint.headers {
                let _: reqwest::header::HeaderName = name.parse()?;
                let _: reqwest::header::HeaderValue = value.parse()?;
            }
        }
        Ok(())
    }
    /// Create a new HTTP bootstrap provider from configuration.
    pub fn new(config: HttpBootstrapConfig) -> Result<Self> {
        Self::build(config, None)
    }

    /// Native snapshots fail closed on mapping errors, pagination cycles and
    /// page-limit exhaustion rather than reporting a partial snapshot as complete.
    pub fn bounded(config: HttpBootstrapConfig, max_response_bytes: usize) -> Result<Self> {
        anyhow::ensure!(max_response_bytes > 0, "response limit must be positive");
        Self::validate_bounded_config(&config)?;
        Self::build(config, Some(max_response_bytes))
    }

    fn build(config: HttpBootstrapConfig, response_limit: Option<usize>) -> Result<Self> {
        let timeout = Duration::from_secs(config.timeout_seconds);
        let mut builder = Client::builder()
            .timeout(timeout)
            // Force HTTP/1.1 to avoid HTTP/2 multiplexing issues when multiple
            // bootstrap calls share this client concurrently. H2 frame reassembly
            // under heavy multiplexing can corrupt large response bodies.
            .http1_only();
        if response_limit.is_some() {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }
        let client = builder.build().context("Failed to build HTTP client")?;

        // Resolve authentication for each endpoint and bundle them together
        let mut endpoints = Vec::new();
        for endpoint in &config.endpoints {
            let auth = match &endpoint.auth {
                Some(auth_config) => Some(
                    (if response_limit.is_some() {
                        auth::resolve_auth_bounded(auth_config, &client, 65_536)
                    } else {
                        auth::resolve_auth(auth_config, &client)
                    })
                    .context("Failed to resolve authentication")?,
                ),
                None => None,
            };
            endpoints.push(ResolvedEndpoint {
                config: endpoint.clone(),
                auth,
            });
        }

        let engine = TemplateEngine::new();

        Ok(Self {
            config,
            client,
            endpoints,
            engine,
            response_limit,
        })
    }

    /// Fetch all pages from a single endpoint and emit bootstrap events.
    async fn fetch_endpoint<F, Fut>(
        &self,
        endpoint: &EndpointConfig,
        auth: &Option<ResolvedAuth>,
        request: &SnapshotRequest<'_>,
        emit: &mut F,
    ) -> Result<u64>
    where
        F: FnMut(SourceChange) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let mut total_sent: u64 = 0;

        // Determine content type override
        let content_type_override = endpoint
            .response
            .content_type
            .as_ref()
            .map(ContentType::from_override);

        // Set up pagination with SSRF protection (origin host validation)
        let origin_host = pagination::extract_origin_host(&endpoint.url).unwrap_or_default();
        let mut paginator: Option<Box<dyn Paginator>> = endpoint
            .pagination
            .as_ref()
            .map(|p| pagination::create_paginator(p, origin_host));

        // Get initial pagination params
        let initial_params: Vec<(String, String)> = paginator
            .as_ref()
            .map(|p| p.initial_params())
            .unwrap_or_default();

        let mut current_url = endpoint.url.clone();
        let mut current_params = initial_params;
        let mut page_num = 0u64;
        let mut seen_requests: HashSet<(String, Vec<(String, String)>)> = HashSet::new();

        loop {
            page_num += 1;

            // Prevent infinite loops
            let max_pages = self.config.max_pages.unwrap_or(DEFAULT_MAX_PAGES);
            if page_num > max_pages {
                anyhow::ensure!(
                    self.response_limit.is_none(),
                    "HTTP bootstrap exceeded maxPages ({max_pages}) before completion"
                );
                error!(
                    "Reached maximum page limit ({max_pages}) for endpoint '{}', stopping pagination. \
                     Configure 'maxPages' to increase this limit.",
                    endpoint.url
                );
                break;
            }

            // Detect cycles: same URL + same params seen before
            let current_key = (current_url.clone(), current_params.clone());
            if !seen_requests.insert(current_key) {
                anyhow::ensure!(
                    self.response_limit.is_none(),
                    "HTTP bootstrap pagination cycle detected"
                );
                warn!(
                    "Pagination cycle detected for endpoint '{}', stopping",
                    endpoint.url
                );
                break;
            }

            debug!("Fetching page {page_num} from endpoint: {}", endpoint.url);

            // Fetch and parse with retry: both transport errors AND body
            // parsing failures (truncation) trigger retries.
            let (parsed_body, response_headers) = self
                .fetch_and_parse_with_retry(
                    &current_url,
                    endpoint,
                    auth,
                    &current_params,
                    &content_type_override,
                )
                .await
                .with_context(|| {
                    format!(
                        "Failed to fetch from endpoint '{}' (page {page_num})",
                        endpoint.url
                    )
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
            let element_results = response::mapped_items(
                &items,
                &endpoint.response.mappings,
                request.source_id,
                &self.engine,
            );

            for (result, mapping) in element_results.zip(endpoint.response.mappings.iter().cycle())
            {
                match result {
                    Ok(mapped) => {
                        // Filter by requested labels
                        if !should_include_change(&mapped, request, &mapping.element_type) {
                            continue;
                        }

                        let source_change = match mapped {
                            response::MappedChange::Upsert { element, operation } => {
                                match operation {
                                    OperationType::Insert => SourceChange::Insert { element },
                                    OperationType::Update => SourceChange::Update { element },
                                    OperationType::Delete => unreachable!(),
                                }
                            }
                            response::MappedChange::Delete { metadata } => {
                                SourceChange::Delete { metadata }
                            }
                        };
                        emit(source_change)
                            .await
                            .context("Failed to send bootstrap event")?;

                        total_sent += 1;
                    }
                    Err(e) => {
                        if self.response_limit.is_some() {
                            return Err(e.context("Failed to map HTTP snapshot item"));
                        }
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
                            if self.response_limit.is_some() {
                                let next = reqwest::Url::parse(&url)?;
                                let origin = reqwest::Url::parse(&endpoint.url)?;
                                anyhow::ensure!(
                                    next.origin() == origin.origin()
                                        && next.username().is_empty()
                                        && next.password().is_none()
                                        && next.fragment().is_none(),
                                    "HTTP bootstrap pagination must remain on the endpoint origin"
                                );
                            }
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

    /// Fetch a URL with retry logic, including body parsing validation.
    /// Both transport errors AND body parsing failures (e.g., truncated responses)
    /// are treated as retriable errors. This handles the case where an intermediary
    /// (proxy, load balancer, CDN) returns a properly-terminated HTTP response with
    /// incomplete body content.
    async fn fetch_and_parse_with_retry(
        &self,
        url: &str,
        endpoint: &EndpointConfig,
        auth: &Option<ResolvedAuth>,
        query_params: &[(String, String)],
        content_type_override: &Option<ContentType>,
    ) -> Result<(serde_json::Value, reqwest::header::HeaderMap)> {
        let max_retries = self.config.max_retries;
        let retry_delay = Duration::from_millis(self.config.retry_delay_ms);

        let mut last_error = None;

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let factor = 1u64.checked_shl(attempt - 1).unwrap_or(u64::MAX);
                let delay = retry_delay
                    .saturating_mul(factor.min(u32::MAX as u64) as u32)
                    .min(MAX_RETRY_DELAY);
                debug!("Retry attempt {attempt} after {delay:?} delay");
                tokio::time::sleep(delay).await;
            }

            // Fetch
            let (response_text, response_headers) =
                match self.make_request(url, endpoint, auth, query_params).await {
                    Ok(result) => result,
                    Err(e) => {
                        warn!(
                            "Request to endpoint failed (attempt {}/{}): {}",
                            attempt + 1,
                            max_retries + 1,
                            e
                        );
                        last_error = Some(e);
                        continue;
                    }
                };

            // Determine content type
            let ct = resolve_content_type(content_type_override, &response_headers);

            // Validate: parse response body
            match content_parser::parse_body(&response_text, &ct) {
                Ok(parsed) => return Ok((parsed, response_headers)),
                Err(e) => {
                    // Body parsing failed — likely truncated response.
                    // Treat as retriable: the next attempt may get a complete response.
                    warn!(
                        "Response body parse failed (attempt {}/{}, possible truncation). \
                         Body length: {}, content-type: {ct:?}, error: {e}",
                        attempt + 1,
                        max_retries + 1,
                        response_text.len(),
                    );
                    last_error = Some(e.context(format!(
                        "Failed to parse response from '{}' as {ct:?} \
                         (body length: {}, possible truncation)",
                        url,
                        response_text.len()
                    )));
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
        let mut response = builder.send().await.context("HTTP request failed")?;

        if let Some(limit) = self.response_limit {
            let status = response.status();
            let headers = response.headers().clone();
            if let Some(length) = response.content_length() {
                anyhow::ensure!(
                    length <= limit as u64,
                    "HTTP snapshot response exceeds byte limit"
                );
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .context("Failed to read response body")?
            {
                anyhow::ensure!(
                    chunk.len() <= limit.saturating_sub(body.len()),
                    "HTTP snapshot response exceeds byte limit"
                );
                body.extend_from_slice(&chunk);
            }
            anyhow::ensure!(
                status.is_success(),
                "HTTP snapshot request returned {status}"
            );
            return Ok((
                String::from_utf8(body).context("HTTP snapshot body is not UTF-8")?,
                headers,
            ));
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read error response".to_string());
            let truncated = if body.len() > 256 {
                format!("{}... (truncated)", safe_truncate(&body, 256))
            } else {
                body
            };
            return Err(anyhow::anyhow!(
                "HTTP request returned error status {status}: {truncated}"
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

/// Resolve the content type from the override or response headers.
fn resolve_content_type(
    override_ct: &Option<ContentType>,
    headers: &reqwest::header::HeaderMap,
) -> ContentType {
    override_ct.clone().unwrap_or_else(|| {
        let header_value = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        ContentType::from_header(header_value)
    })
}

/// Check if an element should be included based on the bootstrap request's label filters.
fn should_include_change(
    change: &response::MappedChange,
    request: &SnapshotRequest<'_>,
    element_type: &super::config::ElementType,
) -> bool {
    let metadata = match change {
        response::MappedChange::Upsert { element, .. } => element.get_metadata(),
        response::MappedChange::Delete { metadata } => metadata,
    };

    // Deletes carry only metadata, so their node/relation kind comes from the
    // selected mapping rather than from a missing element body.
    let is_relation = matches!(element_type, super::config::ElementType::Relation);

    if is_relation {
        if request.relation_labels.is_empty() {
            return true;
        }
        metadata
            .labels
            .iter()
            .any(|l| request.relation_labels.iter().any(|rl| rl.as_str() == &**l))
    } else {
        if request.node_labels.is_empty() {
            return true;
        }
        metadata
            .labels
            .iter()
            .any(|l| request.node_labels.iter().any(|nl| nl.as_str() == &**l))
    }
}

impl HttpSnapshotReader {
    pub async fn read<F, Fut>(&self, request: SnapshotRequest<'_>, mut emit: F) -> Result<u64>
    where
        F: FnMut(SourceChange) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let mut total_events: u64 = 0;

        for resolved in &self.endpoints {
            match self
                .fetch_endpoint(&resolved.config, &resolved.auth, &request, &mut emit)
                .await
            {
                Ok(count) => {
                    total_events += count;
                }
                Err(e) => {
                    error!(
                        "Failed to bootstrap from endpoint '{}': {}",
                        resolved.config.url, e
                    );
                    return Err(e);
                }
            }
        }

        Ok(total_events)
    }
}
