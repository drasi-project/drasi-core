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

mod api;
mod config;
pub mod descriptor;
mod mapping;

pub use config::{CategoryConfig, CloudflareRadarBootstrapConfig};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use log::{info, warn};
use mapping::{
    map_attack_l3_summary, map_attack_l7_summary, map_dns_summary, map_domain_ranking, map_hijack,
    map_http_summary, map_leak, map_outage, normalize_id,
};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use tokio::time::{sleep, Duration};

use crate::api::{
    AttackLayer3SummaryResult, AttackSummaryResult, BgpHijackResult, BgpLeakResult,
    CloudflareResponse, DnsTopLocationsResult, HttpSummaryResult, OutageResult, RankingResult,
};

pub struct CloudflareRadarBootstrapProvider {
    config: CloudflareRadarBootstrapConfig,
    client: Client,
}

impl CloudflareRadarBootstrapProvider {
    pub fn builder() -> CloudflareRadarBootstrapProviderBuilder {
        CloudflareRadarBootstrapProviderBuilder::new()
    }
}

pub struct CloudflareRadarBootstrapProviderBuilder {
    config: CloudflareRadarBootstrapConfig,
}

impl CloudflareRadarBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            config: CloudflareRadarBootstrapConfig::default(),
        }
    }

    pub fn with_api_token(mut self, token: impl Into<String>) -> Self {
        self.config.api_token = token.into();
        self
    }

    pub fn with_api_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.config.api_base_url = base_url.into();
        self
    }

    pub fn with_category(mut self, name: &str, enabled: bool) -> Self {
        match name {
            "outages" => self.config.categories.outages = enabled,
            "bgp_hijacks" => self.config.categories.bgp_hijacks = enabled,
            "bgp_leaks" => self.config.categories.bgp_leaks = enabled,
            "http_traffic" => self.config.categories.http_traffic = enabled,
            "attacks_l7" => self.config.categories.attacks_l7 = enabled,
            "attacks_l3" => self.config.categories.attacks_l3 = enabled,
            "domain_rankings" => self.config.categories.domain_rankings = enabled,
            "dns" => self.config.categories.dns = enabled,
            _ => {
                log::warn!("Unknown Cloudflare Radar category: '{}'", name);
            }
        }
        self
    }

    pub fn with_location_filter(mut self, locations: Vec<String>) -> Self {
        self.config.location_filter = Some(locations);
        self
    }

    pub fn with_bgp_asn_filter(mut self, asns: Vec<u32>) -> Self {
        self.config.asn_filter = Some(asns);
        self
    }

    pub fn with_hijack_min_confidence(mut self, min_confidence: u32) -> Self {
        self.config.hijack_min_confidence = Some(min_confidence);
        self
    }

    pub fn with_ranking_limit(mut self, limit: u32) -> Self {
        self.config.ranking_limit = limit;
        self
    }

    pub fn with_dns_domains(mut self, domains: Vec<String>) -> Self {
        self.config.dns_domains = Some(domains);
        self
    }

    pub fn with_analytics_date_range(mut self, range: impl Into<String>) -> Self {
        self.config.analytics_date_range = range.into();
        self
    }

    pub fn with_event_date_range(mut self, range: impl Into<String>) -> Self {
        self.config.event_date_range = range.into();
        self
    }

    pub fn build(self) -> Result<CloudflareRadarBootstrapProvider> {
        if self.config.api_token.trim().is_empty() {
            return Err(anyhow!("Cloudflare Radar API token is required"));
        }
        if self.config.categories.dns
            && self
                .config
                .dns_domains
                .as_ref()
                .map(|d| d.is_empty())
                .unwrap_or(true)
        {
            return Err(anyhow!("DNS category enabled but no dns_domains provided"));
        }
        if !self.config.categories.outages
            && !self.config.categories.bgp_hijacks
            && !self.config.categories.bgp_leaks
            && !self.config.categories.http_traffic
            && !self.config.categories.attacks_l7
            && !self.config.categories.attacks_l3
            && !self.config.categories.domain_rankings
            && !self.config.categories.dns
        {
            return Err(anyhow!("At least one category must be enabled"));
        }

        let client = build_client(&self.config.api_token)?;
        Ok(CloudflareRadarBootstrapProvider {
            config: self.config,
            client,
        })
    }
}

