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

//! Open511 source plugin for Drasi.
//!
//! This source polls Open511 `/events` endpoints and emits graph changes:
//! - `RoadEvent` nodes for events
//! - `Road` nodes plus `AFFECTS_ROAD` relationships
//! - `Area` nodes plus `IN_AREA` relationships
//!
//! The change detection strategy is hybrid:
//! - Most cycles use incremental polling (`updated=>timestamp`)
//! - Every Nth cycle runs a full sweep to detect removals

pub mod api;
pub mod change_detection;
pub mod config;
pub mod descriptor;
pub mod mapping;
pub mod models;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use drasi_lib::channels::*;
use drasi_lib::profiling::{timestamp_ns, ProfilingMetadata};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::Source;
use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, RwLock};
use tracing::Instrument;

use crate::api::Open511ApiClient;
use crate::change_detection::{detect_full_sweep, detect_incremental, PollState};
pub use crate::config::{InitialCursorBehavior, Open511SourceConfig};
use crate::mapping::area_node_id;
use crate::models::Open511Event;

const KEY_EVENTS: &str = "open511.events";
const KEY_SNAPSHOT: &str = "open511.snapshot";
const KEY_KNOWN_AREAS: &str = "open511.known_areas";
const KEY_LAST_POLL: &str = "open511.last_poll";
const KEY_POLL_COUNT: &str = "open511.poll_count";

/// Open511 polling source.
pub struct Open511Source {
    base: SourceBase,
    config: Open511SourceConfig,
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    configured_state_store: Option<Arc<dyn StateStoreProvider>>,
    shutdown_tx: Arc<RwLock<Option<watch::Sender<bool>>>>,
}

impl Open511Source {
    /// Create a new source from ID and config.
    pub fn new(id: impl Into<String>, config: Open511SourceConfig) -> Result<Self> {
        let params = SourceBaseParams::new(id.into());
        config.validate()?;

        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            state_store: Arc::new(RwLock::new(None)),
            configured_state_store: None,
            shutdown_tx: Arc::new(RwLock::new(None)),
        })
    }

    /// Create a builder for this source type.
    pub fn builder(id: impl Into<String>) -> Open511SourceBuilder {
        Open511SourceBuilder::new(id)
    }
}

