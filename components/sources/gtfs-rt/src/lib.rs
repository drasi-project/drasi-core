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

pub mod config;
pub mod descriptor;
pub mod mapping;
pub mod poller;

pub use config::{GtfsRtFeedType, GtfsRtSourceConfig, InitialCursorMode};

use crate::poller::GtfsRtPoller;
use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::{
    ComponentStatus, DispatchMode, SourceEvent, SourceEventWrapper, SubscriptionResponse,
};
use drasi_lib::sources::{Source, SourceBase, SourceBaseParams};
use drasi_lib::state_store::StateStoreProvider;
use log::{info, warn};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::Instrument;

/// GTFS-RT source implementation.
pub struct GtfsRtSource {
    base: SourceBase,
    config: GtfsRtSourceConfig,
    client: Client,
}

#[async_trait]
impl Source for GtfsRtSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "gtfs-rt"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut properties = HashMap::new();
        properties.insert(
            "trip_updates_url".to_string(),
            self.config
                .trip_updates_url
                .clone()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        properties.insert(
            "vehicle_positions_url".to_string(),
            self.config
                .vehicle_positions_url
                .clone()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        properties.insert(
            "alerts_url".to_string(),
            self.config
                .alerts_url
                .clone()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        properties.insert(
            "poll_interval_secs".to_string(),
            serde_json::Value::Number(self.config.poll_interval_secs.into()),
        );
        properties.insert(
            "timeout_secs".to_string(),
            serde_json::Value::Number(self.config.timeout_secs.into()),
        );
        properties.insert(
            "language".to_string(),
            serde_json::Value::String(self.config.language.clone()),
        );
        properties.insert(
            "header_keys".to_string(),
            serde_json::Value::Array(
                self.config
                    .headers
                    .keys()
                    .map(|key| serde_json::Value::String(key.clone()))
                    .collect(),
            ),
        );
        properties.insert(
            "initial_cursor_mode".to_string(),
            serde_json::Value::String(match self.config.initial_cursor_mode {
                InitialCursorMode::StartFromBeginning => "start_from_beginning".to_string(),
                InitialCursorMode::StartFromNow => "start_from_now".to_string(),
                InitialCursorMode::StartFromTimestamp(ts) => {
                    format!("start_from_timestamp:{ts}")
                }
            }),
        );

        properties
    }

    fn dispatch_mode(&self) -> DispatchMode {
        self.base.get_dispatch_mode()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting GTFS-RT source".to_string()),
            )
            .await?;

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let config = self.config.clone();
        let source_id = self.base.id.clone();
        let dispatchers = self.base.dispatchers.clone();
        let state_store = self.base.state_store().await;
        let client = self.client.clone();

        let instance_id = self
            .base
            .context()
            .await
            .map(|context| context.instance_id)
            .unwrap_or_default();

        let source_id_for_span = source_id.clone();
        let span = tracing::info_span!(
            "gtfs_rt_source_task",
            instance_id = %instance_id,
            component_id = %source_id_for_span,
            component_type = "source"
        );

        let task = tokio::spawn(
            async move {
                let mut poller = GtfsRtPoller::new(config.clone(), client);

                if should_baseline_on_startup(state_store.as_ref(), &source_id, &config).await {
                    poller.set_initial_mode(InitialCursorMode::StartFromNow);
                }

                let mut interval =
                    tokio::time::interval(Duration::from_secs(config.poll_interval_secs));
                // Consume the immediate tick so first poll happens after one full interval.
                interval.tick().await;

                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => {
                            info!("GTFS-RT polling task for '{source_id}' received shutdown");
                            break;
                        }
                        _ = interval.tick() => {
                            match poller.poll_once(&source_id).await {
                                Ok(result) => {
                                    for change in result.changes {
                                        let wrapper = SourceEventWrapper::new(
                                            source_id.clone(),
                                            SourceEvent::Change(change),
                                            chrono::Utc::now(),
                                        );

                                        if let Err(err) = SourceBase::dispatch_from_task(
                                            dispatchers.clone(),
                                            wrapper,
                                            &source_id,
                                        )
                                        .await
                                        {
                                            warn!("Failed to dispatch GTFS-RT change: {err}");
                                        }
                                    }

                                    if let Err(err) = persist_feed_cursors(
                                        state_store.as_ref(),
                                        &source_id,
                                        &result.changed_feeds,
                                    )
                                    .await
                                    {
                                        warn!("Failed to persist GTFS-RT cursor state: {err}");
                                    }
                                }
                                Err(err) => {
                                    warn!("GTFS-RT poll failed for source '{source_id}': {err}");
                                }
                            }
                        }
                    }
                }

                info!("GTFS-RT polling task exited for source '{source_id}'");
            }
            .instrument(span),
        );

        self.base.set_task_handle(task).await;
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("GTFS-RT source started".to_string()),
            )
            .await
    }

    async fn stop(&self) -> Result<()> {
        self.base
            .set_status_with_event(
                ComponentStatus::Stopping,
                Some("Stopping GTFS-RT source".to_string()),
            )
            .await?;

        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "GTFS-RT")
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

