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

//! RIPE NCC RIS Live source plugin for Drasi.
//!
//! This source consumes real-time BGP messages from RIPE RIS Live over WebSocket
//! and maps them to a graph model:
//! - `(:Peer)` nodes
//! - `(:Prefix)` nodes
//! - `(peer)-[:ROUTES]->(prefix)` relationships

pub mod config;
pub mod descriptor;
pub mod mapping;
pub mod messages;
pub mod stream;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use log::{error, info};
use tokio::sync::{watch, RwLock};
use tokio::time::timeout;
use tracing::Instrument;

use drasi_lib::channels::{ComponentStatus, DispatchMode, SubscriptionResponse};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::Source;

pub use config::{RisLiveSourceConfig, StartFrom};

/// RIS Live source implementation.
pub struct RisLiveSource {
    base: SourceBase,
    config: RisLiveSourceConfig,
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    shutdown_tx: Arc<RwLock<Option<watch::Sender<bool>>>>,
}

impl RisLiveSource {
    /// Creates a new source with the given ID and config.
    pub fn new(id: impl Into<String>, config: RisLiveSourceConfig) -> Result<Self> {
        let id = id.into();
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            state_store: Arc::new(RwLock::new(None)),
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        })
    }

    /// Creates a source builder.
    pub fn builder(id: impl Into<String>) -> RisLiveSourceBuilder {
        RisLiveSourceBuilder::new(id)
    }
}

