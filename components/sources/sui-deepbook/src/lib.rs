#![allow(unexpected_cfgs)]
// Copyright 2026 The Drasi Authors.
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

//! Sui DeepBook source plugin for Drasi.
//!
//! This source polls the Sui JSON-RPC API (`suix_queryEvents`) for DeepBook package events
//! and emits them as Drasi source changes.

pub mod config;
pub mod descriptor;
pub mod mapping;
pub mod rpc;

pub use config::{
    StartPosition, SuiDeepBookSourceConfig, DEFAULT_DEEPBOOK_PACKAGE_ID, DEFAULT_SUI_MAINNET_RPC,
};

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, DispatchMode, SubscriptionResponse};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::sources::Source;
use drasi_lib::state_store::StateStoreProvider;
use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::sync::RwLock;

use crate::mapping::{
    build_order_node, build_pool_node, build_relationship, build_trader_node, event_order_id,
    event_pool_id, map_event_to_change, should_include_event, EnrichmentConfig,
};
use crate::rpc::{EventCursor, SuiRpcClient};

const CURSOR_STATE_KEY: &str = "cursor";

pub struct SuiDeepBookSource {
    base: SourceBase,
    config: SuiDeepBookSourceConfig,
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl SuiDeepBookSource {
    pub fn new(id: impl Into<String>, config: SuiDeepBookSourceConfig) -> Result<Self> {
        config.validate()?;
        let id = id.into();
        let params = SourceBaseParams::new(&id);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            state_store: Arc::new(RwLock::new(None)),
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx,
            shutdown_rx,
        })
    }

    pub fn builder(id: impl Into<String>) -> SuiDeepBookSourceBuilder {
        SuiDeepBookSourceBuilder::new(id)
    }
}