async fn should_baseline_on_startup(
    state_store: Option<&Arc<dyn StateStoreProvider>>,
    source_id: &str,
    config: &GtfsRtSourceConfig,
) -> bool {
    let Some(store) = state_store else {
        return false;
    };

    let configured = config.configured_feeds();
    if configured.is_empty() {
        return false;
    }

    // Only baseline if ALL configured feeds have persisted cursors.
    // This avoids silently skipping initial data for newly-added feed URLs.
    let mut all_have_cursors = true;
    for (feed_type, _) in &configured {
        let key = cursor_key(*feed_type);
        match store.get(source_id, &key).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                all_have_cursors = false;
                break;
            }
            Err(err) => {
                warn!(
                    "Failed to read persisted cursor for source '{source_id}' and key '{key}': {err}"
                );
                all_have_cursors = false;
                break;
            }
        }
    }

    all_have_cursors
}

async fn persist_feed_cursors(
    state_store: Option<&Arc<dyn StateStoreProvider>>,
    source_id: &str,
    feed_types: &[GtfsRtFeedType],
) -> Result<()> {
    let Some(store) = state_store else {
        return Ok(());
    };

    for feed_type in feed_types {
        let key = cursor_key(*feed_type);
        // Store a presence marker so we know this feed was successfully polled.
        store.set(source_id, &key, vec![1u8]).await.map_err(|err| {
            anyhow::anyhow!(
                "failed to persist cursor for source '{source_id}' and key '{key}': {err}"
            )
        })?;
    }

    Ok(())
}

fn cursor_key(feed_type: GtfsRtFeedType) -> String {
    format!("feed_poll_marker:{}", feed_type.key())
}

/// Fluent builder for `GtfsRtSource`.
pub struct GtfsRtSourceBuilder {
    id: String,
    trip_updates_url: Option<String>,
    vehicle_positions_url: Option<String>,
    alerts_url: Option<String>,
    poll_interval_secs: u64,
    headers: HashMap<String, String>,
    timeout_secs: u64,
    language: String,
    initial_cursor_mode: InitialCursorMode,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl GtfsRtSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            trip_updates_url: None,
            vehicle_positions_url: None,
            alerts_url: None,
            poll_interval_secs: 30,
            headers: HashMap::new(),
            timeout_secs: 15,
            language: "en".to_string(),
            initial_cursor_mode: InitialCursorMode::StartFromBeginning,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    pub fn with_trip_updates_url(mut self, url: impl Into<String>) -> Self {
        self.trip_updates_url = Some(url.into());
        self
    }

    pub fn with_vehicle_positions_url(mut self, url: impl Into<String>) -> Self {
        self.vehicle_positions_url = Some(url.into());
        self
    }

    pub fn with_alerts_url(mut self, url: impl Into<String>) -> Self {
        self.alerts_url = Some(url.into());
        self
    }

    pub fn with_poll_interval_secs(mut self, poll_interval_secs: u64) -> Self {
        self.poll_interval_secs = poll_interval_secs;
        self
    }

    pub fn with_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }

    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(key.into(), value.into());
        self
    }

    pub fn with_timeout_secs(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = language.into();
        self
    }

    pub fn with_start_from_now(mut self) -> Self {
        self.initial_cursor_mode = InitialCursorMode::StartFromNow;
        self
    }

    pub fn with_start_from_timestamp_ms(mut self, timestamp_ms: i64) -> Self {
        self.initial_cursor_mode = InitialCursorMode::StartFromTimestamp(timestamp_ms);
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

    pub fn with_config(mut self, config: GtfsRtSourceConfig) -> Self {
        self.trip_updates_url = config.trip_updates_url;
        self.vehicle_positions_url = config.vehicle_positions_url;
        self.alerts_url = config.alerts_url;
        self.poll_interval_secs = config.poll_interval_secs;
        self.headers = config.headers;
        self.timeout_secs = config.timeout_secs;
        self.language = config.language;
        self.initial_cursor_mode = config.initial_cursor_mode;
        self
    }

    pub fn build(self) -> Result<GtfsRtSource> {
        let config = GtfsRtSourceConfig {
            trip_updates_url: self.trip_updates_url,
            vehicle_positions_url: self.vehicle_positions_url,
            alerts_url: self.alerts_url,
            poll_interval_secs: self.poll_interval_secs,
            headers: self.headers,
            timeout_secs: self.timeout_secs,
            language: self.language,
            initial_cursor_mode: self.initial_cursor_mode,
        };

        config.validate()?;

        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()?;

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

        Ok(GtfsRtSource {
            base: SourceBase::new(params)?,
            config,
            client,
        })
    }
}

impl GtfsRtSource {
    pub fn builder(id: impl Into<String>) -> GtfsRtSourceBuilder {
        GtfsRtSourceBuilder::new(id)
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "gtfs-rt-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::GtfsRtSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
