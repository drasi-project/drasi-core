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
mod state;

pub use config::{CategoryConfig, CloudflareRadarConfig, StartBehavior};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use drasi_core::models::SourceChange;
use drasi_lib::channels::{ComponentStatus, SourceEvent, SourceEventWrapper};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::sources::Source;
use drasi_lib::state_store::StateStoreProvider;
use log::{debug, error, info, warn};
use mapping::{
    delete_relations, map_attack_l3_summary, map_attack_l7_summary, map_dns_summary,
    map_domain_ranking, map_hijack, map_http_summary, map_leak, map_outage, normalize_id,
    relation_element_ids_for_hijack, relation_element_ids_for_leak,
    relation_element_ids_for_outage, ChangeAction,
};
use reqwest::Client;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use tokio::time::{sleep, Duration};

use crate::api::{
    AttackLayer3SummaryResult, AttackSummaryResult, BgpHijackResult, BgpLeakResult,
    CloudflareResponse, DnsTopLocationsResult, DomainRank, HttpSummaryResult, OutageAnnotation,
    OutageResult, RankingResult,
};
use crate::config::StartBehavior::{StartFromBeginning, StartFromTimestamp};
use crate::state::PollingState;

/// Cloudflare Radar source implementation
pub struct CloudflareRadarSource {
    base: SourceBase,
    config: CloudflareRadarConfig,
    client: Client,
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
}

impl CloudflareRadarSource {
    pub fn new(
        config: CloudflareRadarConfig,
        params: SourceBaseParams,
        state_store: Option<Arc<dyn StateStoreProvider>>,
    ) -> Result<Self> {
        let client = build_client(&config.api_token)?;
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            client,
            state_store: Arc::new(RwLock::new(state_store)),
        })
    }

    /// Create a builder for configuring the Cloudflare Radar source.
    pub fn builder(id: impl Into<String>) -> CloudflareRadarSourceBuilder {
        CloudflareRadarSourceBuilder::new(id)
    }
}

#[async_trait]
impl Source for CloudflareRadarSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "cloudflare-radar"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "poll_interval_secs".to_string(),
            serde_json::Value::Number(self.config.poll_interval_secs.into()),
        );
        props.insert(
            "api_base_url".to_string(),
            serde_json::Value::String(self.config.api_base_url.clone()),
        );
        props.insert(
            "categories".to_string(),
            serde_json::to_value(&self.config.categories).unwrap_or_default(),
        );
        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        info!("[{}] Starting Cloudflare Radar source", self.base.id);

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting Cloudflare Radar source".to_string()),
            )
            .await;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        *self.base.shutdown_tx.write().await = Some(shutdown_tx);

        let source_id = self.base.id.clone();
        let config = self.config.clone();
        let client = self.client.clone();
        let dispatchers = self.base.dispatchers.clone();
        let state_store = self.state_store.read().await.clone();

        let task = tokio::spawn(async move {
            if let Err(err) = polling_loop(
                &source_id,
                config,
                client,
                dispatchers,
                state_store,
                shutdown_rx,
            )
            .await
            {
                error!("[{source_id}] Polling loop exited with error: {err}");
            }
        });

        *self.base.task_handle.write().await = Some(task);

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Cloudflare Radar source running".to_string()),
            )
            .await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping Cloudflare Radar source", self.base.id);

        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping Cloudflare Radar source".to_string()),
            )
            .await;

        if let Some(tx) = self.base.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }

        if let Some(mut handle) = self.base.task_handle.write().await.take() {
            tokio::select! {
                _ = &mut handle => {},
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    warn!("[{}] Polling task did not stop within 5s, aborting", self.base.id);
                    handle.abort();
                }
            }
        }

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Cloudflare Radar source stopped".to_string()),
            )
            .await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<drasi_lib::channels::SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "Cloudflare Radar")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context.clone()).await;
        if let Some(store) = context.state_store {
            *self.state_store.write().await = Some(store);
            debug!(
                "State store injected into Cloudflare Radar source '{}'",
                self.base.id
            );
        }
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// Builder for Cloudflare Radar source
pub struct CloudflareRadarSourceBuilder {
    id: String,
    config: CloudflareRadarConfig,
    dispatch_mode: Option<drasi_lib::channels::DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
    state_store: Option<Arc<dyn StateStoreProvider>>,
}