#[async_trait]
impl Source for Open511Source {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "open511"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "base_url".to_string(),
            serde_json::Value::String(self.config.base_url.clone()),
        );
        props.insert(
            "poll_interval_secs".to_string(),
            serde_json::Value::Number(self.config.poll_interval_secs.into()),
        );
        props.insert(
            "full_sweep_interval".to_string(),
            serde_json::Value::Number(self.config.full_sweep_interval.into()),
        );
        props.insert(
            "request_timeout_secs".to_string(),
            serde_json::Value::Number(self.config.request_timeout_secs.into()),
        );
        props.insert(
            "page_size".to_string(),
            serde_json::Value::Number((self.config.page_size as u64).into()),
        );
        if let Some(ref status) = self.config.status_filter {
            props.insert(
                "status_filter".to_string(),
                serde_json::Value::String(status.clone()),
            );
        }
        if let Some(ref severity) = self.config.severity_filter {
            props.insert(
                "severity_filter".to_string(),
                serde_json::Value::String(severity.clone()),
            );
        }
        if let Some(ref event_type) = self.config.event_type_filter {
            props.insert(
                "event_type_filter".to_string(),
                serde_json::Value::String(event_type.clone()),
            );
        }
        props
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
                Some("Starting Open511 source".to_string()),
            )
            .await?;

        let source_id = self.base.id.clone();
        let config = self.config.clone();
        let dispatchers = self.base.dispatchers.clone();
        let state_store = self.state_store.read().await.clone();

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        let instance_id = self
            .base
            .context()
            .await
            .map(|ctx| ctx.instance_id)
            .unwrap_or_default();
        let span = tracing::info_span!(
            "open511_source_polling",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );

        let task = tokio::spawn(
            async move {
                if let Err(e) = run_poll_loop(
                    source_id.clone(),
                    config,
                    dispatchers,
                    state_store,
                    shutdown_rx,
                )
                .await
                {
                    error!("Open511 source task failed for '{source_id}': {e}");
                }
            }
            .instrument(span),
        );

        *self.base.task_handle.write().await = Some(task);

        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Open511 source running".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base
            .set_status_with_event(
                ComponentStatus::Stopping,
                Some("Stopping Open511 source".to_string()),
            )
            .await?;

        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(true);
        }

        if let Some(handle) = self.base.task_handle.write().await.take() {
            match tokio::time::timeout(Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("Open511 polling task join failed: {e}"),
                Err(_) => warn!("Timed out waiting for Open511 polling task to stop"),
            }
        }

        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Open511 source stopped".to_string()),
            )
            .await?;

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
            .subscribe_with_bootstrap(&settings, "Open511")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn deprovision(&self) -> Result<()> {
        self.base.deprovision_common().await
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context.clone()).await;

        if let Some(state_store) = context.state_store {
            *self.state_store.write().await = Some(state_store);
            return;
        }

        if let Some(ref configured) = self.configured_state_store {
            *self.state_store.write().await = Some(configured.clone());
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
    source_id: String,
    config: Open511SourceConfig,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let api_client = Open511ApiClient::new(config.clone())?;
    let mut state = load_poll_state(&source_id, &state_store).await?;

    initialize_poll_state(&source_id, &config, &api_client, &mut state).await?;

    let mut interval = tokio::time::interval(Duration::from_secs(config.poll_interval_secs));

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("Open511 source '{source_id}' received shutdown signal");
                    break;
                }
            }
            _ = interval.tick() => {
                state.poll_count = state.poll_count.wrapping_add(1);
                let is_full_sweep = should_run_full_sweep(&state, config.full_sweep_interval);
                let updated_since = if is_full_sweep {
                    None
                } else {
                    state.last_poll_time.as_deref()
                };

                let events = match api_client.fetch_all_events(updated_since).await {
                    Ok(events) => events,
                    Err(e) => {
                        warn!(
                            "Open511 source '{source_id}' polling failed (full_sweep={is_full_sweep}): {e}"
                        );
                        continue;
                    }
                };

                let cycle = if is_full_sweep {
                    detect_full_sweep(&state, events, &source_id, config.auto_delete_archived)
                } else {
                    detect_incremental(&state, events, &source_id, config.auto_delete_archived)
                };

                let change_count = cycle.changes.len();
                state = cycle.next_state;
                state.last_poll_time = Some(format_api_timestamp(&Utc::now()));

                let mut dispatch_failures = 0usize;
                for change in cycle.changes {
                    if let Err(e) = dispatch_change(dispatchers.clone(), &source_id, change).await {
                        dispatch_failures = dispatch_failures.saturating_add(1);
                        warn!("Open511 source '{source_id}' failed to dispatch change: {e}");
                    }
                }

                if dispatch_failures > 0 {
                    warn!(
                        "Open511 source '{source_id}' encountered {dispatch_failures} dispatch failures in poll cycle"
                    );
                }

                if let Err(e) = save_poll_state(&source_id, &state_store, &state).await {
                    warn!("Open511 source '{source_id}' failed to persist poll state: {e}");
                }

                debug!(
                    "Open511 source '{source_id}' processed {change_count} changes (full_sweep={is_full_sweep})"
                );
            }
        }
    }

    Ok(())
}

fn should_run_full_sweep(state: &PollState, full_sweep_interval: u32) -> bool {
    (state.events_by_id.is_empty() && state.last_poll_time.is_none())
        || state.poll_count % full_sweep_interval == 0
}

async fn dispatch_change(
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    source_id: &str,
    change: drasi_core::models::SourceChange,
) -> Result<()> {
    let mut profiling = ProfilingMetadata::new();
    profiling.source_send_ns = Some(timestamp_ns());

    let wrapper = SourceEventWrapper::with_profiling(
        source_id.to_string(),
        SourceEvent::Change(change),
        Utc::now(),
        profiling,
    );

    SourceBase::dispatch_from_task(dispatchers, wrapper, source_id).await
}