#[async_trait]
impl Source for SuiDeepBookSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "sui-deepbook"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "rpc_endpoint".to_string(),
            serde_json::Value::String(self.config.rpc_endpoint.clone()),
        );
        props.insert(
            "deepbook_package_id".to_string(),
            serde_json::Value::String(self.config.deepbook_package_id.clone()),
        );
        props.insert(
            "poll_interval_ms".to_string(),
            serde_json::Value::Number(self.config.poll_interval_ms.into()),
        );
        props.insert(
            "request_limit".to_string(),
            serde_json::Value::Number(self.config.request_limit.into()),
        );
        props.insert(
            "event_filters".to_string(),
            serde_json::Value::Array(
                self.config
                    .event_filters
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        props.insert(
            "pools".to_string(),
            serde_json::Value::Array(
                self.config
                    .pools
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        props.insert(
            "start_position".to_string(),
            serde_json::to_value(self.config.start_position).unwrap_or(serde_json::Value::Null),
        );
        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base.set_status(ComponentStatus::Starting).await;
        info!("Starting Sui DeepBook source '{}'", self.base.id);

        let source_id = self.base.id.clone();
        let config = self.config.clone();
        let base = self.base.clone_shared();
        let state_store = self.state_store.read().await.clone();
        let mut shutdown_rx = self.shutdown_rx.clone();

        let task_handle = tokio::spawn(async move {
            if let Err(err) =
                run_poll_loop(&source_id, config, &base, state_store, &mut shutdown_rx).await
            {
                error!("Sui DeepBook polling task failed for '{source_id}': {err}");
                let _ = base
                    .set_status_with_event(ComponentStatus::Error, Some(err.to_string()))
                    .await;
            }
        });

        *self.task_handle.write().await = Some(task_handle);
        self.base.set_status(ComponentStatus::Running).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping Sui DeepBook source '{}'", self.base.id);
        self.base.set_status(ComponentStatus::Stopping).await;

        if let Err(err) = self.shutdown_tx.send(true) {
            warn!(
                "Failed sending shutdown signal for Sui DeepBook source '{}': {err}",
                self.base.id
            );
        }

        if let Some(handle) = self.task_handle.write().await.take() {
            match tokio::time::timeout(Duration::from_secs(5), handle).await {
                Ok(Ok(())) => debug!("Sui DeepBook polling task stopped gracefully"),
                Ok(Err(err)) => warn!("Sui DeepBook polling task panicked: {err}"),
                Err(_) => warn!("Sui DeepBook polling task did not stop within timeout"),
            }
        }

        self.base.set_status(ComponentStatus::Stopped).await;
        Ok(())
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "Sui DeepBook")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context.clone()).await;
        if let Some(state_store) = context.state_store {
            *self.state_store.write().await = Some(state_store);
            debug!(
                "State store injected into Sui DeepBook source '{}'",
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

async fn run_poll_loop(
    source_id: &str,
    config: SuiDeepBookSourceConfig,
    base: &SourceBase,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let rpc_client = SuiRpcClient::new(config.rpc_endpoint.clone())?;
    // Use MoveModule filter to only fetch events from the DeepBook package.
    // This is orders of magnitude faster than {"All":[]} on Sui mainnet.
    let query_filter = serde_json::json!({
        "MoveModule": {
            "package": config.deepbook_package_id,
            "module": "pool"
        }
    });

    let mut cursor = load_cursor(source_id, &state_store).await?;

    if cursor.is_none() && matches!(config.start_position, StartPosition::Now) {
        cursor =
            initialize_cursor_for_now(&rpc_client, &query_filter, config.request_limit).await?;
        if let Some(cursor_ref) = cursor.as_ref() {
            save_cursor(source_id, cursor_ref, &state_store).await?;
        }
    }

    let timestamp_floor = match config.start_position {
        StartPosition::Timestamp(value) => Some(value.max(0) as u64),
        _ => None,
    };

    let enrichment = EnrichmentConfig {
        enable_pool_nodes: config.enable_pool_nodes,
        enable_trader_nodes: config.enable_trader_nodes,
        enable_order_nodes: config.enable_order_nodes,
    };
    let mut enrichment_state = EnrichmentState {
        seen_pools: HashSet::new(),
        seen_traders: HashSet::new(),
        seen_orders: HashSet::new(),
    };

    // ── Lookback phase ──
    // Fetch recent historical events in descending order so the dashboard
    // has data immediately on startup.
    if config.lookback_events > 0 && cursor.is_some() {
        info!(
            "Fetching up to {} recent events for lookback",
            config.lookback_events
        );
        match fetch_lookback_events(
            source_id,
            &config,
            &rpc_client,
            &query_filter,
            &enrichment,
            &mut enrichment_state,
            base,
        )
        .await
        {
            Ok(count) => info!("Lookback phase complete: processed {count} events"),
            Err(err) => warn!("Lookback phase failed (non-fatal, continuing): {err}"),
        }
    }

    let mut consecutive_failures = 0usize;
    loop {
        if *shutdown_rx.borrow() {
            info!("Received shutdown signal for Sui DeepBook source '{source_id}'");
            break;
        }

        let query_result = match rpc_client
            .query_events(
                query_filter.clone(),
                cursor.as_ref(),
                config.request_limit,
                false,
            )
            .await
        {
            Ok(result) => {
                consecutive_failures = 0;
                result
            }
            Err(err) => {
                consecutive_failures += 1;
                error!("Failed querying DeepBook events (attempt {consecutive_failures}): {err}");
                if consecutive_failures >= 10 {
                    return Err(err);
                }
                if wait_for_poll_or_shutdown(shutdown_rx, config.poll_interval_ms).await {
                    break;
                }
                continue;
            }
        };

        let mut latest_cursor = cursor.clone();
        for event in query_result.data {
            latest_cursor = Some(event.id.clone());

            if event.package_id != config.deepbook_package_id {
                continue;
            }

            if let Some(ts_floor) = timestamp_floor {
                if event
                    .timestamp_ms
                    .is_some_and(|timestamp| timestamp < ts_floor)
                {
                    continue;
                }
            }

            if !should_include_event(&event, &config.event_filters, &config.pools) {
                continue;
            }

            let effective_from = event
                .timestamp_ms
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().max(0) as u64);

            // Emit enrichment nodes before the event node
            emit_enrichment_nodes(
                source_id,
                &event,
                &enrichment,
                &rpc_client,
                &mut enrichment_state,
                effective_from,
                base,
            )
            .await?;

            // Emit the event node itself
            let event_entity_id = crate::mapping::derive_entity_id_pub(&event);
            let change = map_event_to_change(source_id, &event);
            base.dispatch_source_change(change).await?;

            // Emit relationships after both nodes exist
            emit_enrichment_relationships(
                source_id,
                &event,
                &enrichment,
                &event_entity_id,
                effective_from,
                base,
            )
            .await?;
        }

        // Always advance cursor from next_cursor when present, even if all
        // events on this page were filtered out.  Without this, the source
        // would re-fetch the same page forever when no events match.
        if let Some(next_cursor) = query_result.next_cursor {
            latest_cursor = Some(next_cursor);
        }

        if latest_cursor != cursor {
            cursor = latest_cursor;
            if let Some(cursor_ref) = cursor.as_ref() {
                save_cursor(source_id, cursor_ref, &state_store).await?;
            }
        }

        if query_result.has_next_page {
            continue;
        }

        if wait_for_poll_or_shutdown(shutdown_rx, config.poll_interval_ms).await {
            break;
        }
    }

    Ok(())
}

/// Mutable state for the enrichment caches within a polling session.
struct EnrichmentState {
    seen_pools: HashSet<String>,
    seen_traders: HashSet<String>,
    seen_orders: HashSet<String>,
}

/// Emit Pool, Trader, and Order nodes if not already seen.
async fn emit_enrichment_nodes(
    source_id: &str,
    event: &crate::rpc::SuiEvent,
    enrichment: &EnrichmentConfig,
    rpc_client: &SuiRpcClient,
    state: &mut EnrichmentState,
    effective_from: u64,
    base: &SourceBase,
) -> Result<()> {
    // Pool node
    if enrichment.enable_pool_nodes {
        if let Some(pool_id) = event_pool_id(event) {
            if state.seen_pools.insert(pool_id.clone()) {
                let object_data = match rpc_client.get_object(&pool_id).await {
                    Ok(data) => {
                        debug!("Fetched pool metadata for {pool_id}");
                        Some(data)
                    }
                    Err(err) => {
                        warn!("Failed to fetch pool object for {pool_id}: {err}");
                        None
                    }
                };
                let pool_change =
                    build_pool_node(source_id, &pool_id, object_data.as_ref(), effective_from);
                base.dispatch_source_change(pool_change).await?;
            }
        }
    }

    // Trader node
    if enrichment.enable_trader_nodes
        && !event.sender.is_empty()
        && state.seen_traders.insert(event.sender.clone())
    {
        let trader_change = build_trader_node(source_id, &event.sender, effective_from);
        base.dispatch_source_change(trader_change).await?;
    }

    // Order node
    if enrichment.enable_order_nodes {
        if let Some(order_id) = event_order_id(event) {
            if state.seen_orders.insert(order_id.clone()) {
                let pool_id = event_pool_id(event);
                let order_change =
                    build_order_node(source_id, &order_id, pool_id.as_deref(), effective_from);
                base.dispatch_source_change(order_change).await?;
            }
        }
    }

    Ok(())
}

/// Emit IN_POOL, SENT_BY, FOR_ORDER relationships after both nodes exist.
async fn emit_enrichment_relationships(
    source_id: &str,
    event: &crate::rpc::SuiEvent,
    enrichment: &EnrichmentConfig,
    event_entity_id: &str,
    effective_from: u64,
    base: &SourceBase,
) -> Result<()> {
    // IN_POOL relationship
    if enrichment.enable_pool_nodes {
        if let Some(pool_id) = event_pool_id(event) {
            let rel = build_relationship(
                source_id,
                "IN_POOL",
                &format!("rel:in_pool:{event_entity_id}"),
                event_entity_id,
                &format!("pool_meta:{pool_id}"),
                effective_from,
            );
            base.dispatch_source_change(rel).await?;
        }
    }

    // SENT_BY relationship
    if enrichment.enable_trader_nodes && !event.sender.is_empty() {
        let rel = build_relationship(
            source_id,
            "SENT_BY",
            &format!("rel:sent_by:{event_entity_id}"),
            event_entity_id,
            &format!("trader:{}", event.sender),
            effective_from,
        );
        base.dispatch_source_change(rel).await?;
    }

    // FOR_ORDER relationship
    if enrichment.enable_order_nodes {
        if let Some(order_id) = event_order_id(event) {
            let rel = build_relationship(
                source_id,
                "FOR_ORDER",
                &format!("rel:for_order:{event_entity_id}"),
                event_entity_id,
                &format!("order_meta:{order_id}"),
                effective_from,
            );
            base.dispatch_source_change(rel).await?;
        }
    }

    Ok(())
}

async fn initialize_cursor_for_now(
    rpc_client: &SuiRpcClient,
    query_filter: &serde_json::Value,
    request_limit: u16,
) -> Result<Option<EventCursor>> {
    let result = rpc_client
        .query_events(query_filter.clone(), None, request_limit.min(10), true)
        .await?;

    if let Some(first) = result.data.first() {
        return Ok(Some(first.id.clone()));
    }
    Ok(result.next_cursor)
}

/// Fetch the most recent `lookback_events` events in descending order,
/// reverse them into chronological order, and process them as inserts.
/// This lets the dashboard display data immediately on startup.
async fn fetch_lookback_events(
    source_id: &str,
    config: &SuiDeepBookSourceConfig,
    rpc_client: &SuiRpcClient,
    query_filter: &serde_json::Value,
    enrichment: &EnrichmentConfig,
    enrichment_state: &mut EnrichmentState,
    base: &SourceBase,
) -> Result<usize> {
    let mut all_events = Vec::new();
    let mut page_cursor: Option<EventCursor> = None;
    let remaining = config.lookback_events as usize;
    let page_size = config.request_limit.min(50);

    // Fetch pages in descending (most recent first) order
    loop {
        let result = rpc_client
            .query_events(
                query_filter.clone(),
                page_cursor.as_ref(),
                page_size,
                true, // descending
            )
            .await?;

        for event in result.data {
            if event.package_id != config.deepbook_package_id {
                continue;
            }
            if !should_include_event(&event, &config.event_filters, &config.pools) {
                continue;
            }
            all_events.push(event);
            if all_events.len() >= remaining {
                break;
            }
        }

        if all_events.len() >= remaining || !result.has_next_page {
            break;
        }
        page_cursor = result.next_cursor;
    }

    // Reverse to chronological order (oldest first)
    all_events.reverse();

    let count = all_events.len();
    debug!("Lookback: processing {count} events in chronological order");

    for event in &all_events {
        let effective_from = event
            .timestamp_ms
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().max(0) as u64);

        emit_enrichment_nodes(
            source_id,
            event,
            enrichment,
            rpc_client,
            enrichment_state,
            effective_from,
            base,
        )
        .await?;

        let event_entity_id = crate::mapping::derive_entity_id_pub(event);
        let change = map_event_to_change(source_id, event);
        base.dispatch_source_change(change).await?;

        emit_enrichment_relationships(
            source_id,
            event,
            enrichment,
            &event_entity_id,
            effective_from,
            base,
        )
        .await?;
    }

    Ok(count)
}

async fn load_cursor(
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
) -> Result<Option<EventCursor>> {
    let Some(store) = state_store else {
        return Ok(None);
    };

    let Some(bytes) = store.get(source_id, CURSOR_STATE_KEY).await? else {
        return Ok(None);
    };

    match serde_json::from_slice::<EventCursor>(&bytes) {
        Ok(cursor) => Ok(Some(cursor)),
        Err(err) => {
            warn!(
                "Failed to parse persisted DeepBook cursor for source '{source_id}': {err}. Clearing state."
            );
            let _ = store.delete(source_id, CURSOR_STATE_KEY).await;
            Ok(None)
        }
    }
}

async fn save_cursor(
    source_id: &str,
    cursor: &EventCursor,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
) -> Result<()> {
    let Some(store) = state_store else {
        return Ok(());
    };

    let bytes = serde_json::to_vec(cursor)?;
    store.set(source_id, CURSOR_STATE_KEY, bytes).await?;
    Ok(())
}

async fn wait_for_poll_or_shutdown(
    shutdown_rx: &mut watch::Receiver<bool>,
    poll_interval_ms: u64,
) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(poll_interval_ms)) => false,
        changed = shutdown_rx.changed() => {
            changed.is_ok() && *shutdown_rx.borrow()
        }
    }
}