impl CloudflareRadarSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: CloudflareRadarConfig::default(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
            state_store: None,
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

    pub fn with_poll_interval_secs(mut self, poll_interval_secs: u64) -> Self {
        self.config.poll_interval_secs = poll_interval_secs;
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
                log::warn!("Unknown Cloudflare Radar category: '{name}'");
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

    pub fn with_start_behavior(mut self, behavior: StartBehavior) -> Self {
        self.config.start_behavior = behavior;
        self
    }

    pub fn with_dispatch_mode(mut self, mode: drasi_lib::channels::DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_state_store(mut self, state_store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(state_store);
        self
    }

    pub fn build(self) -> Result<CloudflareRadarSource> {
        if self.config.api_token.trim().is_empty() {
            return Err(anyhow!("Cloudflare Radar API token is required"));
        }
        if self.config.poll_interval_secs == 0 {
            return Err(anyhow!("poll_interval_secs must be greater than 0"));
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

        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);

        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        CloudflareRadarSource::new(self.config, params, self.state_store)
    }
}

fn build_client(api_token: &str) -> Result<Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&format!("Bearer {api_token}"))?,
    );

    let client = Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(30))
        .build()?;
    Ok(client)
}

async fn polling_loop(
    source_id: &str,
    config: CloudflareRadarConfig,
    client: Client,
    dispatchers: Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(config.poll_interval_secs));

    let mut state = load_state(source_id, state_store.as_ref())
        .await
        .unwrap_or_else(|err| {
            warn!("[{source_id}] Failed to load polling state: {err}");
            PollingState::new()
        });

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let emit_changes = state.initialized || matches!(config.start_behavior, StartFromBeginning | StartFromTimestamp(_));
                let min_timestamp = match config.start_behavior {
                    StartFromTimestamp(ts) => Some(ts),
                    _ => None,
                };

                if config.categories.outages {
                    if let Err(err) = poll_outages(source_id, &config, &client, &dispatchers, &mut state, emit_changes, min_timestamp).await {
                        warn!("[{source_id}] Outages poll failed: {err}");
                    }
                }
                if config.categories.bgp_hijacks {
                    if let Err(err) = poll_bgp_hijacks(source_id, &config, &client, &dispatchers, &mut state, emit_changes, min_timestamp).await {
                        warn!("[{source_id}] BGP hijacks poll failed: {err}");
                    }
                }
                if config.categories.bgp_leaks {
                    if let Err(err) = poll_bgp_leaks(source_id, &config, &client, &dispatchers, &mut state, emit_changes, min_timestamp).await {
                        warn!("[{source_id}] BGP leaks poll failed: {err}");
                    }
                }
                if config.categories.http_traffic {
                    if let Err(err) = poll_http_summary(source_id, &config, &client, &dispatchers, &mut state, emit_changes).await {
                        warn!("[{source_id}] HTTP traffic poll failed: {err}");
                    }
                }
                if config.categories.attacks_l7 {
                    if let Err(err) = poll_attack_l7_summary(source_id, &config, &client, &dispatchers, &mut state, emit_changes).await {
                        warn!("[{source_id}] L7 attacks poll failed: {err}");
                    }
                }
                if config.categories.attacks_l3 {
                    if let Err(err) = poll_attack_l3_summary(source_id, &config, &client, &dispatchers, &mut state, emit_changes).await {
                        warn!("[{source_id}] L3 attacks poll failed: {err}");
                    }
                }
                if config.categories.domain_rankings {
                    if let Err(err) = poll_domain_rankings(source_id, &config, &client, &dispatchers, &mut state, emit_changes).await {
                        warn!("[{source_id}] Domain rankings poll failed: {err}");
                    }
                }
                if config.categories.dns {
                    if let Err(err) = poll_dns_top_locations(source_id, &config, &client, &dispatchers, &mut state, emit_changes).await {
                        warn!("[{source_id}] DNS poll failed: {err}");
                    }
                }

                if !state.initialized {
                    state.initialized = true;
                }

                if let Some(store) = state_store.as_ref() {
                    if let Err(err) = save_state(source_id, store, &state).await {
                        warn!("[{source_id}] Failed to persist polling state: {err}");
                    }
                }
            }
            _ = &mut shutdown_rx => {
                info!("[{source_id}] Shutdown signal received");
                break;
            }
        }
    }

    Ok(())
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
                warn!("Cloudflare API request failed ({err}); retrying in {delay:?}");
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