async fn initialize_poll_state(
    source_id: &str,
    config: &Open511SourceConfig,
    api_client: &Open511ApiClient,
    state: &mut PollState,
) -> Result<()> {
    if !state.events_by_id.is_empty() || state.last_poll_time.is_some() {
        return Ok(());
    }

    match config.initial_cursor_behavior {
        InitialCursorBehavior::StartFromBeginning => {
            info!("Open511 source '{source_id}' starting from beginning (first full sweep emits inserts)");
        }
        InitialCursorBehavior::StartFromNow => {
            info!("Open511 source '{source_id}' seeding current snapshot without emitting changes");
            let events = api_client.fetch_all_events(None).await?;
            state.events_by_id = events
                .into_iter()
                .map(|event| (event.id.clone(), event))
                .collect();
            // Keep known_areas empty so first post-start updates can emit required Area nodes.
            state.known_areas.clear();
            state.last_poll_time = latest_event_timestamp(state.events_by_id.values())
                .or_else(|| Some(format_api_timestamp(&Utc::now())));
        }
        InitialCursorBehavior::StartFromTimestamp { timestamp_millis } => {
            let cursor =
                DateTime::<Utc>::from_timestamp_millis(timestamp_millis).ok_or_else(|| {
                    anyhow::anyhow!("Invalid start_from_timestamp value '{timestamp_millis}'")
                })?;
            info!(
                "Open511 source '{source_id}' starting incremental mode from timestamp {}",
                format_api_timestamp(&cursor)
            );
            state.last_poll_time = Some(format_api_timestamp(&cursor));
        }
    }

    Ok(())
}