#[async_trait]
impl BootstrapProvider for CloudflareRadarBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        info!(
            "[{}] Cloudflare Radar bootstrap started for query {}",
            context.source_id, request.query_id
        );

        let mut sent_ids: HashSet<String> = HashSet::new();
        let mut total_events = 0usize;

        if self.config.categories.outages
            && query_requests_any(
                &request,
                &["Outage", "Location", "AutonomousSystem"],
                &["AFFECTS_LOCATION", "AFFECTS_ASN"],
            )
        {
            let url = build_url(
                &self.config.api_base_url,
                "/radar/annotations/outages",
                &[
                    ("limit", "50".to_string()),
                    ("dateRange", self.config.event_date_range.clone()),
                    ("format", "json".to_string()),
                ],
            );
            let result: OutageResult = fetch_cloudflare(&self.client, &url).await?;
            for outage in result.annotations.iter() {
                if !outage_matches_filters(
                    outage,
                    self.config.location_filter.as_ref(),
                    self.config.asn_filter.as_ref(),
                ) {
                    continue;
                }
                let outage_id = compute_outage_id(outage);
                let changes = map_outage(&context.source_id, &outage_id, outage, true);
                total_events += send_changes(
                    &context.source_id,
                    &event_tx,
                    &context.sequence_counter,
                    changes,
                    &mut sent_ids,
                )
                .await?;
            }
        }

        if self.config.categories.bgp_hijacks
            && query_requests_any(
                &request,
                &["BgpHijack", "AutonomousSystem", "Prefix"],
                &["HIJACKED_BY", "VICTIM_ASN", "TARGETS_PREFIX"],
            )
        {
            let asns = self.config.asn_filter.clone().unwrap_or_default();
            let mut targets = vec![];
            if asns.is_empty() {
                targets.push(None);
            } else {
                for asn in asns {
                    targets.push(Some(asn));
                }
            }

            for target_asn in targets {
                let mut params = vec![
                    ("per_page", "50".to_string()),
                    ("format", "json".to_string()),
                ];
                if let Some(asn) = target_asn {
                    params.push(("involvedAsn", asn.to_string()));
                }
                if let Some(min_conf) = self.config.hijack_min_confidence {
                    params.push(("minConfidence", min_conf.to_string()));
                }
                let url = build_url(
                    &self.config.api_base_url,
                    "/radar/bgp/hijacks/events",
                    &params,
                );
                let result: BgpHijackResult = fetch_cloudflare(&self.client, &url).await?;
                for event in result.events.iter() {
                    let changes = map_hijack(&context.source_id, event, true);
                    total_events += send_changes(
                        &context.source_id,
                        &event_tx,
                        &context.sequence_counter,
                        changes,
                        &mut sent_ids,
                    )
                    .await?;
                }
            }
        }

        if self.config.categories.bgp_leaks
            && query_requests_any(&request, &["BgpLeak", "AutonomousSystem"], &["LEAKED_BY"])
        {
            let asns = self.config.asn_filter.clone().unwrap_or_default();
            let mut targets = vec![];
            if asns.is_empty() {
                targets.push(None);
            } else {
                for asn in asns {
                    targets.push(Some(asn));
                }
            }

            for target_asn in targets {
                let mut params = vec![
                    ("per_page", "50".to_string()),
                    ("format", "json".to_string()),
                ];
                if let Some(asn) = target_asn {
                    params.push(("involvedAsn", asn.to_string()));
                }
                let url = build_url(
                    &self.config.api_base_url,
                    "/radar/bgp/leaks/events",
                    &params,
                );
                let result: BgpLeakResult = fetch_cloudflare(&self.client, &url).await?;
                for event in result.events.iter() {
                    let changes = map_leak(&context.source_id, event, true);
                    total_events += send_changes(
                        &context.source_id,
                        &event_tx,
                        &context.sequence_counter,
                        changes,
                        &mut sent_ids,
                    )
                    .await?;
                }
            }
        }

        if self.config.categories.http_traffic
            && query_requests_any(&request, &["HttpTraffic"], &[])
        {
            let params = vec![
                ("dateRange", self.config.analytics_date_range.clone()),
                ("format", "json".to_string()),
            ];
            let url = build_url(
                &self.config.api_base_url,
                "/radar/http/summary/device_type",
                &params,
            );
            let result: HttpSummaryResult = fetch_cloudflare(&self.client, &url).await?;
            let effective_from = Utc::now().timestamp_millis() as u64;
            for (series, summary) in result.summaries.iter() {
                let node_id = format!("http-traffic-{}", normalize_id(series));
                let change = map_http_summary(
                    &context.source_id,
                    &node_id,
                    series,
                    summary,
                    effective_from,
                );
                total_events += send_changes(
                    &context.source_id,
                    &event_tx,
                    &context.sequence_counter,
                    vec![change],
                    &mut sent_ids,
                )
                .await?;
            }
        }

        if self.config.categories.attacks_l7 && query_requests_any(&request, &["AttackL7"], &[]) {
            let params = vec![
                ("dateRange", self.config.analytics_date_range.clone()),
                ("format", "json".to_string()),
            ];
            let url = build_url(
                &self.config.api_base_url,
                "/radar/attacks/layer7/summary",
                &params,
            );
            let result: AttackSummaryResult = fetch_cloudflare(&self.client, &url).await?;
            let effective_from = Utc::now().timestamp_millis() as u64;
            for (series, summary) in result.summaries.iter() {
                let node_id = format!("attack-l7-{}", normalize_id(series));
                let change = map_attack_l7_summary(
                    &context.source_id,
                    &node_id,
                    series,
                    summary,
                    effective_from,
                );
                total_events += send_changes(
                    &context.source_id,
                    &event_tx,
                    &context.sequence_counter,
                    vec![change],
                    &mut sent_ids,
                )
                .await?;
            }
        }

        if self.config.categories.attacks_l3 && query_requests_any(&request, &["AttackL3"], &[]) {
            let params = vec![
                ("dateRange", self.config.analytics_date_range.clone()),
                ("format", "json".to_string()),
            ];
            let url = build_url(
                &self.config.api_base_url,
                "/radar/attacks/layer3/summary",
                &params,
            );
            let result: AttackLayer3SummaryResult = fetch_cloudflare(&self.client, &url).await?;
            let effective_from = Utc::now().timestamp_millis() as u64;
            for (series, summary) in result.summaries.iter() {
                let node_id = format!("attack-l3-{}", normalize_id(series));
                let change = map_attack_l3_summary(
                    &context.source_id,
                    &node_id,
                    series,
                    summary,
                    effective_from,
                );
                total_events += send_changes(
                    &context.source_id,
                    &event_tx,
                    &context.sequence_counter,
                    vec![change],
                    &mut sent_ids,
                )
                .await?;
            }
        }

        if self.config.categories.domain_rankings
            && query_requests_any(&request, &["Domain", "DomainRanking"], &["RANKED_AS"])
        {
            let params = vec![("limit", self.config.ranking_limit.to_string())];
            let url = build_url(&self.config.api_base_url, "/radar/ranking/top", &params);
            let result: RankingResult = fetch_cloudflare(&self.client, &url).await?;
            let effective_from = Utc::now().timestamp_millis() as u64;
            for ranks in result.rankings.values() {
                for domain in ranks {
                    let changes = map_domain_ranking(&context.source_id, domain, effective_from);
                    total_events += send_changes(
                        &context.source_id,
                        &event_tx,
                        &context.sequence_counter,
                        changes,
                        &mut sent_ids,
                    )
                    .await?;
                }
            }
        }

        if self.config.categories.dns && query_requests_any(&request, &["DnsQuerySummary"], &[]) {
            let domains = self.config.dns_domains.clone().unwrap_or_default();
            let effective_from = Utc::now().timestamp_millis() as u64;
            for domain in domains {
                let params = vec![
                    ("domain", domain.clone()),
                    ("dateRange", self.config.analytics_date_range.clone()),
                    ("format", "json".to_string()),
                    ("limit", "10".to_string()),
                ];
                let url = build_url(
                    &self.config.api_base_url,
                    "/radar/dns/top/locations",
                    &params,
                );
                let result: DnsTopLocationsResult = fetch_cloudflare(&self.client, &url).await?;
                if let Some(locations) = result.top.values().next() {
                    let change =
                        map_dns_summary(&context.source_id, &domain, locations, effective_from);
                    total_events += send_changes(
                        &context.source_id,
                        &event_tx,
                        &context.sequence_counter,
                        vec![change],
                        &mut sent_ids,
                    )
                    .await?;
                }
            }
        }

        info!(
            "[{}] Cloudflare Radar bootstrap complete: {} events",
            context.source_id, total_events
        );

        Ok(total_events)
    }
}