async fn poll_outages(
    source_id: &str,
    config: &CloudflareRadarConfig,
    client: &Client,
    dispatchers: &Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    state: &mut PollingState,
    emit_changes: bool,
    min_timestamp: Option<i64>,
) -> Result<()> {
    let url = build_url(
        &config.api_base_url,
        "/radar/annotations/outages",
        &[
            ("limit", "50".to_string()),
            ("dateRange", config.event_date_range.clone()),
            ("format", "json".to_string()),
        ],
    );

    let result: OutageResult = fetch_cloudflare(client, &url).await?;
    let mut next_map = HashMap::new();

    for outage in result.annotations.into_iter() {
        if !outage_matches_filters(
            &outage,
            config.location_filter.as_ref(),
            config.asn_filter.as_ref(),
        ) {
            continue;
        }
        let outage_id = compute_outage_id(&outage);
        next_map.insert(outage_id.clone(), outage.clone());

        let should_emit = emit_changes && is_after_min_timestamp(min_timestamp, &outage.start_date);
        if !state.seen_outage_ids.contains_key(&outage_id) {
            if should_emit {
                let changes =
                    map_outage(source_id, &outage_id, &outage, ChangeAction::Insert, true);
                dispatch_changes(source_id, dispatchers, changes).await?;
            }
        } else if state.seen_outage_ids.get(&outage_id) != Some(&outage) && should_emit {
            let changes = map_outage(source_id, &outage_id, &outage, ChangeAction::Update, false);
            dispatch_changes(source_id, dispatchers, changes).await?;

            // Delete old relationships and insert new ones
            if let Some(old_outage) = state.seen_outage_ids.get(&outage_id) {
                let old_rels = relation_element_ids_for_outage(&outage_id, old_outage);
                let del_changes = delete_relations(source_id, &old_rels);
                dispatch_changes(source_id, dispatchers, del_changes).await?;
            }
            let new_rels = map_outage(source_id, &outage_id, &outage, ChangeAction::Insert, true);
            // Skip the first element (the node itself) and dispatch only relations/shared nodes
            if new_rels.len() > 1 {
                dispatch_changes(source_id, dispatchers, new_rels[1..].to_vec()).await?;
            }
        }
    }

    if emit_changes {
        let removed: Vec<(String, OutageAnnotation)> = state
            .seen_outage_ids
            .iter()
            .filter(|(id, _)| !next_map.contains_key(*id))
            .map(|(id, outage)| (id.clone(), outage.clone()))
            .collect();

        for (id, outage) in removed {
            let rel_ids = relation_element_ids_for_outage(&id, &outage);
            let rel_changes = delete_relations(source_id, &rel_ids);
            dispatch_changes(source_id, dispatchers, rel_changes).await?;

            let changes = map_outage(source_id, &id, &outage, ChangeAction::Delete, false);
            dispatch_changes(source_id, dispatchers, changes).await?;
        }
    }

    state.seen_outage_ids = next_map;
    Ok(())
}

