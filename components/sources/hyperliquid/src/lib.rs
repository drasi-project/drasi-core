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

//! Hyperliquid market data source plugin for Drasi.
//!
//! This source connects to Hyperliquid's public REST + WebSocket APIs to stream
//! real-time DeFi market data into Drasi as graph nodes and relationships.

pub mod config;
pub mod descriptor;
pub mod mapping;
pub mod rest;
pub mod stream;
pub mod types;

#[cfg(test)]
mod tests;

pub use config::{CoinSelection, HyperliquidNetwork, HyperliquidSourceConfig, InitialCursor};

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{Element, SourceChange};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::{
    BootstrapEvent, BootstrapEventSender, ComponentStatus, DispatchMode, SubscriptionResponse,
};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::state_store::{MemoryStateStoreProvider, StateStoreProvider};
use drasi_lib::Source;
use log::{info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{watch, RwLock};
use tracing::Instrument;

use crate::mapping::{
    map_funding_rate_to_changes, map_meta_to_coin_changes, map_mid_prices_to_changes,
    map_order_book_to_changes, map_spot_meta_to_nodes, InitializedEntities,
};
use crate::rest::HyperliquidRestClient;
use crate::stream::{
    load_funding_state, load_trade_dedupe_state, run_funding_poll, run_ws_stream,
    FundingPollParams, StreamState, WsStreamParams,
};

/// Hyperliquid market data source.
pub struct HyperliquidSource {
    base: SourceBase,
    config: HyperliquidSourceConfig,
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    stream_state: StreamState,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl HyperliquidSource {
    /// Create a builder for HyperliquidSource.
    pub fn builder(id: impl Into<String>) -> HyperliquidSourceBuilder {
        HyperliquidSourceBuilder::new(id)
    }
}

#[async_trait]
impl Source for HyperliquidSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "hyperliquid"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "network".to_string(),
            serde_json::Value::String(format!("{:?}", self.config.network)),
        );

        let coins_value = match &self.config.coins {
            CoinSelection::Specific { coins } => serde_json::Value::Array(
                coins
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
            CoinSelection::All => serde_json::Value::String("all".to_string()),
        };
        props.insert("coins".to_string(), coins_value);

        props.insert(
            "enable_trades".to_string(),
            serde_json::Value::Bool(self.config.enable_trades),
        );
        props.insert(
            "enable_order_book".to_string(),
            serde_json::Value::Bool(self.config.enable_order_book),
        );
        props.insert(
            "enable_mid_prices".to_string(),
            serde_json::Value::Bool(self.config.enable_mid_prices),
        );
        props.insert(
            "enable_funding_rates".to_string(),
            serde_json::Value::Bool(self.config.enable_funding_rates),
        );
        props.insert(
            "enable_liquidations".to_string(),
            serde_json::Value::Bool(self.config.enable_liquidations),
        );
        props.insert(
            "funding_poll_interval_secs".to_string(),
            serde_json::Value::Number(self.config.funding_poll_interval_secs.into()),
        );
        props.insert(
            "initial_cursor".to_string(),
            serde_json::to_value(&self.config.initial_cursor).unwrap_or_default(),
        );

        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base.set_status(ComponentStatus::Starting, Some("Starting Hyperliquid source".to_string())).await;
        info!("Starting Hyperliquid source '{}'", self.base.id);

        let config = self.config.clone();
        config.validate()?;

        let rest_client = HyperliquidRestClient::new(config.network.rest_url());
        let coins = match &config.coins {
            CoinSelection::Specific { coins } => coins.clone(),
            CoinSelection::All => rest_client.resolve_all_coins().await?,
        };

        if coins.is_empty() {
            warn!("Hyperliquid source '{}': resolved coin list is empty — source will produce no data", self.base.id);
        }

        let state_store = self.state_store.read().await.clone();
        let trade_dedupe = load_trade_dedupe_state(&self.base.id, &state_store, &coins).await;
        let funding_state = load_funding_state(&self.base.id, &state_store, &coins).await;

        *self.stream_state.trade_dedupe.write().await = trade_dedupe;
        *self.stream_state.funding_state.write().await = funding_state;

        let source_id = self.base.id.clone();
        let dispatchers = self.base.dispatchers.clone();
        let stream_state = self.stream_state.clone();
        let shutdown_rx = self.shutdown_rx.clone();
        let start_timestamp = config.initial_cursor.start_timestamp();
        let ws_url = config.network.ws_url();

        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span_source_id = source_id.clone();
        let stream_task = tokio::spawn(
            async move {
                let ws_handle = tokio::spawn(run_ws_stream(WsStreamParams {
                    source_id: source_id.clone(),
                    ws_url,
                    config: config.clone(),
                    coins,
                    dispatchers: dispatchers.clone(),
                    state_store: state_store.clone(),
                    stream_state: stream_state.clone(),
                    shutdown_rx: shutdown_rx.clone(),
                    start_timestamp,
                }));

                let funding_handle = if config.enable_funding_rates {
                    let rest_client = HyperliquidRestClient::new(config.network.rest_url());
                    Some(tokio::spawn(run_funding_poll(FundingPollParams {
                        source_id: source_id.clone(),
                        rest_client,
                        config: config.clone(),
                        dispatchers: dispatchers.clone(),
                        state_store: state_store.clone(),
                        stream_state: stream_state.clone(),
                        shutdown_rx: shutdown_rx.clone(),
                        start_timestamp,
                    })))
                } else {
                    None
                };

                match funding_handle {
                    Some(funding_h) => {
                        // Pin both handles so select! can poll without consuming.
                        tokio::pin!(ws_handle);
                        tokio::pin!(funding_h);

                        tokio::select! {
                            result = &mut ws_handle => {
                                match result {
                                    Ok(Ok(_)) => {}
                                    Ok(Err(e)) => warn!("WebSocket stream error: {e}"),
                                    Err(e) => warn!("WebSocket stream task panicked: {e}"),
                                }
                                funding_h.abort();
                            }
                            result = &mut funding_h => {
                                match result {
                                    Ok(Ok(_)) => {}
                                    Ok(Err(e)) => warn!("Funding poll error: {e}"),
                                    Err(e) => warn!("Funding poll task panicked: {e}"),
                                }
                                ws_handle.abort();
                            }
                        }
                    }
                    None => match ws_handle.await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => warn!("WebSocket stream error: {e}"),
                        Err(e) => warn!("WebSocket stream task panicked: {e}"),
                    },
                }
            }
            .instrument(tracing::info_span!(
                "hyperliquid_source",
                instance_id = %instance_id,
                component_id = %span_source_id,
                component_type = "source"
            )),
        );

        *self.task_handle.write().await = Some(stream_task);
        self.base.set_status(ComponentStatus::Running, Some("Hyperliquid source started".to_string())).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping Hyperliquid source '{}'", self.base.id);

        if let Err(e) = self.shutdown_tx.send(true) {
            warn!("Failed to send shutdown signal: {e}");
        }

        if let Some(handle) = self.task_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Hyperliquid source task panicked: {e}"),
                Err(_) => warn!("Hyperliquid source task did not stop within timeout"),
            }
        }

        self.base.set_status(ComponentStatus::Stopped, Some("Hyperliquid source stopped".to_string())).await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "Hyperliquid")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context.clone()).await;

        if let Some(state_store) = context.state_store {
            *self.state_store.write().await = Some(state_store);
        }
    }

    async fn set_bootstrap_provider(&self, provider: Box<dyn BootstrapProvider + 'static>) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// Builder for HyperliquidSource.
pub struct HyperliquidSourceBuilder {
    id: String,
    config: HyperliquidSourceConfig,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    auto_start: bool,
    state_store: Option<Arc<dyn StateStoreProvider>>,
}

impl HyperliquidSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: HyperliquidSourceConfig::default(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
            state_store: None,
        }
    }

    pub fn with_network(mut self, network: HyperliquidNetwork) -> Self {
        self.config.network = network;
        self
    }

    pub fn with_coins(mut self, coins: Vec<impl Into<String>>) -> Self {
        self.config.coins = CoinSelection::Specific {
            coins: coins.into_iter().map(Into::into).collect(),
        };
        self
    }

    pub fn with_all_coins(mut self) -> Self {
        self.config.coins = CoinSelection::All;
        self
    }

    pub fn with_trades(mut self, enabled: bool) -> Self {
        self.config.enable_trades = enabled;
        self
    }

    pub fn with_order_book(mut self, enabled: bool) -> Self {
        self.config.enable_order_book = enabled;
        self
    }

    pub fn with_mid_prices(mut self, enabled: bool) -> Self {
        self.config.enable_mid_prices = enabled;
        self
    }

    pub fn with_funding_rates(mut self, enabled: bool) -> Self {
        self.config.enable_funding_rates = enabled;
        self
    }

    pub fn with_liquidations(mut self, enabled: bool) -> Self {
        self.config.enable_liquidations = enabled;
        self
    }

    pub fn with_funding_poll_interval_secs(mut self, interval_secs: u64) -> Self {
        self.config.funding_poll_interval_secs = interval_secs;
        self
    }

    pub fn start_from_beginning(mut self) -> Self {
        self.config.initial_cursor = InitialCursor::StartFromBeginning;
        self
    }

    pub fn start_from_now(mut self) -> Self {
        self.config.initial_cursor = InitialCursor::StartFromNow;
        self
    }

    pub fn start_from_timestamp(mut self, timestamp: i64) -> Self {
        self.config.initial_cursor = InitialCursor::StartFromTimestamp { timestamp };
        self
    }

    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    pub fn with_bootstrap_provider(mut self, provider: impl BootstrapProvider + 'static) -> Self {
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

    pub fn build(self) -> Result<HyperliquidSource> {
        self.config.validate()?;

        let state_store = self
            .state_store
            .unwrap_or_else(|| Arc::new(MemoryStateStoreProvider::new()));

        let stream_state = StreamState::default();

        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        } else {
            // Default bootstrap provider shares initialized-entity state with
            // the streaming layer to avoid duplicate Insert events.
            params =
                params.with_bootstrap_provider(HyperliquidBootstrapProvider::with_stream_state(
                    self.config.clone(),
                    stream_state.initialized.clone(),
                ));
        }
        params = params.with_state_store_provider(state_store.clone());

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Ok(HyperliquidSource {
            base: SourceBase::new(params)?,
            config: self.config,
            state_store: Arc::new(RwLock::new(Some(state_store))),
            stream_state,
            shutdown_tx,
            shutdown_rx,
            task_handle: Arc::new(RwLock::new(None)),
        })
    }
}

