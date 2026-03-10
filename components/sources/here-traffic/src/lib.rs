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

//! HERE Traffic API source plugin for Drasi.
//!
//! Polls the HERE Traffic API v7 flow and incidents endpoints and emits
//! changes for TrafficSegment and TrafficIncident nodes, plus AFFECTS
//! relationships based on proximity.

mod client;
mod config;
pub mod descriptor;
pub mod mapping;
mod state;

#[cfg(test)]
mod tests;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{watch, Mutex, RwLock};

use drasi_lib::channels::{ComponentStatus, DispatchMode};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::sources::Source;
use drasi_lib::state_store::StateStoreProvider;
use tracing::Instrument;

pub use client::{FlowResponse, HereTrafficClient, IncidentsResponse};
pub use config::{AuthMethod, Endpoint, HereTrafficConfig, StartFrom};
pub use mapping::{TrafficIncidentSnapshot, TrafficSegmentSnapshot};

const LAST_POLL_KEY: &str = "last_poll_timestamp";

/// HERE Traffic source implementation.
pub struct HereTrafficSource {
    base: SourceBase,
    config: HereTrafficConfig,
    client: HereTrafficClient,
    state: Arc<Mutex<state::SourceState>>,
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl HereTrafficSource {
    /// Create a HERE Traffic source from configuration.
    pub fn new(id: impl Into<String>, config: HereTrafficConfig) -> Result<Self> {
        HereTrafficSourceBuilder::from_config(id, config).build()
    }

    /// Create a builder using API key authentication.
    pub fn builder(
        id: impl Into<String>,
        api_key: impl Into<String>,
        bounding_box: impl Into<String>,
    ) -> HereTrafficSourceBuilder {
        HereTrafficSourceBuilder::new(id, api_key, bounding_box)
    }

    /// Create a builder using OAuth 2.0 authentication.
    pub fn builder_oauth(
        id: impl Into<String>,
        access_key_id: impl Into<String>,
        access_key_secret: impl Into<String>,
        bounding_box: impl Into<String>,
    ) -> HereTrafficSourceBuilder {
        HereTrafficSourceBuilder::new_oauth(id, access_key_id, access_key_secret, bounding_box)
    }

    async fn run_loop(
        base: SourceBase,
        config: HereTrafficConfig,
        client: HereTrafficClient,
        state: Arc<Mutex<state::SourceState>>,
        mut stop_signal: watch::Receiver<bool>,
    ) {
        let source_id = base.id.clone();
        match load_last_poll_timestamp(&base).await {
            Ok(Some(ts)) => {
                state.lock().await.last_poll = Some(ts);
                info!("[{source_id}] Resumed from last poll timestamp: {ts}");
            }
            Ok(None) => {}
            Err(e) => {
                warn!("[{source_id}] Failed to load last poll timestamp: {e}");
            }
        }

        match config.start_from {
            StartFrom::Now => {}
            StartFrom::Timestamp { value } => {
                info!(
                    "[{source_id}] StartFrom configured with timestamp {value} (ignored for polling)"
                );
            }
        }

        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(config.polling_interval));

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = poll_and_dispatch(&base, &config, &client, &state).await {
                        error!("[{source_id}] Poll failed: {e}");
                    }
                }
                _ = stop_signal.changed() => {
                    if *stop_signal.borrow() {
                        info!("[{source_id}] Stop signal received");
                        break;
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Source for HereTrafficSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "here-traffic"
    }

    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "bounding_box".to_string(),
            serde_json::Value::String(self.config.bounding_box.clone()),
        );
        props.insert(
            "polling_interval".to_string(),
            serde_json::Value::Number(self.config.polling_interval.into()),
        );
        props.insert(
            "endpoints".to_string(),
            serde_json::Value::Array(
                self.config
                    .endpoints
                    .iter()
                    .map(|endpoint| match endpoint {
                        Endpoint::Flow => serde_json::Value::String("flow".to_string()),
                        Endpoint::Incidents => serde_json::Value::String("incidents".to_string()),
                    })
                    .collect(),
            ),
        );
        props.insert(
            "flow_change_threshold".to_string(),
            serde_json::Value::Number(
                serde_json::Number::from_f64(self.config.flow_change_threshold)
                    .unwrap_or_else(|| serde_json::Number::from(0)),
            ),
        );
        props.insert(
            "speed_change_threshold".to_string(),
            serde_json::Value::Number(
                serde_json::Number::from_f64(self.config.speed_change_threshold)
                    .unwrap_or_else(|| serde_json::Number::from(0)),
            ),
        );
        props.insert(
            "incident_match_distance_meters".to_string(),
            serde_json::Value::Number(
                serde_json::Number::from_f64(self.config.incident_match_distance_meters)
                    .unwrap_or_else(|| serde_json::Number::from(0)),
            ),
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

        self.base.set_status(ComponentStatus::Starting, None).await;
        info!("Starting HERE Traffic source '{}'", self.base.id);

        let base = self.base.clone_shared();
        let config = self.config.clone();
        let client = self.client.clone();
        let state = Arc::clone(&self.state);
        let stop_signal = self.shutdown_rx.clone();

        let instance_id = self
            .base
            .context()
            .await
            .map(|context| context.instance_id)
            .unwrap_or_default();

        let source_id_for_span = self.base.id.clone();
        let span = tracing::info_span!(
            "here_traffic_source",
            instance_id = %instance_id,
            component_id = %source_id_for_span,
            component_type = "source"
        );

        let handle = tokio::spawn(
            async move {
                Self::run_loop(base, config, client, state, stop_signal).await;
            }
            .instrument(span),
        );

        *self.task_handle.write().await = Some(handle);
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping HERE Traffic source '{}'", self.base.id);
        if let Err(e) = self.shutdown_tx.send(true) {
            warn!("Failed to send shutdown signal: {e}");
        }

        if let Some(handle) = self.task_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    debug!("HERE Traffic source task stopped gracefully");
                }
                Ok(Err(e)) => {
                    warn!("HERE Traffic source task panicked: {e}");
                }
                Err(_) => {
                    warn!("HERE Traffic source task did not stop within timeout");
                }
            }
        }

        self.base.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<drasi_lib::channels::SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "HERE Traffic")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// Builder for HERE Traffic source.