async fn poll_bgp_hijacks(
    source_id: &str,
    config: &CloudflareRadarConfig,
    client: &Client,
    dispatchers: &Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    state: &mut PollingState,
    emit_changes: bool,
    min_timestamp: Option<i64>,
) -> Result<()> {
    let mut new_map = HashMap::new();
    let asns = config.asn_filter.clone().unwrap_or_default();
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
        if let Some(min_conf) = config.hijack_min_confidence {
            params.push(("minConfidence", min_conf.to_string()));
        }

        let url = build_url(&config.api_base_url, "/radar/bgp/hijacks/events", &params);
        let result: BgpHijackResult = fetch_cloudflare(client, &url).await?;

        for event in result.events.into_iter() {
            let should_emit =
                emit_changes && is_after_min_timestamp(min_timestamp, &event.min_hijack_ts);
            new_map.insert(event.id, event.clone());

            if !state.seen_hijack_ids.contains_key(&event.id) {
                if should_emit {
                    let changes = map_hijack(source_id, &event, ChangeAction::Insert, true);
                    dispatch_changes(source_id, dispatchers, changes).await?;
                }
            } else if state.seen_hijack_ids.get(&event.id) != Some(&event) && should_emit {
                let changes = map_hijack(source_id, &event, ChangeAction::Update, false);
                dispatch_changes(source_id, dispatchers, changes).await?;

                // Delete old relationships and insert new ones
                if let Some(old_event) = state.seen_hijack_ids.get(&event.id) {
                    let old_rels = relation_element_ids_for_hijack(old_event);
                    let del_changes = delete_relations(source_id, &old_rels);
                    dispatch_changes(source_id, dispatchers, del_changes).await?;
                }
                let new_rels = map_hijack(source_id, &event, ChangeAction::Insert, true);
                if new_rels.len() > 1 {
                    dispatch_changes(source_id, dispatchers, new_rels[1..].to_vec()).await?;
                }
            }
        }
    }

    if emit_changes {
        let removed_ids: Vec<u64> = state
            .seen_hijack_ids
            .keys()
            .filter(|id| !new_map.contains_key(*id))
            .copied()
            .collect();

        for id in removed_ids {
            if let Some(old_event) = state.seen_hijack_ids.get(&id) {
                let rel_ids = relation_element_ids_for_hijack(old_event);
                let rel_changes = delete_relations(source_id, &rel_ids);
                dispatch_changes(source_id, dispatchers, rel_changes).await?;

                let changes = map_hijack(source_id, old_event, ChangeAction::Delete, false);
                dispatch_changes(source_id, dispatchers, changes).await?;
            }
        }
    }

    state.seen_hijack_ids = new_map;
    Ok(())
}

async fn poll_bgp_leaks(
    source_id: &str,
    config: &CloudflareRadarConfig,
    client: &Client,
    dispatchers: &Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    state: &mut PollingState,
    emit_changes: bool,
    min_timestamp: Option<i64>,
) -> Result<()> {
    let mut new_map = HashMap::new();
    let asns = config.asn_filter.clone().unwrap_or_default();
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

        let url = build_url(&config.api_base_url, "/radar/bgp/leaks/events", &params);
        let result: BgpLeakResult = fetch_cloudflare(client, &url).await?;

        for event in result.events.into_iter() {
            let should_emit =
                emit_changes && is_after_min_timestamp(min_timestamp, &event.detected_ts);
            new_map.insert(event.id, event.clone());

            if !state.seen_leak_ids.contains_key(&event.id) {
                if should_emit {
                    let changes = map_leak(source_id, &event, ChangeAction::Insert, true);
                    dispatch_changes(source_id, dispatchers, changes).await?;
                }
            } else if state.seen_leak_ids.get(&event.id) != Some(&event) && should_emit {
                let changes = map_leak(source_id, &event, ChangeAction::Update, false);
                dispatch_changes(source_id, dispatchers, changes).await?;

                // Delete old relationships and insert new ones
                if let Some(old_event) = state.seen_leak_ids.get(&event.id) {
                    let old_rels = relation_element_ids_for_leak(old_event);
                    let del_changes = delete_relations(source_id, &old_rels);
                    dispatch_changes(source_id, dispatchers, del_changes).await?;
                }
                let new_rels = map_leak(source_id, &event, ChangeAction::Insert, true);
                if new_rels.len() > 1 {
                    dispatch_changes(source_id, dispatchers, new_rels[1..].to_vec()).await?;
                }
            }
        }
    }

    if emit_changes {
        let removed_ids: Vec<u64> = state
            .seen_leak_ids
            .keys()
            .filter(|id| !new_map.contains_key(*id))
            .copied()
            .collect();

        for id in removed_ids {
            if let Some(old_event) = state.seen_leak_ids.get(&id) {
                let rel_ids = relation_element_ids_for_leak(old_event);
                let rel_changes = delete_relations(source_id, &rel_ids);
                dispatch_changes(source_id, dispatchers, rel_changes).await?;

                let changes = map_leak(source_id, old_event, ChangeAction::Delete, false);
                dispatch_changes(source_id, dispatchers, changes).await?;
            }
        }
    }

    state.seen_leak_ids = new_map;
    Ok(())
}