#[async_trait]
impl Source for RisLiveSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "ris-live"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut properties = HashMap::new();
        properties.insert(
            "websocket_url".to_string(),
            serde_json::Value::String(self.config.websocket_url.clone()),
        );
        if let Some(client_name) = &self.config.client_name {
            properties.insert(
                "client_name".to_string(),
                serde_json::Value::String(client_name.clone()),
            );
        }
        if let Some(host) = &self.config.host {
            properties.insert("host".to_string(), serde_json::Value::String(host.clone()));
        }
        if let Some(message_type) = &self.config.message_type {
            properties.insert(
                "message_type".to_string(),
                serde_json::Value::String(message_type.clone()),
            );
        }
        if let Some(prefixes) = &self.config.prefixes {
            properties.insert(
                "prefixes".to_string(),
                serde_json::Value::Array(
                    prefixes
                        .iter()
                        .map(|prefix| serde_json::Value::String(prefix.clone()))
                        .collect(),
                ),
            );
        }
        properties.insert(
            "include_peer_state".to_string(),
            serde_json::Value::Bool(self.config.include_peer_state),
        );
        properties.insert(
            "reconnect_delay_secs".to_string(),
            serde_json::Value::Number(self.config.reconnect_delay_secs.into()),
        );
        properties.insert(
            "clear_state_on_start".to_string(),
            serde_json::Value::Bool(self.config.clear_state_on_start),
        );
        properties
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base.set_status(ComponentStatus::Starting, Some("Starting RIS Live source".to_string())).await;

        let source_id = self.base.id.clone();
        let config = self.config.clone();
        let dispatchers = self.base.dispatchers.clone();
        let state_store = self.state_store.read().await.clone();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let source_id_for_span = source_id.clone();
        let instance_id = self
            .base
            .context()
            .await
            .map(|context| context.instance_id)
            .unwrap_or_default();
        let span = tracing::info_span!(
            "ris_live_stream_task",
            instance_id = %instance_id,
            component_id = %source_id_for_span,
            component_type = "source"
        );

        let task_handle = tokio::spawn(
            async move {
                if let Err(error) = stream::run_stream_loop(
                    source_id.clone(),
                    config,
                    dispatchers,
                    state_store,
                    shutdown_rx,
                )
                .await
                {
                    error!("[{source_id}] RIS stream loop failed: {error}");
                }
            }
            .instrument(span),
        );

        *self.shutdown_tx.write().await = Some(shutdown_tx);
        *self.task_handle.write().await = Some(task_handle);

        self.base.set_status(ComponentStatus::Running, Some("RIS Live source started".to_string())).await;
        info!("[{}] RIS Live source started", self.base.id);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Stopped {
            return Ok(());
        }

        self.base.set_status(ComponentStatus::Stopping, Some("Stopping RIS Live source".to_string())).await;

        if let Some(shutdown_tx) = self.shutdown_tx.write().await.take() {
            let _ = shutdown_tx.send(true);
        }

        if let Some(task_handle) = self.task_handle.write().await.take() {
            match timeout(Duration::from_secs(10), task_handle).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    error!("[{}] RIS Live task panicked: {error}", self.base.id);
                }
                Err(_) => {
                    error!(
                        "[{}] Timed out while waiting for RIS task to stop",
                        self.base.id
                    );
                }
            }
        }

        self.base.set_status(ComponentStatus::Stopped, Some("RIS Live source stopped".to_string())).await;

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
            .subscribe_with_bootstrap(&settings, "RIS Live")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context.clone()).await;
        if self.state_store.read().await.is_none() {
            if let Some(store) = context.state_store.as_ref() {
                *self.state_store.write().await = Some(store.clone());
            }
        }
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// Builder for [`RisLiveSource`].
pub struct RisLiveSourceBuilder {
    id: String,
    websocket_url: String,
    client_name: Option<String>,
    host: Option<String>,
    message_type: Option<String>,
    prefixes: Option<Vec<String>>,
    more_specific: Option<bool>,
    less_specific: Option<bool>,
    path: Option<String>,
    peer: Option<String>,
    require: Option<String>,
    include_peer_state: bool,
    reconnect_delay_secs: u64,
    clear_state_on_start: bool,
    start_from: StartFrom,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl RisLiveSourceBuilder {
    /// Creates a new builder.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            websocket_url: "wss://ris-live.ripe.net/v1/ws/".to_string(),
            client_name: None,
            host: None,
            message_type: None,
            prefixes: None,
            more_specific: None,
            less_specific: None,
            path: None,
            peer: None,
            require: None,
            include_peer_state: true,
            reconnect_delay_secs: 5,
            clear_state_on_start: false,
            start_from: StartFrom::Now,
            state_store: None,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    pub fn with_websocket_url(mut self, websocket_url: impl Into<String>) -> Self {
        self.websocket_url = websocket_url.into();
        self
    }

    pub fn with_client_name(mut self, client_name: impl Into<String>) -> Self {
        self.client_name = Some(client_name.into());
        self
    }

    pub fn with_optional_client_name(mut self, client_name: Option<String>) -> Self {
        self.client_name = client_name;
        self
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    pub fn with_optional_host(mut self, host: Option<String>) -> Self {
        self.host = host;
        self
    }

    pub fn with_message_type(mut self, message_type: impl Into<String>) -> Self {
        self.message_type = Some(message_type.into());
        self
    }

    pub fn with_optional_message_type(mut self, message_type: Option<String>) -> Self {
        self.message_type = message_type;
        self
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        let mut prefixes = self.prefixes.unwrap_or_default();
        prefixes.push(prefix.into());
        self.prefixes = Some(prefixes);
        self
    }

    pub fn with_prefixes(mut self, prefixes: Vec<String>) -> Self {
        self.prefixes = Some(prefixes);
        self
    }

    pub fn with_optional_prefixes(mut self, prefixes: Option<Vec<String>>) -> Self {
        self.prefixes = prefixes;
        self
    }

    pub fn with_more_specific(mut self, more_specific: bool) -> Self {
        self.more_specific = Some(more_specific);
        self
    }

    pub fn with_optional_more_specific(mut self, more_specific: Option<bool>) -> Self {
        self.more_specific = more_specific;
        self
    }

    pub fn with_less_specific(mut self, less_specific: bool) -> Self {
        self.less_specific = Some(less_specific);
        self
    }

    pub fn with_optional_less_specific(mut self, less_specific: Option<bool>) -> Self {
        self.less_specific = less_specific;
        self
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_optional_path(mut self, path: Option<String>) -> Self {
        self.path = path;
        self
    }

    pub fn with_peer(mut self, peer: impl Into<String>) -> Self {
        self.peer = Some(peer.into());
        self
    }

    pub fn with_optional_peer(mut self, peer: Option<String>) -> Self {
        self.peer = peer;
        self
    }

    pub fn with_require(mut self, require: impl Into<String>) -> Self {
        self.require = Some(require.into());
        self
    }

    pub fn with_optional_require(mut self, require: Option<String>) -> Self {
        self.require = require;
        self
    }

    pub fn with_include_peer_state(mut self, include_peer_state: bool) -> Self {
        self.include_peer_state = include_peer_state;
        self
    }

    pub fn with_reconnect_delay_secs(mut self, reconnect_delay_secs: u64) -> Self {
        self.reconnect_delay_secs = reconnect_delay_secs;
        self
    }

    pub fn with_clear_state_on_start(mut self, clear_state_on_start: bool) -> Self {
        self.clear_state_on_start = clear_state_on_start;
        self
    }

    pub fn with_start_from_beginning(mut self) -> Self {
        self.start_from = StartFrom::Beginning;
        self
    }

    pub fn with_start_from_now(mut self) -> Self {
        self.start_from = StartFrom::Now;
        self
    }

    pub fn with_start_from_timestamp(mut self, timestamp_ms: i64) -> Self {
        self.start_from = StartFrom::Timestamp { timestamp_ms };
        self
    }

    pub fn with_start_from(mut self, start_from: StartFrom) -> Self {
        self.start_from = start_from;
        self
    }

    pub fn with_state_store(mut self, state_store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(state_store);
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

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn build(self) -> Result<RisLiveSource> {
        let _ = url::Url::parse(&self.websocket_url)?;

        let config = RisLiveSourceConfig {
            websocket_url: self.websocket_url,
            client_name: self.client_name,
            host: self.host,
            message_type: self.message_type,
            prefixes: self.prefixes,
            more_specific: self.more_specific,
            less_specific: self.less_specific,
            path: self.path,
            peer: self.peer,
            require: self.require,
            include_peer_state: self.include_peer_state,
            reconnect_delay_secs: self.reconnect_delay_secs.max(1),
            clear_state_on_start: self.clear_state_on_start,
            start_from: self.start_from,
        };

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

        Ok(RisLiveSource {
            base: SourceBase::new(params)?,
            config,
            state_store: Arc::new(RwLock::new(self.state_store)),
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        })
    }
}

#[cfg(test)]
mod tests {
    use drasi_lib::Source;

    use super::{RisLiveSource, StartFrom};

    #[test]
    fn builder_sets_defaults() {
        let source = RisLiveSource::builder("test-source")
            .build()
            .expect("source should build");

        assert_eq!(source.id(), "test-source");
        assert_eq!(source.type_name(), "ris-live");
        assert_eq!(source.config.reconnect_delay_secs, 5);
        assert!(source.config.include_peer_state);
    }

    #[test]
    fn builder_sets_custom_values() {
        let source = RisLiveSource::builder("test-source")
            .with_websocket_url("wss://example.invalid/ws/")
            .with_host("rrc00")
            .with_message_type("UPDATE")
            .with_prefix("203.0.113.0/24")
            .with_reconnect_delay_secs(10)
            .with_clear_state_on_start(true)
            .with_start_from_timestamp(1_700_000_000_000)
            .build()
            .expect("source should build");

        assert_eq!(source.config.websocket_url, "wss://example.invalid/ws/");
        assert_eq!(source.config.host.as_deref(), Some("rrc00"));
        assert_eq!(source.config.message_type.as_deref(), Some("UPDATE"));
        assert_eq!(
            source.config.prefixes,
            Some(vec!["203.0.113.0/24".to_string()])
        );
        assert_eq!(source.config.reconnect_delay_secs, 10);
        assert!(source.config.clear_state_on_start);
        assert_eq!(
            source.config.start_from,
            StartFrom::Timestamp {
                timestamp_ms: 1_700_000_000_000,
            }
        );
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "ris-live-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::RisLiveSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