pub struct HereTrafficSourceBuilder {
    id: String,
    config: HereTrafficConfig,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    auto_start: bool,
}

impl HereTrafficSourceBuilder {
    /// Create a builder using API key authentication.
    pub fn new(
        id: impl Into<String>,
        api_key: impl Into<String>,
        bounding_box: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            config: HereTrafficConfig::new(api_key, bounding_box),
            bootstrap_provider: None,
            state_store: None,
            auto_start: true,
        }
    }

    /// Create a builder using OAuth 2.0 authentication.
    pub fn new_oauth(
        id: impl Into<String>,
        access_key_id: impl Into<String>,
        access_key_secret: impl Into<String>,
        bounding_box: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            config: HereTrafficConfig::new_oauth(access_key_id, access_key_secret, bounding_box),
            bootstrap_provider: None,
            state_store: None,
            auto_start: true,
        }
    }

    pub fn from_config(id: impl Into<String>, config: HereTrafficConfig) -> Self {
        Self {
            id: id.into(),
            config,
            bootstrap_provider: None,
            state_store: None,
            auto_start: true,
        }
    }

    pub fn with_polling_interval(mut self, interval: std::time::Duration) -> Self {
        self.config.polling_interval = interval.as_secs().max(1);
        self
    }

    pub fn with_polling_interval_secs(mut self, interval_secs: u64) -> Self {
        self.config.polling_interval = interval_secs.max(1);
        self
    }

    pub fn with_endpoints(mut self, endpoints: Vec<Endpoint>) -> Self {
        self.config.endpoints = endpoints;
        self
    }

    pub fn with_flow_change_threshold(mut self, threshold: f64) -> Self {
        self.config.flow_change_threshold = threshold;
        self
    }

    pub fn with_speed_change_threshold(mut self, threshold: f64) -> Self {
        self.config.speed_change_threshold = threshold;
        self
    }

    pub fn with_incident_match_distance_meters(mut self, distance: f64) -> Self {
        self.config.incident_match_distance_meters = distance;
        self
    }

    pub fn with_start_from(mut self, start_from: StartFrom) -> Self {
        self.config.start_from = start_from;
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.config.base_url = base_url.into();
        self
    }

    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    pub fn with_state_store(mut self, store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(store);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn build(self) -> Result<HereTrafficSource> {
        self.config.validate()?;
        if self.config.polling_interval < 30 {
            warn!(
                "polling_interval is set to {}s which may exceed HERE API rate limits",
                self.config.polling_interval
            );
        }

        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);

        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        if let Some(state_store) = self.state_store {
            params = params.with_state_store(state_store);
        }

        let base = SourceBase::new(params)?;
        let client =
            HereTrafficClient::new(self.config.auth.clone(), self.config.base_url.clone())?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        Ok(HereTrafficSource {
            base,
            config: self.config,
            client,
            state: Arc::new(Mutex::new(state::SourceState::default())),
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx,
            shutdown_rx,
        })
    }
}