async fn poll_http_summary(
    source_id: &str,
    config: &CloudflareRadarConfig,
    client: &Client,
    dispatchers: &Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    state: &mut PollingState,
    emit_changes: bool,
) -> Result<()> {
    let params = vec![
        ("dateRange", config.analytics_date_range.clone()),
        ("format", "json".to_string()),
    ];
    let url = build_url(
        &config.api_base_url,
        "/radar/http/summary/device_type",
        &params,
    );
    let result: HttpSummaryResult = fetch_cloudflare(client, &url).await?;

    let hash = calculate_hash(&result)?;
    if state.last_http_hash == Some(hash) {
        return Ok(());
    }

    if emit_changes {
        let effective_from = Utc::now().timestamp_millis() as u64;
        let action = if state.last_http_hash.is_some() {
            ChangeAction::Update
        } else {
            ChangeAction::Insert
        };
        for (series, summary) in result.summaries.iter() {
            let node_id = format!("http-traffic-{}", normalize_id(series));
            let change =
                map_http_summary(source_id, &node_id, series, summary, action, effective_from);
            dispatch_changes(source_id, dispatchers, vec![change]).await?;
        }
    }

    state.last_http_hash = Some(hash);
    Ok(())
}

async fn poll_attack_l7_summary(
    source_id: &str,
    config: &CloudflareRadarConfig,
    client: &Client,
    dispatchers: &Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    state: &mut PollingState,
    emit_changes: bool,
) -> Result<()> {
    let params = vec![
        ("dateRange", config.analytics_date_range.clone()),
        ("format", "json".to_string()),
    ];
    let url = build_url(
        &config.api_base_url,
        "/radar/attacks/layer7/summary",
        &params,
    );
    let result: AttackSummaryResult = fetch_cloudflare(client, &url).await?;

    let hash = calculate_hash(&result)?;
    if state.last_l7_hash == Some(hash) {
        return Ok(());
    }

    if emit_changes {
        let effective_from = Utc::now().timestamp_millis() as u64;
        let action = if state.last_l7_hash.is_some() {
            ChangeAction::Update
        } else {
            ChangeAction::Insert
        };
        for (series, summary) in result.summaries.iter() {
            let node_id = format!("attack-l7-{}", normalize_id(series));
            let change =
                map_attack_l7_summary(source_id, &node_id, series, summary, action, effective_from);
            dispatch_changes(source_id, dispatchers, vec![change]).await?;
        }
    }

    state.last_l7_hash = Some(hash);
    Ok(())
}

async fn poll_attack_l3_summary(
    source_id: &str,
    config: &CloudflareRadarConfig,
    client: &Client,
    dispatchers: &Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    state: &mut PollingState,
    emit_changes: bool,
) -> Result<()> {
    let params = vec![
        ("dateRange", config.analytics_date_range.clone()),
        ("format", "json".to_string()),
    ];
    let url = build_url(
        &config.api_base_url,
        "/radar/attacks/layer3/summary",
        &params,
    );
    let result: AttackLayer3SummaryResult = fetch_cloudflare(client, &url).await?;

    let hash = calculate_hash(&result)?;
    if state.last_l3_hash == Some(hash) {
        return Ok(());
    }

    if emit_changes {
        let effective_from = Utc::now().timestamp_millis() as u64;
        let action = if state.last_l3_hash.is_some() {
            ChangeAction::Update
        } else {
            ChangeAction::Insert
        };
        for (series, summary) in result.summaries.iter() {
            let node_id = format!("attack-l3-{}", normalize_id(series));
            let change =
                map_attack_l3_summary(source_id, &node_id, series, summary, action, effective_from);
            dispatch_changes(source_id, dispatchers, vec![change]).await?;
        }
    }

    state.last_l3_hash = Some(hash);
    Ok(())
}