/// Format a UTC DateTime as clean ISO-8601 with second precision and `Z` suffix.
///
/// Produces `2026-03-08T02:33:35Z` — no subseconds, no `+00:00`.
/// This format is required by the DriveBC Open511 API for the `updated` filter.
fn format_api_timestamp(dt: &DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn derive_known_areas(events_by_id: &HashMap<String, Open511Event>) -> HashSet<String> {
    let mut known_areas = HashSet::new();
    for event in events_by_id.values() {
        if let Some(areas) = &event.areas {
            for (index, area) in areas.iter().enumerate() {
                known_areas.insert(area_node_id(event, area, index));
            }
        }
    }
    known_areas
}

fn latest_event_timestamp<'a, I>(events: I) -> Option<String>
where
    I: IntoIterator<Item = &'a Open511Event>,
{
    events
        .into_iter()
        .filter_map(|event| event.updated_or_created())
        .filter_map(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
        .max()
        .map(|timestamp| format_api_timestamp(&timestamp.with_timezone(&Utc)))
}

async fn load_poll_state(
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
) -> Result<PollState> {
    let Some(store) = state_store else {
        return Ok(PollState::default());
    };

    let mut state = PollState::default();

    if let Some(bytes) = store.get(source_id, KEY_EVENTS).await? {
        match serde_json::from_slice::<HashMap<String, Open511Event>>(&bytes) {
            Ok(events) => state.events_by_id = events,
            Err(e) => warn!("Failed to deserialize '{KEY_EVENTS}': {e}"),
        }
    }

    if let Some(bytes) = store.get(source_id, KEY_KNOWN_AREAS).await? {
        match serde_json::from_slice::<HashSet<String>>(&bytes) {
            Ok(known_areas) => state.known_areas = known_areas,
            Err(e) => warn!("Failed to deserialize '{KEY_KNOWN_AREAS}': {e}"),
        }
    }

    if let Some(bytes) = store.get(source_id, KEY_LAST_POLL).await? {
        match String::from_utf8(bytes) {
            Ok(last_poll) if !last_poll.is_empty() => state.last_poll_time = Some(last_poll),
            Ok(_) => {}
            Err(e) => warn!("Failed to deserialize '{KEY_LAST_POLL}': {e}"),
        }
    }

    if let Some(bytes) = store.get(source_id, KEY_POLL_COUNT).await? {
        if let Ok(count_str) = String::from_utf8(bytes) {
            if let Ok(count) = count_str.parse::<u32>() {
                state.poll_count = count;
            }
        }
    }

    Ok(state)
}

async fn save_poll_state(
    source_id: &str,
    state_store: &Option<Arc<dyn StateStoreProvider>>,
    state: &PollState,
) -> Result<()> {
    let Some(store) = state_store else {
        return Ok(());
    };

    let events_bytes = serde_json::to_vec(&state.events_by_id)?;
    store.set(source_id, KEY_EVENTS, events_bytes).await?;

    let known_areas_bytes = serde_json::to_vec(&state.known_areas)?;
    store
        .set(source_id, KEY_KNOWN_AREAS, known_areas_bytes)
        .await?;

    let snapshot: HashMap<String, String> = state
        .events_by_id
        .iter()
        .map(|(id, event)| {
            (
                id.clone(),
                event.updated_or_created().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let snapshot_bytes = serde_json::to_vec(&snapshot)?;
    store.set(source_id, KEY_SNAPSHOT, snapshot_bytes).await?;

    let last_poll = state.last_poll_time.clone().unwrap_or_default();
    store
        .set(source_id, KEY_LAST_POLL, last_poll.into_bytes())
        .await?;

    store
        .set(
            source_id,
            KEY_POLL_COUNT,
            state.poll_count.to_string().into_bytes(),
        )
        .await?;

    Ok(())
}

/// Builder for [`Open511Source`].
pub struct Open511SourceBuilder {
    id: String,
    config: Open511SourceConfig,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
    state_store: Option<Arc<dyn StateStoreProvider>>,
}

impl Open511SourceBuilder {
    /// Create builder with defaults.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: Open511SourceConfig::default(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
            state_store: None,
        }
    }

    /// Replace full config.
    pub fn with_config(mut self, config: Open511SourceConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.config.base_url = base_url.into();
        self
    }

    pub fn with_poll_interval_secs(mut self, poll_interval_secs: u64) -> Self {
        self.config.poll_interval_secs = poll_interval_secs;
        self
    }

    pub fn with_full_sweep_interval(mut self, full_sweep_interval: u32) -> Self {
        self.config.full_sweep_interval = full_sweep_interval;
        self
    }

    pub fn with_request_timeout_secs(mut self, request_timeout_secs: u64) -> Self {
        self.config.request_timeout_secs = request_timeout_secs;
        self
    }

    pub fn with_page_size(mut self, page_size: usize) -> Self {
        self.config.page_size = page_size;
        self
    }

    pub fn with_status_filter(mut self, status: impl Into<String>) -> Self {
        self.config.status_filter = Some(status.into());
        self
    }

    pub fn with_severity_filter(mut self, severity: impl Into<String>) -> Self {
        self.config.severity_filter = Some(severity.into());
        self
    }

    pub fn with_event_type_filter(mut self, event_type: impl Into<String>) -> Self {
        self.config.event_type_filter = Some(event_type.into());
        self
    }

    pub fn with_area_id_filter(mut self, area_id: impl Into<String>) -> Self {
        self.config.area_id_filter = Some(area_id.into());
        self
    }

    pub fn with_road_name_filter(mut self, road_name: impl Into<String>) -> Self {
        self.config.road_name_filter = Some(road_name.into());
        self
    }

    pub fn with_jurisdiction_filter(mut self, jurisdiction: impl Into<String>) -> Self {
        self.config.jurisdiction_filter = Some(jurisdiction.into());
        self
    }

    pub fn with_bbox_filter(mut self, bbox: impl Into<String>) -> Self {
        self.config.bbox_filter = Some(bbox.into());
        self
    }

    /// When enabled, events that transition to `ARCHIVED` status are automatically
    /// deleted from the graph instead of being treated as updates.
    pub fn with_auto_delete_archived(mut self, enabled: bool) -> Self {
        self.config.auto_delete_archived = enabled;
        self
    }

    /// Initial cursor: start from beginning.
    pub fn with_start_from_beginning(mut self) -> Self {
        self.config.initial_cursor_behavior = InitialCursorBehavior::StartFromBeginning;
        self
    }

    /// Initial cursor: seed from current state, then only future changes.
    pub fn with_start_from_now(mut self) -> Self {
        self.config.initial_cursor_behavior = InitialCursorBehavior::StartFromNow;
        self
    }

    /// Initial cursor: incremental polling from given timestamp (milliseconds).
    pub fn with_start_from_timestamp(mut self, timestamp_millis: i64) -> Self {
        self.config.initial_cursor_behavior =
            InitialCursorBehavior::StartFromTimestamp { timestamp_millis };
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

    /// Configure an explicit state store provider for standalone usage.
    pub fn with_state_store(mut self, state_store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(state_store);
        self
    }

    pub fn build(self) -> Result<Open511Source> {
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

        Ok(Open511Source {
            base: SourceBase::new(params)?,
            config: self.config,
            state_store: Arc::new(RwLock::new(self.state_store.clone())),
            configured_state_store: self.state_store,
            shutdown_tx: Arc::new(RwLock::new(None)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Open511Area, Open511Event};
    use std::collections::HashMap;

    #[test]
    fn builder_configures_defaults() {
        let source = Open511Source::builder("open511-source")
            .with_base_url("https://api.open511.gov.bc.ca")
            .build()
            .expect("source build should succeed");

        assert_eq!(source.id(), "open511-source");
        assert_eq!(source.config.status_filter, Some("ACTIVE".to_string()));
        assert_eq!(source.config.full_sweep_interval, 10);
    }

    #[test]
    fn builder_applies_start_cursor_settings() {
        let source = Open511Source::builder("open511-source")
            .with_base_url("https://api.open511.gov.bc.ca")
            .with_start_from_timestamp(1_700_000_000_000)
            .build()
            .expect("source build should succeed");

        assert_eq!(
            source.config.initial_cursor_behavior,
            InitialCursorBehavior::StartFromTimestamp {
                timestamp_millis: 1_700_000_000_000
            }
        );
    }

    #[test]
    fn full_sweep_requires_empty_state_without_cursor() {
        let state = PollState {
            poll_count: 1,
            ..Default::default()
        };
        assert!(should_run_full_sweep(&state, 10));

        let state_with_cursor = PollState {
            poll_count: 1,
            last_poll_time: Some("2026-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        assert!(!should_run_full_sweep(&state_with_cursor, 10));

        let periodic_full_sweep = PollState {
            poll_count: 10,
            last_poll_time: Some("2026-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        assert!(should_run_full_sweep(&periodic_full_sweep, 10));
    }

    #[test]
    fn derive_known_areas_uses_fallback_area_ids() {
        let mut events_by_id = HashMap::new();
        events_by_id.insert(
            "event-1".to_string(),
            Open511Event {
                id: "event-1".to_string(),
                status: "ACTIVE".to_string(),
                areas: Some(vec![Open511Area {
                    id: None,
                    name: Some("Unnamed Area".to_string()),
                    url: None,
                }]),
                ..Default::default()
            },
        );

        let known_areas = derive_known_areas(&events_by_id);
        assert!(known_areas.contains("event-1::area::0"));
    }

    #[test]
    fn latest_event_timestamp_uses_most_recent_event_time() {
        let events = vec![
            Open511Event {
                id: "event-1".to_string(),
                status: "ACTIVE".to_string(),
                updated: Some("2026-01-01T00:00:00Z".to_string()),
                ..Default::default()
            },
            Open511Event {
                id: "event-2".to_string(),
                status: "ACTIVE".to_string(),
                updated: Some("2026-01-02T00:00:00-07:00".to_string()),
                ..Default::default()
            },
        ];

        let latest = latest_event_timestamp(events.iter()).expect("latest timestamp");
        assert_eq!(latest, "2026-01-02T07:00:00Z");
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "open511-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::Open511SourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