pub struct SuiDeepBookSourceBuilder {
    id: String,
    config: SuiDeepBookSourceConfig,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    auto_start: bool,
}

impl SuiDeepBookSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: SuiDeepBookSourceConfig::default(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            state_store: None,
            auto_start: true,
        }
    }

    pub fn with_rpc_endpoint(mut self, rpc_endpoint: impl Into<String>) -> Self {
        self.config.rpc_endpoint = rpc_endpoint.into();
        self
    }

    pub fn with_deepbook_package_id(mut self, package_id: impl Into<String>) -> Self {
        self.config.deepbook_package_id = package_id.into();
        self
    }

    pub fn with_poll_interval_ms(mut self, poll_interval_ms: u64) -> Self {
        self.config.poll_interval_ms = poll_interval_ms;
        self
    }

    pub fn with_request_limit(mut self, request_limit: u16) -> Self {
        self.config.request_limit = request_limit;
        self
    }

    pub fn with_event_filters(mut self, event_filters: Vec<String>) -> Self {
        self.config.event_filters = event_filters;
        self
    }

    pub fn with_pools(mut self, pools: Vec<String>) -> Self {
        self.config.pools = pools;
        self
    }

    pub fn with_start_position(mut self, start_position: StartPosition) -> Self {
        self.config.start_position = start_position;
        self
    }

    pub fn with_start_from_beginning(mut self) -> Self {
        self.config.start_position = StartPosition::Beginning;
        self
    }

    pub fn with_start_from_now(mut self) -> Self {
        self.config.start_position = StartPosition::Now;
        self
    }

    pub fn with_start_from_timestamp(mut self, timestamp_ms: i64) -> Self {
        self.config.start_position = StartPosition::Timestamp(timestamp_ms);
        self
    }

    /// Fetch up to `count` recent historical events on startup before
    /// entering the forward-polling loop.  Useful for populating dashboards
    /// with recent data immediately.
    pub fn with_lookback_events(mut self, count: u16) -> Self {
        self.config.lookback_events = count;
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

    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    pub fn with_state_store(mut self, state_store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(state_store);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_config(mut self, config: SuiDeepBookSourceConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_enable_pool_nodes(mut self, enable: bool) -> Self {
        self.config.enable_pool_nodes = enable;
        self
    }

    pub fn with_enable_trader_nodes(mut self, enable: bool) -> Self {
        self.config.enable_trader_nodes = enable;
        self
    }

    pub fn with_enable_order_nodes(mut self, enable: bool) -> Self {
        self.config.enable_order_nodes = enable;
        self
    }

    pub fn build(self) -> Result<SuiDeepBookSource> {
        self.config.validate()?;

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

        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Ok(SuiDeepBookSource {
            base: SourceBase::new(params)?,
            config: self.config,
            state_store: Arc::new(RwLock::new(self.state_store)),
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx,
            shutdown_rx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_defaults() {
        let source = SuiDeepBookSource::builder("source-1").build().unwrap();
        assert_eq!(source.id(), "source-1");
        assert_eq!(source.type_name(), "sui-deepbook");
        assert_eq!(source.config.poll_interval_ms, 2_000);
    }

    #[test]
    fn test_builder_with_custom_values() {
        let source = SuiDeepBookSource::builder("source-1")
            .with_rpc_endpoint("https://fullnode.testnet.sui.io:443")
            .with_deepbook_package_id("0xabc")
            .with_poll_interval_ms(500)
            .with_request_limit(25)
            .with_start_from_beginning()
            .build()
            .unwrap();

        assert_eq!(
            source.config.rpc_endpoint,
            "https://fullnode.testnet.sui.io:443"
        );
        assert_eq!(source.config.deepbook_package_id, "0xabc");
        assert_eq!(source.config.poll_interval_ms, 500);
        assert_eq!(source.config.request_limit, 25);
        assert!(matches!(
            source.config.start_position,
            StartPosition::Beginning
        ));
    }

    #[test]
    fn test_builder_rejects_invalid_config() {
        let result = SuiDeepBookSource::builder("source-1")
            .with_rpc_endpoint("")
            .build();
        assert!(result.is_err());
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "sui-deepbook-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::SuiDeepBookSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