async fn poll_domain_rankings(
    source_id: &str,
    config: &CloudflareRadarConfig,
    client: &Client,
    dispatchers: &Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    state: &mut PollingState,
    emit_changes: bool,
) -> Result<()> {
    let params = vec![("limit", config.ranking_limit.to_string())];
    let url = build_url(&config.api_base_url, "/radar/ranking/top", &params);
    let result: RankingResult = fetch_cloudflare(client, &url).await?;

    let mut next_rankings = HashMap::new();
    let effective_from = Utc::now().timestamp_millis() as u64;

    for ranks in result.rankings.values() {
        for domain in ranks {
            next_rankings.insert(domain.domain.clone(), domain.rank);
            if !emit_changes {
                continue;
            }

            let prev_rank = state.known_rankings.get(&domain.domain).copied();
            if prev_rank != Some(domain.rank) {
                let action = if prev_rank.is_some() {
                    ChangeAction::Update
                } else {
                    ChangeAction::Insert
                };
                let changes = map_domain_ranking(source_id, domain, action, effective_from);
                dispatch_changes(source_id, dispatchers, changes).await?;
            }
        }
    }

    if emit_changes {
        let removed_domains: Vec<DomainRank> = state
            .known_rankings
            .iter()
            .filter(|(domain, _)| !next_rankings.contains_key(*domain))
            .map(|(domain, rank)| DomainRank {
                domain: domain.clone(),
                rank: *rank,
            })
            .collect();

        for removed in removed_domains {
            let changes =
                map_domain_ranking(source_id, &removed, ChangeAction::Delete, effective_from);
            dispatch_changes(source_id, dispatchers, changes).await?;
        }
    }

    state.known_rankings = next_rankings;
    Ok(())
}

async fn poll_dns_top_locations(
    source_id: &str,
    config: &CloudflareRadarConfig,
    client: &Client,
    dispatchers: &Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    state: &mut PollingState,
    emit_changes: bool,
) -> Result<()> {
    let domains = config.dns_domains.clone().unwrap_or_default();
    let effective_from = Utc::now().timestamp_millis() as u64;

    for domain in domains {
        let params = vec![
            ("domain", domain.clone()),
            ("dateRange", config.analytics_date_range.clone()),
            ("format", "json".to_string()),
            ("limit", "10".to_string()),
        ];
        let url = build_url(&config.api_base_url, "/radar/dns/top/locations", &params);
        let result: DnsTopLocationsResult = fetch_cloudflare(client, &url).await?;

        let hash = calculate_hash(&result)?;
        let previous = state.last_dns_hash.get(&domain).copied();
        if previous == Some(hash) {
            continue;
        }

        if emit_changes {
            let action = if previous.is_some() {
                ChangeAction::Update
            } else {
                ChangeAction::Insert
            };
            if let Some(locations) = result.top.values().next() {
                let change = map_dns_summary(source_id, &domain, locations, action, effective_from);
                dispatch_changes(source_id, dispatchers, vec![change]).await?;
            }
        }

        state.last_dns_hash.insert(domain, hash);
    }

    Ok(())
}

async fn dispatch_changes(
    source_id: &str,
    dispatchers: &Arc<
        RwLock<
            Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
        >,
    >,
    changes: Vec<SourceChange>,
) -> Result<()> {
    for change in changes {
        let event = SourceEvent::Change(change);
        let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
        profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());
        let wrapper =
            SourceEventWrapper::with_profiling(source_id.to_string(), event, Utc::now(), profiling);
        drasi_lib::sources::base::SourceBase::dispatch_from_task(
            dispatchers.clone(),
            wrapper,
            source_id,
        )
        .await?;
    }
    Ok(())
}

fn calculate_hash<T: serde::Serialize>(value: &T) -> Result<u64> {
    let mut value = serde_json::to_value(value)?;
    canonicalize_json(&mut value);
    let json = serde_json::to_vec(&value)?;
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    json.hash(&mut hasher);
    Ok(hasher.finish())
}

fn canonicalize_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> =
                std::mem::take(map).into_iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            for (key, mut value) in entries {
                canonicalize_json(&mut value);
                map.insert(key, value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                canonicalize_json(value);
            }
        }
        _ => {}
    }
}

fn outage_matches_filters(
    outage: &OutageAnnotation,
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

fn is_after_min_timestamp(min_timestamp: Option<i64>, timestamp: &Option<String>) -> bool {
    match min_timestamp {
        None => true,
        Some(threshold) => {
            if let Some(value) = timestamp.as_ref() {
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
                    return dt.timestamp() >= threshold;
                }
            }
            true
        }
    }
}