async fn poll_and_dispatch(
    base: &SourceBase,
    config: &HereTrafficConfig,
    client: &HereTrafficClient,
    state: &Arc<Mutex<state::SourceState>>,
) -> Result<()> {
    let mut flow_segments = None;
    let mut incident_snapshots = None;
    let mut flow_success = false;
    let mut incident_success = false;

    if config.endpoints.contains(&Endpoint::Flow) {
        match client.get_flow(&config.bounding_box).await {
            Ok(flow) => {
                flow_segments = Some(mapping::extract_segments(flow));
                flow_success = true;
            }
            Err(e) => {
                error!("[{}] Flow endpoint failed: {e}", base.id);
            }
        }
    }

    if config.endpoints.contains(&Endpoint::Incidents) {
        match client.get_incidents(&config.bounding_box).await {
            Ok(incidents) => {
                incident_snapshots = Some(mapping::extract_incidents(incidents));
                incident_success = true;
            }
            Err(e) => {
                error!("[{}] Incidents endpoint failed: {e}", base.id);
            }
        }
    }

    if !flow_success && !incident_success {
        return Err(anyhow::anyhow!("Both HERE Traffic API endpoints failed"));
    }

    let mut changes = Vec::new();
    {
        let mut guard = state.lock().await;
        if let Some(segments) = flow_segments {
            changes.extend(guard.update_flow(&base.id, config, segments));
        }
        if let Some(incidents) = incident_snapshots {
            changes.extend(guard.update_incidents(&base.id, incidents));
        }
        if flow_success || incident_success {
            changes.extend(guard.update_relations(&base.id, config));
        }
        guard.last_poll = Some(Utc::now());
    }

    for change in changes {
        base.dispatch_source_change(change).await?;
    }

    if flow_success || incident_success {
        save_last_poll_timestamp(base, Utc::now()).await?;
    }

    Ok(())
}

async fn load_last_poll_timestamp(base: &SourceBase) -> Result<Option<DateTime<Utc>>> {
    if let Some(store) = base.state_store().await {
        if let Some(bytes) = store.get(&base.id, LAST_POLL_KEY).await? {
            let raw = String::from_utf8(bytes)?;
            if let Ok(parsed) = DateTime::parse_from_rfc3339(&raw) {
                let timestamp = parsed.with_timezone(&Utc);
                info!("[{}] Loaded last poll timestamp: {}", base.id, timestamp);
                return Ok(Some(timestamp));
            }
        }
    }
    Ok(None)
}

async fn save_last_poll_timestamp(base: &SourceBase, timestamp: DateTime<Utc>) -> Result<()> {
    if let Some(store) = base.state_store().await {
        store
            .set(&base.id, LAST_POLL_KEY, timestamp.to_rfc3339().into_bytes())
            .await?;
    }
    Ok(())
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "here-traffic-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::HereTrafficSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