/// Bootstrap provider for Hyperliquid market data.
pub struct HyperliquidBootstrapProvider {
    config: HyperliquidSourceConfig,
    stream_initialized: Option<Arc<RwLock<InitializedEntities>>>,
}

impl HyperliquidBootstrapProvider {
    pub fn new(config: HyperliquidSourceConfig) -> Self {
        Self {
            config,
            stream_initialized: None,
        }
    }

    /// Create a bootstrap provider that shares initialized-entity tracking with
    /// the streaming layer so that singletons bootstrapped as Inserts are
    /// subsequently emitted as Updates (not duplicate Inserts) by the stream.
    pub fn with_stream_state(
        config: HyperliquidSourceConfig,
        stream_initialized: Arc<RwLock<InitializedEntities>>,
    ) -> Self {
        Self {
            config,
            stream_initialized: Some(stream_initialized),
        }
    }
}

#[async_trait]
impl BootstrapProvider for HyperliquidBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<usize> {
        let rest_client = HyperliquidRestClient::new(self.config.network.rest_url());
        let mut node_changes: Vec<SourceChange> = Vec::new();
        let mut relation_changes: Vec<SourceChange> = Vec::new();
        let mut existing_coins: HashSet<String> = HashSet::new();
        let mut initialized = InitializedEntities::new();