fn build_client(api_token: &str) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", api_token))?,
    );

    let client = Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    Ok(client)
}

async fn fetch_cloudflare<T: DeserializeOwned>(client: &Client, url: &str) -> Result<T> {
    let mut attempt = 0;
    let mut delay = Duration::from_millis(500);
    loop {
        attempt += 1;
        let response = match client.get(url).send().await {
            Ok(response) => response,
            Err(err) => {
                if attempt >= 4 {
                    return Err(err.into());
                }
                warn!(
                    "Cloudflare API request failed ({}); retrying in {:?}",
                    err, delay
                );
                sleep(delay).await;
                delay *= 2;
                continue;
            }
        };

        if response.status().is_success() {
            let payload: CloudflareResponse<T> = response.json().await?;
            if payload.success {
                return Ok(payload.result);
            }
            return Err(anyhow!("Cloudflare API returned success=false"));
        }

        if response.status().as_u16() == 429 || response.status().is_server_error() {
            if attempt >= 4 {
                return Err(anyhow!(
                    "Cloudflare API error after retries: {}",
                    response.status()
                ));
            }
            warn!(
                "Cloudflare API rate limited or server error ({}); retrying in {:?}",
                response.status(),
                delay
            );
            sleep(delay).await;
            delay *= 2;
            continue;
        }
        return Err(anyhow!(
            "Cloudflare API request failed: {}",
            response.status()
        ));
    }
}