async fn load_state(
    source_id: &str,
    store: Option<&Arc<dyn StateStoreProvider>>,
) -> Result<PollingState> {
    if let Some(store) = store {
        let mut state = PollingState::new();
        if let Some(bytes) = store.get(source_id, "initialized").await? {
            state.initialized =
                deserialize_or_warn::<bool>(source_id, "initialized", &bytes).unwrap_or(false);
        }
        if let Some(bytes) = store.get(source_id, "seen_hijack_ids").await? {
            state.seen_hijack_ids =
                deserialize_or_warn(source_id, "seen_hijack_ids", &bytes).unwrap_or_default();
        }
        if let Some(bytes) = store.get(source_id, "seen_leak_ids").await? {
            state.seen_leak_ids =
                deserialize_or_warn(source_id, "seen_leak_ids", &bytes).unwrap_or_default();
        }
        if let Some(bytes) = store.get(source_id, "seen_outage_ids").await? {
            state.seen_outage_ids =
                deserialize_or_warn(source_id, "seen_outage_ids", &bytes).unwrap_or_default();
        }
        if let Some(bytes) = store.get(source_id, "last_http_hash").await? {
            state.last_http_hash = deserialize_or_warn(source_id, "last_http_hash", &bytes);
        }
        if let Some(bytes) = store.get(source_id, "last_l7_hash").await? {
            state.last_l7_hash = deserialize_or_warn(source_id, "last_l7_hash", &bytes);
        }
        if let Some(bytes) = store.get(source_id, "last_l3_hash").await? {
            state.last_l3_hash = deserialize_or_warn(source_id, "last_l3_hash", &bytes);
        }
        if let Some(bytes) = store.get(source_id, "last_dns_hash").await? {
            state.last_dns_hash =
                deserialize_or_warn(source_id, "last_dns_hash", &bytes).unwrap_or_default();
        }
        if let Some(bytes) = store.get(source_id, "known_rankings").await? {
            state.known_rankings =
                deserialize_or_warn(source_id, "known_rankings", &bytes).unwrap_or_default();
        }
        return Ok(state);
    }
    Ok(PollingState::new())
}

fn deserialize_or_warn<T: serde::de::DeserializeOwned>(
    source_id: &str,
    field: &str,
    bytes: &[u8],
) -> Option<T> {
    match serde_json::from_slice(bytes) {
        Ok(value) => Some(value),
        Err(err) => {
            warn!("[{source_id}] Failed to deserialize {field}: {err}");
            None
        }
    }
}

async fn save_state(
    source_id: &str,
    store: &Arc<dyn StateStoreProvider>,
    state: &PollingState,
) -> Result<()> {
    store
        .set(
            source_id,
            "initialized",
            serde_json::to_vec(&state.initialized)?,
        )
        .await?;
    store
        .set(
            source_id,
            "seen_hijack_ids",
            serde_json::to_vec(&state.seen_hijack_ids)?,
        )
        .await?;
    store
        .set(
            source_id,
            "seen_leak_ids",
            serde_json::to_vec(&state.seen_leak_ids)?,
        )
        .await?;
    store
        .set(
            source_id,
            "seen_outage_ids",
            serde_json::to_vec(&state.seen_outage_ids)?,
        )
        .await?;
    if let Some(hash) = state.last_http_hash {
        store
            .set(source_id, "last_http_hash", serde_json::to_vec(&hash)?)
            .await?;
    }
    if let Some(hash) = state.last_l7_hash {
        store
            .set(source_id, "last_l7_hash", serde_json::to_vec(&hash)?)
            .await?;
    }
    if let Some(hash) = state.last_l3_hash {
        store
            .set(source_id, "last_l3_hash", serde_json::to_vec(&hash)?)
            .await?;
    }
    store
        .set(
            source_id,
            "last_dns_hash",
            serde_json::to_vec(&state.last_dns_hash)?,
        )
        .await?;
    store
        .set(
            source_id,
            "known_rankings",
            serde_json::to_vec(&state.known_rankings)?,
        )
        .await?;
    Ok(())
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "cloudflare-radar-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::CloudflareRadarSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