        let meta = rest_client.fetch_meta().await?;
        for asset in &meta.universe {
            existing_coins.insert(asset.name.clone());
        }
        node_changes.extend(map_meta_to_coin_changes(&context.source_id, &meta)?);

        let spot_meta = rest_client.fetch_spot_meta().await?;
        node_changes.extend(map_spot_meta_to_nodes(
            &context.source_id,
            &spot_meta,
            &mut existing_coins,
        )?);

        let coin_filter: Option<HashSet<String>> = match &self.config.coins {
            CoinSelection::Specific { coins } => Some(coins.iter().cloned().collect()),
            CoinSelection::All => None,
        };

        let timestamp = chrono::Utc::now().timestamp_millis();

        if self.config.enable_mid_prices {
            let all_mids = rest_client.fetch_all_mids().await?;
            let filtered_mids: HashMap<String, String> = if let Some(filter) = &coin_filter {
                all_mids
                    .into_iter()
                    .filter(|(coin, _)| filter.contains(coin))
                    .collect()
            } else {
                all_mids
            };

            split_changes(
                map_mid_prices_to_changes(
                    &context.source_id,
                    &filtered_mids,
                    &mut initialized,
                    timestamp,
                )?,
                &mut node_changes,
                &mut relation_changes,
            );
        }