fn build_url(base: &str, path: &str, params: &[(&str, String)]) -> String {
    if params.is_empty() {
        format!("{base}{path}")
    } else {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in params {
            serializer.append_pair(key, value);
        }
        let query = serializer.finish();
        format!("{base}{path}?{query}")
    }
}

fn query_requests_any(
    request: &BootstrapRequest,
    node_labels: &[&str],
    relation_labels: &[&str],
) -> bool {
    if request.node_labels.is_empty() && request.relation_labels.is_empty() {
        return true;
    }
    node_labels
        .iter()
        .any(|label| request.node_labels.contains(&label.to_string()))
        || relation_labels
            .iter()
            .any(|label| request.relation_labels.contains(&label.to_string()))
}

async fn send_changes(
    source_id: &str,
    event_tx: &BootstrapEventSender,
    sequence_counter: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    changes: Vec<drasi_core::models::SourceChange>,
    sent_ids: &mut HashSet<String>,
) -> Result<usize> {
    let mut count = 0usize;
    for change in changes {
        let reference = change.get_reference();
        let key = format!("{}:{}", source_id, reference.element_id);
        if !sent_ids.insert(key) {
            continue;
        }
        let sequence = sequence_counter.fetch_add(1, Ordering::SeqCst);
        let event = BootstrapEvent {
            source_id: source_id.to_string(),
            change,
            timestamp: Utc::now(),
            sequence,
        };
        event_tx
            .send(event)
            .await
            .map_err(|err| anyhow!(err.to_string()))?;
        count += 1;
    }
    Ok(count)
}

fn compute_outage_id(outage: &crate::api::OutageAnnotation) -> String {
    let start = outage
        .start_date
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let scope = outage.scope.clone().unwrap_or_default();
    let mut locations = outage.locations.clone();
    locations.sort();
    let key = format!("{start}-{scope}-{}", locations.join(","));
    format!("outage-{}", normalize_id(&key))
}

fn outage_matches_filters(
    outage: &crate::api::OutageAnnotation,
    location_filter: Option<&Vec<String>>,
    asn_filter: Option<&Vec<u32>>,
) -> bool {
    if let Some(locations) = location_filter {
        if !locations.is_empty()
            && !outage
                .locations
                .iter()
                .any(|location| locations.contains(location))
        {
            return false;
        }
    }

    if let Some(asns) = asn_filter {
        if !asns.is_empty() && !outage.asns.iter().any(|asn| asns.contains(asn)) {
            return false;
        }
    }

    true
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "cloudflare-radar-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::CloudflareRadarBootstrapDescriptor],
);