        if self.config.enable_funding_rates {
            let (meta_from_ctx, asset_ctxs) = rest_client.fetch_meta_and_asset_ctxs().await?;
            for (asset, ctx) in meta_from_ctx.universe.iter().zip(asset_ctxs.iter()) {
                if let Some(filter) = &coin_filter {
                    if !filter.contains(&asset.name) {
                        continue;
                    }
                }

                let (changes, _snapshot) = map_funding_rate_to_changes(
                    &context.source_id,
                    &asset.name,
                    ctx,
                    &mut initialized,
                    timestamp,
                )?;
                split_changes(changes, &mut node_changes, &mut relation_changes);
            }
        }

        if self.config.enable_order_book {
            let l2_coins: Vec<String> = match &self.config.coins {
                CoinSelection::Specific { coins } => coins.clone(),
                CoinSelection::All => meta
                    .universe
                    .iter()
                    .map(|asset| asset.name.clone())
                    .collect(),
            };

            for coin in &l2_coins {
                let book = rest_client.fetch_l2_book(coin).await?;
                let changes =
                    map_order_book_to_changes(&context.source_id, &book, &mut initialized)?;
                split_changes(changes, &mut node_changes, &mut relation_changes);
            }
        }

        // Propagate bootstrap-initialized state to the streaming layer so that
        // singleton entities (MidPrice, OrderBook, FundingRate) that were
        // bootstrapped as Inserts will be emitted as Updates by the stream.
        if let Some(shared) = &self.stream_initialized {
            *shared.write().await = initialized;
        }

        let node_labels = &request.node_labels;
        let relation_labels = &request.relation_labels;

        let mut sequence = 0u64;
        for change in node_changes
            .into_iter()
            .filter(|change| matches_labels(change, node_labels, relation_labels))
        {
            sequence += 1;
            event_tx
                .send(BootstrapEvent {
                    source_id: context.source_id.clone(),
                    change,
                    timestamp: chrono::Utc::now(),
                    sequence,
                })
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send bootstrap event: {e}"))?;
        }

        for change in relation_changes
            .into_iter()
            .filter(|change| matches_labels(change, node_labels, relation_labels))
        {
            sequence += 1;
            event_tx
                .send(BootstrapEvent {
                    source_id: context.source_id.clone(),
                    change,
                    timestamp: chrono::Utc::now(),
                    sequence,
                })
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send bootstrap event: {e}"))?;
        }

        Ok(sequence as usize)
    }
}

fn split_changes(
    changes: Vec<SourceChange>,
    nodes: &mut Vec<SourceChange>,
    relations: &mut Vec<SourceChange>,
) {
    for change in changes {
        match &change {
            SourceChange::Insert { element } | SourceChange::Update { element } => match element {
                Element::Node { .. } => nodes.push(change),
                Element::Relation { .. } => relations.push(change),
            },
            _ => nodes.push(change),
        }
    }
}

fn matches_labels(
    change: &SourceChange,
    node_labels: &[String],
    relation_labels: &[String],
) -> bool {
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => match element {
            Element::Node { metadata, .. } => labels_match(&metadata.labels, node_labels),
            Element::Relation { metadata, .. } => labels_match(&metadata.labels, relation_labels),
        },
        _ => true,
    }
}

fn labels_match(labels: &Arc<[Arc<str>]>, requested: &[String]) -> bool {
    if requested.is_empty() {
        return true;
    }

    labels
        .iter()
        .any(|label| requested.iter().any(|req| req == label.as_ref()))
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "hyperliquid-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::HyperliquidSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
