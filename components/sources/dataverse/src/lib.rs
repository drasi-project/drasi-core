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

//! Dataverse Source Plugin for Drasi
//!
//! This plugin monitors Microsoft Dataverse tables for changes using OData
//! change tracking, which is the Web API equivalent of the platform's
//! `RetrieveEntityChangesRequest`. It supports:
//!
//! - **Polling-based change detection** using delta links (equivalent to `DataVersion`/`DataToken`)
//! - **Adaptive backoff** matching the platform's SyncWorker pattern
//! - **Per-entity workers** each tracking their own delta token
//! - **OAuth2 authentication** via Azure AD / Microsoft Entra ID client credentials
//!
//! # Architecture Alignment with Platform Source
//!
//! This Rust/Web API implementation mirrors the platform's C# Dataverse source:
//!
//! | Platform (C#)                           | Drasi-Core (Rust)                         |
//! |-----------------------------------------|-------------------------------------------|
//! | `RetrieveEntityChangesRequest`          | OData `Prefer: odata.track-changes`       |
//! | `DataVersion` / `DataToken`             | Delta token in `@odata.deltaLink`         |
//! | `NewOrUpdatedItem`                      | Record without `$deletedEntity` context   |
//! | `RemovedOrDeletedItem`                  | Record with `$deletedEntity` in context   |
//! | `SyncWorker` (per-entity)               | Per-entity `tokio::spawn` task            |
//! | `{entity}-deltatoken` state key         | Same state key format                     |
//! | `ServiceClient`                         | `reqwest` HTTP client                     |
//! | Adaptive backoff (500ms → scaled max) | Same adaptive backoff pattern             |
//!
//! # Configuration
//!
//! | Field                  | Type                      | Default   | Description                              |
//! |------------------------|---------------------------|-----------|------------------------------------------|
//! | `environment_url`      | String                    | required  | Dataverse environment URL                |
//! | `tenant_id`            | String                    | required  | Azure AD tenant ID                       |
//! | `client_id`            | String                    | required  | Azure AD application ID                  |
//! | `client_secret`        | String                    | required  | Azure AD client secret                   |
//! | `entities`             | Vec\<String\>             | required  | Entity logical names to monitor          |
//! | `entity_set_overrides` | HashMap\<String, String\> | `{}`      | Override entity set name mapping         |
//! | `entity_columns`       | HashMap\<String, Vec...\> | `{}`      | Per-entity column selection              |
//! | `min_interval_ms`      | u64                       | `500`     | Minimum adaptive interval                |
//! | `max_interval_seconds` | u64                       | `30`      | Per-entity max interval (sqrt-scaled by entity count) |
//! | `api_version`          | String                    | `"v9.2"`  | Web API version                          |
//!
//! # Usage
//!
//! ```rust,ignore
//! use drasi_source_dataverse::DataverseSource;
//!
//! let source = DataverseSource::builder("dv-source")
//!     .with_environment_url("https://myorg.crm.dynamics.com")
//!     .with_tenant_id("00000000-0000-0000-0000-000000000001")
//!     .with_client_id("00000000-0000-0000-0000-000000000002")
//!     .with_client_secret("my-client-secret")
//!     .with_entities(vec!["account".to_string(), "contact".to_string()])
//!     .build()?;
//! ```

pub mod auth;
pub mod client;
pub mod config;
pub mod descriptor;
pub mod types;

pub use config::DataverseSourceConfig;

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::channels::{ComponentStatus, DispatchMode, SourceEvent, SourceEventWrapper};
use drasi_lib::identity::IdentityProvider;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::Source;
use tracing::Instrument;

use crate::auth::TokenManager;
use crate::client::DataverseClient;
use crate::types::{parse_delta_changes, DataverseChange};

/// Dataverse source plugin for polling-based change detection.
///
/// Uses OData change tracking (Web API equivalent of `RetrieveEntityChangesRequest`)
/// to detect inserts, updates, and deletes in Microsoft Dataverse tables.
///
/// # Fields
///
/// - `base`: Common source functionality (dispatchers, status, lifecycle)
/// - `config`: Dataverse-specific configuration (connection, entities, polling)
pub struct DataverseSource {
    /// Base source implementation providing common functionality.
    base: SourceBase,
    /// Dataverse source configuration.
    config: DataverseSourceConfig,
    /// Optional identity provider for token acquisition.
    /// When set, takes precedence over config-based client credentials / Azure CLI.
    identity_provider: Option<Box<dyn IdentityProvider>>,
}

impl DataverseSource {
    /// Create a new Dataverse source.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    /// * `config` - Dataverse source configuration
    ///
    /// # Returns
    ///
    /// A new `DataverseSource` instance, or an error if construction fails.
    pub fn new(id: impl Into<String>, config: DataverseSourceConfig) -> Result<Self> {
        config.validate().map_err(|e| anyhow::anyhow!(e))?;
        let params = SourceBaseParams::new(id.into());
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            identity_provider: None,
        })
    }

    /// Create a builder for `DataverseSource`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    pub fn builder(id: impl Into<String>) -> DataverseSourceBuilder {
        DataverseSourceBuilder::new(id)
    }

    /// Run the polling loop for a single entity.
    ///
    /// This is the Rust equivalent of the platform's `SyncWorker.ExecuteAsync()`.
    /// It implements the same adaptive backoff pattern:
    /// - Starts at `min_interval_ms` (500ms default)
    /// - Slow backoff (1.2x) under 5s threshold
    /// - Fast backoff (1.5x) above 5s threshold
    /// - Resets to minimum on any detected changes
    #[allow(clippy::too_many_arguments)]
    async fn run_entity_worker(
        source_id: String,
        entity_name: String,
        entity_set_name: String,
        select: Option<String>,
        client: Arc<DataverseClient>,
        dispatchers: Arc<
            tokio::sync::RwLock<
                Vec<
                    Box<
                        dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync,
                    >,
                >,
            >,
        >,
        state_store: Option<Arc<dyn drasi_lib::StateStoreProvider>>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
        min_interval_ms: u64,
        max_interval_seconds: u64,
    ) {
        const THRESHOLD_MS: u64 = 5000;
        const SLOW_BACKOFF: f64 = 1.2;
        const FAST_BACKOFF: f64 = 1.5;

        let mut current_interval_ms = min_interval_ms;
        let max_interval_ms = max_interval_seconds * 1000;

        // Load last delta token from state store (like platform's checkpoint resume)
        let state_key = format!("{entity_name}-deltatoken");
        let mut delta_link: Option<String> = None;

        if let Some(ref store) = state_store {
            match store.get(&source_id, &state_key).await {
                Ok(Some(bytes)) => {
                    if let Ok(token) = String::from_utf8(bytes) {
                        log::info!("[{source_id}] Resuming from checkpoint for {entity_name}");
                        delta_link = Some(token);
                    }
                }
                Ok(None) => {
                    log::info!("[{source_id}] No checkpoint found for {entity_name}, getting current delta token");
                }
                Err(e) => {
                    log::warn!("[{source_id}] Failed to load delta token for {entity_name}: {e}");
                }
            }
        }

        // If no delta token, get the initial one by requesting change tracking
        // This matches the platform's GetCurrentDeltaToken() behavior:
        // page through all existing data to get the latest token
        if delta_link.is_none() {
            match Self::get_initial_delta_token(&client, &entity_set_name, select.as_deref()).await
            {
                Ok(token) => {
                    log::info!("[{source_id}] Initial delta token obtained for {entity_name}");
                    delta_link = Some(token);
                    // Save the initial token
                    if let Some(ref store) = state_store {
                        if let Some(ref dl) = delta_link {
                            let _ = store
                                .set(&source_id, &state_key, dl.as_bytes().to_vec())
                                .await;
                        }
                    }
                }
                Err(e) => {
                    log::error!(
                        "[{source_id}] Failed to get initial delta token for {entity_name}: {e}"
                    );
                    return;
                }
            }
        }

        // Main polling loop (mirrors platform's SyncWorker while loop)
        loop {
            // Check for shutdown signal
            if *shutdown_rx.borrow() {
                log::info!("[{source_id}] Shutting down entity worker for {entity_name}");
                break;
            }

            log::debug!(
                "[{source_id}] Polling for changes in entity: {entity_name} (interval: {current_interval_ms}ms)"
            );

            match Self::poll_for_changes(
                &source_id,
                &entity_name,
                &client,
                delta_link.as_deref(),
                &dispatchers,
            )
            .await
            {
                Ok((new_delta_link, change_count)) => {
                    if change_count > 0 {
                        // Changes detected - reset to minimum interval (responsive polling)
                        current_interval_ms = min_interval_ms;
                        log::info!(
                            "[{source_id}] Got {change_count} changes for entity {entity_name}"
                        );
                    } else {
                        // No changes - two-phase multiplicative backoff
                        let multiplier = if current_interval_ms < THRESHOLD_MS {
                            SLOW_BACKOFF
                        } else {
                            FAST_BACKOFF
                        };
                        current_interval_ms =
                            ((current_interval_ms as f64 * multiplier) as u64).min(max_interval_ms);
                    }

                    // Save the new delta token (like platform's state store Put)
                    if let Some(ref dl) = new_delta_link {
                        delta_link = Some(dl.clone());
                        if let Some(ref store) = state_store {
                            let _ = store
                                .set(&source_id, &state_key, dl.as_bytes().to_vec())
                                .await;
                        }
                    }

                    if change_count > 0 {
                        // Skip delay to poll immediately after processing changes
                        // (matching platform behavior)
                        continue;
                    }
                }
                Err(e) => {
                    log::error!("[{source_id}] Error polling entity {entity_name}: {e}");
                    // On error, wait 5 seconds before retrying (matching platform)
                    current_interval_ms = 5000;
                }
            }

            // Wait for the current interval, with shutdown check
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(current_interval_ms)) => {}
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        log::info!("[{source_id}] Shutting down entity worker for {entity_name}");
                        break;
                    }
                }
            }
        }
    }

    /// Get the initial delta token by performing the first change tracking request.
    ///
    /// Mirrors the platform's `GetCurrentDeltaToken()` which pages through all
    /// existing data to get the latest DataToken.
    async fn get_initial_delta_token(
        client: &DataverseClient,
        entity_set_name: &str,
        select: Option<&str>,
    ) -> Result<String> {
        let mut response = client
            .initial_change_tracking(entity_set_name, select)
            .await?;

        // Page through all data to get to the end and get the latest token
        // (matching platform's while loop in GetCurrentDeltaToken)
        while response.next_link.is_some() {
            let next = response
                .next_link
                .as_ref()
                .expect("next_link checked above");
            response = client.follow_next_link(next).await?;
        }

        response.delta_link.ok_or_else(|| {
            anyhow::anyhow!("No delta link returned from initial change tracking request")
        })
    }

    /// Poll for changes using the delta link.
    ///
    /// Returns the new delta link and the number of changes processed.
    /// Mirrors the platform's `GetChanges(deltaToken)` method.
    async fn poll_for_changes(
        source_id: &str,
        entity_name: &str,
        client: &DataverseClient,
        delta_link: Option<&str>,
        dispatchers: &Arc<
            tokio::sync::RwLock<
                Vec<
                    Box<
                        dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync,
                    >,
                >,
            >,
        >,
    ) -> Result<(Option<String>, usize)> {
        let delta_link =
            delta_link.ok_or_else(|| anyhow::anyhow!("No delta link available for polling"))?;

        let mut response = client.follow_delta_link(delta_link).await?;
        let mut all_changes = Vec::new();
        let mut final_delta_link = response.delta_link.clone();

        // Collect changes from all pages
        let changes = parse_delta_changes(&response, entity_name);
        all_changes.extend(changes);

        // Follow pagination (matching platform's while(moreData) loop)
        while response.next_link.is_some() {
            let next = response
                .next_link
                .as_ref()
                .expect("next_link checked above");
            response = client.follow_next_link(next).await?;
            let changes = parse_delta_changes(&response, entity_name);
            all_changes.extend(changes);
            if response.delta_link.is_some() {
                final_delta_link = response.delta_link.clone();
            }
        }

        let change_count = all_changes.len();

        // Dispatch changes (matching platform's channel.Writer.WriteAsync pattern)
        for change in &all_changes {
            let source_change = Self::convert_to_source_change(source_id, change);
            let timestamp = chrono::Utc::now();

            let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
            profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

            let wrapper = SourceEventWrapper::with_profiling(
                source_id.to_string(),
                SourceEvent::Change(source_change),
                timestamp,
                profiling,
            );

            if let Err(e) =
                SourceBase::dispatch_from_task(dispatchers.clone(), wrapper, source_id).await
            {
                log::error!("[{source_id}] Failed to dispatch change for {entity_name}: {e}");
            }
        }

        Ok((final_delta_link, change_count))
    }

    /// Convert a Dataverse change to a Drasi SourceChange.
    ///
    /// Maps the platform's `IChangedItem` classification:
    /// - `NewOrUpdated` → `SourceChange::Update` (like platform's `ChangeOp.UPDATE`)
    /// - `Deleted` → `SourceChange::Delete` (like platform's `ChangeOp.DELETE`)
    fn convert_to_source_change(source_id: &str, change: &DataverseChange) -> SourceChange {
        match change {
            DataverseChange::NewOrUpdated {
                id,
                entity_name,
                attributes,
            } => {
                // Convert JSON attributes to ElementPropertyMap
                // Mirrors the platform's JsonEventMapper attribute processing
                let mut properties = ElementPropertyMap::new();
                for (key, value) in attributes {
                    let element_value = Self::convert_json_value(value);
                    properties.insert(key, element_value);
                }

                let element_id = format!("{entity_name}:{id}");
                let metadata = ElementMetadata {
                    reference: ElementReference::new(source_id, &element_id),
                    labels: Arc::from(vec![Arc::from(entity_name.as_str())]),
                    effective_from: chrono::Utc::now().timestamp_millis() as u64,
                };

                SourceChange::Update {
                    element: Element::Node {
                        metadata,
                        properties,
                    },
                }
            }
            DataverseChange::Deleted { id, entity_name } => {
                let element_id = format!("{entity_name}:{id}");
                let metadata = ElementMetadata {
                    reference: ElementReference::new(source_id, &element_id),
                    labels: Arc::from(vec![Arc::from(entity_name.as_str())]),
                    effective_from: chrono::Utc::now().timestamp_millis() as u64,
                };

                SourceChange::Delete { metadata }
            }
        }
    }

    /// Convert a JSON value to an ElementValue.
    ///
    /// Handles Dataverse-specific value types, mirroring the platform's
    /// `JsonEventMapper` which extracts `Value` from complex types like
    /// `OptionSetValue` and `EntityReference`.
    fn convert_json_value(value: &serde_json::Value) -> ElementValue {
        match value {
            serde_json::Value::Null => ElementValue::Null,
            serde_json::Value::Bool(b) => ElementValue::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    ElementValue::Integer(i)
                } else if let Some(f) = n.as_f64() {
                    ElementValue::Float(ordered_float::OrderedFloat(f))
                } else {
                    ElementValue::Null
                }
            }
            serde_json::Value::String(s) => ElementValue::String(Arc::from(s.as_str())),
            serde_json::Value::Array(arr) => {
                // Handle multi-select choice: [{"Value":1},{"Value":2}] -> [1,2]
                // Mirrors platform's JsonEventMapper array handling
                if !arr.is_empty() {
                    if let Some(first_obj) = arr[0].as_object() {
                        if first_obj.contains_key("Value") {
                            let values: Vec<ElementValue> = arr
                                .iter()
                                .filter_map(|item| {
                                    item.as_object()
                                        .and_then(|obj| obj.get("Value"))
                                        .map(Self::convert_json_value)
                                })
                                .collect();
                            return ElementValue::List(values);
                        }
                    }
                }
                ElementValue::List(arr.iter().map(Self::convert_json_value).collect())
            }
            serde_json::Value::Object(obj) => {
                // Handle single value types: {"Value":123} -> 123
                // Mirrors platform's JsonEventMapper object handling
                if obj.contains_key("Value") && obj.len() <= 2 {
                    if let Some(val) = obj.get("Value") {
                        return Self::convert_json_value(val);
                    }
                }
                // Convert object to ElementPropertyMap
                let mut map = ElementPropertyMap::new();
                for (k, v) in obj {
                    map.insert(k, Self::convert_json_value(v));
                }
                ElementValue::Object(map)
            }
        }
    }
}

#[async_trait]
impl Source for DataverseSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "dataverse"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "environment_url".to_string(),
            serde_json::Value::String(self.config.environment_url.clone()),
        );
        props.insert(
            "tenant_id".to_string(),
            serde_json::Value::String(self.config.tenant_id.clone()),
        );
        props.insert(
            "client_id".to_string(),
            serde_json::Value::String(self.config.client_id.clone()),
        );
        // Do NOT expose client_secret in properties
        props.insert(
            "entities".to_string(),
            serde_json::Value::Array(
                self.config
                    .entities
                    .iter()
                    .map(|e| serde_json::Value::String(e.clone()))
                    .collect(),
            ),
        );
        props.insert(
            "api_version".to_string(),
            serde_json::Value::String(self.config.api_version.clone()),
        );
        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        log::info!("[{}] Starting Dataverse source", self.base.id);

        self.base.set_status(ComponentStatus::Starting).await;
        self.base
            .send_component_event(
                ComponentStatus::Starting,
                Some("Starting Dataverse source".to_string()),
            )
            .await?;

        // Create token manager and client
        let base_url = self.config.environment_url.clone();

        // Build the identity provider for token acquisition.
        // Priority: explicit identity_provider > Azure CLI > client credentials
        let provider: Arc<dyn IdentityProvider> = if let Some(ref ip) = self.identity_provider {
            Arc::from(ip.clone_box())
        } else if self.config.use_azure_cli {
            log::info!(
                "[{}] Using Azure CLI authentication (TokenManager)",
                self.base.id
            );
            Arc::new(TokenManager::azure_cli(&base_url))
        } else {
            let token_url = format!(
                "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
                self.config.tenant_id
            );
            Arc::new(TokenManager::with_token_url(
                &self.config.tenant_id,
                &self.config.client_id,
                &self.config.client_secret,
                &base_url,
                &token_url,
            ))
        };

        let client = Arc::new(DataverseClient::new(
            &base_url,
            &self.config.api_version,
            provider,
        ));

        // Create shutdown channel (watch channel for multiple receivers)
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        let shutdown_tx = Arc::new(shutdown_tx);

        let dispatchers = self.base.dispatchers.clone();
        let state_store = self.base.state_store().await;
        let source_id = self.base.id.clone();

        // Calculate effective max interval using square root scaling based on
        // entity count, matching the platform's ChangeMonitor.cs:
        //   calculatedMaxIntervalMs = SingleEntityMaxIntervalMs * sqrt(entityCount)
        // Examples: 1 entity = 30s, 5 entities = ~67s, 10 entities = ~95s
        let entity_count = self.config.entities.len() as f64;
        let effective_max_interval_seconds =
            (self.config.max_interval_seconds as f64 * entity_count.sqrt()).max(1.0) as u64;
        log::info!(
            "[{}] Effective max polling interval: {}s (base {}s * sqrt({} entities))",
            self.base.id,
            effective_max_interval_seconds,
            self.config.max_interval_seconds,
            self.config.entities.len()
        );

        // Get instance_id from context for log routing
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        // Spawn a worker task per entity (matching platform's SyncWorker pattern)
        let mut task_handles = Vec::new();
        for entity_name in &self.config.entities {
            let entity_set_name = self.config.entity_set_name(entity_name);
            let select = self.config.select_columns(entity_name);
            let source_id = source_id.clone();
            let entity_name = entity_name.clone();
            let client = client.clone();
            let dispatchers = dispatchers.clone();
            let state_store = state_store.clone();
            let shutdown_rx = shutdown_tx.subscribe();
            let min_interval_ms = self.config.min_interval_ms;
            let max_interval_seconds = effective_max_interval_seconds;
            let instance_id = instance_id.clone();

            let span = tracing::info_span!(
                "dataverse_entity_worker",
                instance_id = %instance_id,
                component_id = %source_id,
                component_type = "source",
                entity = %entity_name
            );

            let handle = tokio::spawn(
                async move {
                    Self::run_entity_worker(
                        source_id,
                        entity_name,
                        entity_set_name,
                        select,
                        client,
                        dispatchers,
                        state_store,
                        shutdown_rx,
                        min_interval_ms,
                        max_interval_seconds,
                    )
                    .await;
                }
                .instrument(span),
            );
            task_handles.push(handle);
        }

        // Store the shutdown sender for stop()
        // We use a combined task that waits for all workers
        let source_id = self.base.id.clone();
        let combined_handle = tokio::spawn(async move {
            for (i, handle) in task_handles.into_iter().enumerate() {
                if let Err(e) = handle.await {
                    log::error!("[{source_id}] Entity worker {i} terminated with error: {e}");
                }
            }
            log::info!("[{source_id}] All entity workers stopped");
        });

        *self.base.task_handle.write().await = Some(combined_handle);

        // Store a shutdown bridge so stop() can trigger shutdown via the watch channel
        {
            let mut lock = self.base.shutdown_tx.write().await;
            let shutdown_tx_for_stop = shutdown_tx.clone();
            let (bridge_tx, bridge_rx) = tokio::sync::oneshot::channel::<()>();
            tokio::spawn(async move {
                let _ = bridge_rx.await;
                let _ = shutdown_tx_for_stop.send(true);
            });
            *lock = Some(bridge_tx);
        }

        self.base.set_status(ComponentStatus::Running).await;
        self.base
            .send_component_event(
                ComponentStatus::Running,
                Some(format!(
                    "Dataverse source running, monitoring {} entities",
                    self.config.entities.len()
                )),
            )
            .await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log::info!("[{}] Stopping Dataverse source", self.base.id);

        self.base.set_status(ComponentStatus::Stopping).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopping,
                Some("Stopping Dataverse source".to_string()),
            )
            .await?;

        // Send shutdown signal through the bridge
        if let Some(tx) = self.base.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }

        // Wait for the combined task to finish
        if let Some(handle) = self.base.task_handle.write().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
        }

        self.base.set_status(ComponentStatus::Stopped).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopped,
                Some("Dataverse source stopped".to_string()),
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
    ) -> Result<drasi_lib::channels::SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "Dataverse")
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

/// Builder for `DataverseSource` instances.
///
/// Provides a fluent API for constructing Dataverse sources with sensible defaults.
/// The builder takes the source ID at construction and returns a fully constructed
/// `DataverseSource` from `build()`.
///
/// # Example
///
/// ```rust,ignore
/// let source = DataverseSource::builder("dv-source")
///     .with_environment_url("https://myorg.crm.dynamics.com")
///     .with_tenant_id("tenant-id")
///     .with_client_id("client-id")
///     .with_client_secret("client-secret")
///     .with_entities(vec!["account".to_string()])
///     .build()?;
/// ```
pub struct DataverseSourceBuilder {
    id: String,
    environment_url: String,
    tenant_id: String,
    client_id: String,
    client_secret: String,
    use_azure_cli: bool,
    entities: Vec<String>,
    entity_set_overrides: HashMap<String, String>,
    entity_columns: HashMap<String, Vec<String>>,
    min_interval_ms: u64,
    max_interval_seconds: u64,
    api_version: String,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    identity_provider: Option<Box<dyn IdentityProvider>>,
    auto_start: bool,
}

impl DataverseSourceBuilder {
    /// Create a new builder with the given source ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            environment_url: String::new(),
            tenant_id: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            use_azure_cli: false,
            entities: Vec::new(),
            entity_set_overrides: HashMap::new(),
            entity_columns: HashMap::new(),
            min_interval_ms: 500,
            max_interval_seconds: 30,
            api_version: "v9.2".to_string(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            identity_provider: None,
            auto_start: true,
        }
    }

    /// Set the Dataverse environment URL.
    pub fn with_environment_url(mut self, url: impl Into<String>) -> Self {
        self.environment_url = url.into();
        self
    }

    /// Set the Azure AD tenant ID.
    pub fn with_tenant_id(mut self, tenant_id: impl Into<String>) -> Self {
        self.tenant_id = tenant_id.into();
        self
    }

    /// Set the Azure AD client ID.
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = client_id.into();
        self
    }

    /// Set the Azure AD client secret.
    pub fn with_client_secret(mut self, client_secret: impl Into<String>) -> Self {
        self.client_secret = client_secret.into();
        self
    }

    /// Use Azure CLI for authentication instead of client credentials.
    ///
    /// When enabled, the source acquires tokens by running
    /// `az account get-access-token --resource <environment_url>`.
    /// Requires `az login` to have been run beforehand.
    ///
    /// This is the simplest auth option for local development and testing.
    /// When using Azure CLI auth, `tenant_id`, `client_id`, and `client_secret` are not required.
    pub fn with_azure_cli_auth(mut self) -> Self {
        self.use_azure_cli = true;
        self
    }

    /// Set an identity provider for token acquisition.
    ///
    /// When an identity provider is set, it takes precedence over
    /// `tenant_id`/`client_id`/`client_secret` and `use_azure_cli`.
    /// The provider's `get_credentials()` must return `Credentials::Token`.
    ///
    /// This enables using any of the platform's identity providers, including:
    /// - `AzureIdentityProvider::with_client_secret(...)` for client credentials
    /// - `AzureIdentityProvider::with_default_credentials(...)` for managed identity
    /// - `AzureIdentityProvider::with_developer_tools(...)` for local dev (CLI, azd, PS)
    /// - `AzureIdentityProvider::with_workload_identity(...)` for Kubernetes workloads
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_lib::identity::AzureIdentityProvider;
    /// use drasi_source_dataverse::DataverseSource;
    ///
    /// let provider = AzureIdentityProvider::with_client_secret(
    ///     "tenant-id", "client-id", "client-secret", "dataverse",
    /// )?
    /// .with_scope("https://myorg.crm.dynamics.com/.default");
    ///
    /// let source = DataverseSource::builder("dv-source")
    ///     .with_environment_url("https://myorg.crm.dynamics.com")
    ///     .with_entities(vec!["account".to_string()])
    ///     .with_identity_provider(provider)
    ///     .build()?;
    /// ```
    pub fn with_identity_provider(mut self, provider: impl IdentityProvider + 'static) -> Self {
        self.identity_provider = Some(Box::new(provider));
        self
    }

    /// Set the list of entity logical names to monitor.
    pub fn with_entities(mut self, entities: Vec<String>) -> Self {
        self.entities = entities;
        self
    }

    /// Add a single entity to monitor.
    pub fn with_entity(mut self, entity: impl Into<String>) -> Self {
        self.entities.push(entity.into());
        self
    }

    /// Override the entity set name for a specific entity.
    pub fn with_entity_set_override(
        mut self,
        entity_name: impl Into<String>,
        entity_set_name: impl Into<String>,
    ) -> Self {
        self.entity_set_overrides
            .insert(entity_name.into(), entity_set_name.into());
        self
    }

    /// Set column selection for a specific entity.
    pub fn with_entity_columns(mut self, entity: impl Into<String>, columns: Vec<String>) -> Self {
        self.entity_columns.insert(entity.into(), columns);
        self
    }

    /// Set the minimum adaptive polling interval in milliseconds.
    pub fn with_min_interval_ms(mut self, ms: u64) -> Self {
        self.min_interval_ms = ms;
        self
    }

    /// Set the maximum adaptive polling interval in seconds.
    pub fn with_max_interval_seconds(mut self, seconds: u64) -> Self {
        self.max_interval_seconds = seconds;
        self
    }

    /// Set the Dataverse Web API version.
    pub fn with_api_version(mut self, version: impl Into<String>) -> Self {
        self.api_version = version.into();
        self
    }

    /// Set the dispatch mode.
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set the dispatch buffer capacity.
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the bootstrap provider for initial data delivery.
    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Set whether this source should auto-start when DrasiLib starts.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Build the `DataverseSource` instance.
    pub fn build(self) -> Result<DataverseSource> {
        let config = DataverseSourceConfig {
            environment_url: self.environment_url,
            tenant_id: self.tenant_id,
            client_id: self.client_id,
            client_secret: self.client_secret,
            use_azure_cli: self.use_azure_cli,
            entities: self.entities,
            entity_set_overrides: self.entity_set_overrides,
            entity_columns: self.entity_columns,
            min_interval_ms: self.min_interval_ms,
            max_interval_seconds: self.max_interval_seconds,
            api_version: self.api_version,
        };

        // When an identity provider is supplied, skip client credential validation
        // (only environment_url and entities are required)
        if self.identity_provider.is_some() {
            config
                .validate_with_identity_provider()
                .map_err(|e| anyhow::anyhow!(e))?;
        } else {
            config.validate().map_err(|e| anyhow::anyhow!(e))?;
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

        Ok(DataverseSource {
            base: SourceBase::new(params)?,
            config,
            identity_provider: self.identity_provider,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod construction {
        use super::*;

        #[test]
        fn test_builder_creates_source() {
            let source = DataverseSource::builder("dv-source")
                .with_environment_url("https://myorg.crm.dynamics.com")
                .with_tenant_id("tenant-1")
                .with_client_id("client-1")
                .with_client_secret("secret-1")
                .with_entities(vec!["account".to_string()])
                .build();
            assert!(source.is_ok());
        }

        #[test]
        fn test_builder_fails_without_entities() {
            let source = DataverseSource::builder("dv-source")
                .with_environment_url("https://myorg.crm.dynamics.com")
                .with_tenant_id("tenant-1")
                .with_client_id("client-1")
                .with_client_secret("secret-1")
                .build();
            assert!(source.is_err());
        }

        #[test]
        fn test_builder_fails_without_url() {
            let source = DataverseSource::builder("dv-source")
                .with_tenant_id("tenant-1")
                .with_client_id("client-1")
                .with_client_secret("secret-1")
                .with_entities(vec!["account".to_string()])
                .build();
            assert!(source.is_err());
        }

        #[test]
        fn test_new_with_valid_config() {
            let config = DataverseSourceConfig {
                environment_url: "https://test.crm.dynamics.com".to_string(),
                tenant_id: "t".to_string(),
                client_id: "c".to_string(),
                client_secret: "s".to_string(),
                use_azure_cli: false,
                entities: vec!["account".to_string()],
                entity_set_overrides: HashMap::new(),
                entity_columns: HashMap::new(),
                min_interval_ms: 500,
                max_interval_seconds: 30,
                api_version: "v9.2".to_string(),
            };
            let source = DataverseSource::new("test-source", config);
            assert!(source.is_ok());
        }
    }

    mod properties {
        use super::*;

        #[test]
        fn test_id_returns_correct_value() {
            let source = DataverseSource::builder("my-dv-source")
                .with_environment_url("https://test.crm.dynamics.com")
                .with_tenant_id("t")
                .with_client_id("c")
                .with_client_secret("s")
                .with_entities(vec!["account".to_string()])
                .build()
                .expect("should build");
            assert_eq!(source.id(), "my-dv-source");
        }

        #[test]
        fn test_type_name_returns_dataverse() {
            let source = DataverseSource::builder("test")
                .with_environment_url("https://test.crm.dynamics.com")
                .with_tenant_id("t")
                .with_client_id("c")
                .with_client_secret("s")
                .with_entities(vec!["account".to_string()])
                .build()
                .expect("should build");
            assert_eq!(source.type_name(), "dataverse");
        }

        #[test]
        fn test_properties_does_not_expose_secret() {
            let source = DataverseSource::builder("test")
                .with_environment_url("https://test.crm.dynamics.com")
                .with_tenant_id("t")
                .with_client_id("c")
                .with_client_secret("super-secret-value")
                .with_entities(vec!["account".to_string()])
                .build()
                .expect("should build");
            let props = source.properties();

            assert!(props.contains_key("environment_url"));
            assert!(props.contains_key("tenant_id"));
            assert!(props.contains_key("client_id"));
            assert!(props.contains_key("entities"));
            assert!(!props.contains_key("client_secret"));
        }

        #[test]
        fn test_properties_contains_correct_values() {
            let source = DataverseSource::builder("test")
                .with_environment_url("https://myorg.crm.dynamics.com")
                .with_tenant_id("tenant-123")
                .with_client_id("client-456")
                .with_client_secret("s")
                .with_entities(vec!["account".to_string(), "contact".to_string()])
                .build()
                .expect("should build");
            let props = source.properties();

            assert_eq!(
                props.get("environment_url"),
                Some(&serde_json::Value::String(
                    "https://myorg.crm.dynamics.com".to_string()
                ))
            );
            assert_eq!(
                props.get("tenant_id"),
                Some(&serde_json::Value::String("tenant-123".to_string()))
            );
        }
    }

    mod lifecycle {
        use super::*;

        #[tokio::test]
        async fn test_initial_status_is_stopped() {
            let source = DataverseSource::builder("test")
                .with_environment_url("https://test.crm.dynamics.com")
                .with_tenant_id("t")
                .with_client_id("c")
                .with_client_secret("s")
                .with_entities(vec!["account".to_string()])
                .build()
                .expect("should build");
            assert_eq!(source.status().await, ComponentStatus::Stopped);
        }
    }

    mod builder {
        use super::*;

        #[test]
        fn test_builder_defaults() {
            let source = DataverseSource::builder("test")
                .with_environment_url("https://test.crm.dynamics.com")
                .with_tenant_id("t")
                .with_client_id("c")
                .with_client_secret("s")
                .with_entities(vec!["account".to_string()])
                .build()
                .expect("should build");

            assert_eq!(source.config.min_interval_ms, 500);
            assert_eq!(source.config.max_interval_seconds, 30);
            assert_eq!(source.config.api_version, "v9.2");
            assert!(source.identity_provider.is_none());
        }

        #[test]
        fn test_builder_custom_values() {
            let source = DataverseSource::builder("test")
                .with_environment_url("https://custom.crm.dynamics.com")
                .with_tenant_id("custom-tenant")
                .with_client_id("custom-client")
                .with_client_secret("custom-secret")
                .with_entities(vec!["account".to_string()])
                .with_min_interval_ms(200)
                .with_max_interval_seconds(60)
                .with_api_version("v9.1")
                .build()
                .expect("should build");

            assert_eq!(
                source.config.environment_url,
                "https://custom.crm.dynamics.com"
            );
            assert_eq!(source.config.min_interval_ms, 200);
            assert_eq!(source.config.max_interval_seconds, 60);
            assert_eq!(source.config.api_version, "v9.1");
        }

        #[test]
        fn test_builder_with_entity() {
            let source = DataverseSource::builder("test")
                .with_environment_url("https://test.crm.dynamics.com")
                .with_tenant_id("t")
                .with_client_id("c")
                .with_client_secret("s")
                .with_entity("account")
                .with_entity("contact")
                .build()
                .expect("should build");

            assert_eq!(source.config.entities, vec!["account", "contact"]);
        }

        #[test]
        fn test_builder_with_entity_set_override() {
            let source = DataverseSource::builder("test")
                .with_environment_url("https://test.crm.dynamics.com")
                .with_tenant_id("t")
                .with_client_id("c")
                .with_client_secret("s")
                .with_entity("activityparty")
                .with_entity_set_override("activityparty", "activityparties")
                .build()
                .expect("should build");

            assert_eq!(
                source.config.entity_set_name("activityparty"),
                "activityparties"
            );
        }

        #[test]
        fn test_builder_with_entity_columns() {
            let source = DataverseSource::builder("test")
                .with_environment_url("https://test.crm.dynamics.com")
                .with_tenant_id("t")
                .with_client_id("c")
                .with_client_secret("s")
                .with_entity("account")
                .with_entity_columns("account", vec!["name".to_string(), "revenue".to_string()])
                .build()
                .expect("should build");

            assert_eq!(
                source.config.select_columns("account"),
                Some("name,revenue".to_string())
            );
        }

        #[test]
        fn test_builder_with_identity_provider() {
            // When an identity provider is set, client credentials are not required
            let provider = drasi_lib::identity::PasswordIdentityProvider::new("user", "token");
            let source = DataverseSource::builder("test")
                .with_environment_url("https://test.crm.dynamics.com")
                .with_entities(vec!["account".to_string()])
                .with_identity_provider(provider)
                .build()
                .expect("should build with identity provider and no client credentials");

            assert!(source.identity_provider.is_some());
        }

        #[test]
        fn test_builder_with_identity_provider_still_needs_url() {
            let provider = drasi_lib::identity::PasswordIdentityProvider::new("user", "token");
            let result = DataverseSource::builder("test")
                .with_entities(vec!["account".to_string()])
                .with_identity_provider(provider)
                .build();
            assert!(result.is_err(), "should fail without environment_url");
        }

        #[test]
        fn test_builder_with_identity_provider_still_needs_entities() {
            let provider = drasi_lib::identity::PasswordIdentityProvider::new("user", "token");
            let result = DataverseSource::builder("test")
                .with_environment_url("https://test.crm.dynamics.com")
                .with_identity_provider(provider)
                .build();
            assert!(result.is_err(), "should fail without entities");
        }
    }

    mod change_conversion {
        use super::*;

        #[test]
        fn test_convert_new_or_updated() {
            let mut attributes = serde_json::Map::new();
            attributes.insert(
                "name".to_string(),
                serde_json::Value::String("Contoso".to_string()),
            );
            attributes.insert("revenue".to_string(), serde_json::json!(1000000.0));
            attributes.insert(
                "accountid".to_string(),
                serde_json::Value::String("abc-123".to_string()),
            );

            let change = DataverseChange::NewOrUpdated {
                id: "abc-123".to_string(),
                entity_name: "account".to_string(),
                attributes,
            };

            let source_change = DataverseSource::convert_to_source_change("test-source", &change);
            match source_change {
                SourceChange::Update { element } => match element {
                    Element::Node {
                        metadata,
                        properties,
                    } => {
                        assert_eq!(metadata.reference.element_id.as_ref(), "account:abc-123");
                        assert_eq!(metadata.reference.source_id.as_ref(), "test-source");
                        assert_eq!(metadata.labels.len(), 1);
                        assert_eq!(metadata.labels[0].as_ref(), "account");
                        assert!(properties.get("name").is_some());
                    }
                    _ => panic!("Expected Node element"),
                },
                _ => panic!("Expected Update change"),
            }
        }

        #[test]
        fn test_convert_deleted() {
            let change = DataverseChange::Deleted {
                id: "def-456".to_string(),
                entity_name: "contact".to_string(),
            };

            let source_change = DataverseSource::convert_to_source_change("test-source", &change);
            match source_change {
                SourceChange::Delete { metadata } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "contact:def-456");
                    assert_eq!(metadata.reference.source_id.as_ref(), "test-source");
                    assert_eq!(metadata.labels[0].as_ref(), "contact");
                }
                _ => panic!("Expected Delete change"),
            }
        }

        #[test]
        fn test_convert_json_value_primitives() {
            assert_eq!(
                DataverseSource::convert_json_value(&serde_json::Value::Null),
                ElementValue::Null
            );
            assert_eq!(
                DataverseSource::convert_json_value(&serde_json::json!(true)),
                ElementValue::Bool(true)
            );
            assert_eq!(
                DataverseSource::convert_json_value(&serde_json::json!(42)),
                ElementValue::Integer(42)
            );
            assert_eq!(
                DataverseSource::convert_json_value(&serde_json::json!(3.15)),
                ElementValue::Float(ordered_float::OrderedFloat(3.15))
            );
            assert_eq!(
                DataverseSource::convert_json_value(&serde_json::json!("hello")),
                ElementValue::String(Arc::from("hello"))
            );
        }

        #[test]
        fn test_convert_json_value_extracts_value() {
            // Single value type: {"Value": 123} -> 123
            let json = serde_json::json!({"Value": 123});
            assert_eq!(
                DataverseSource::convert_json_value(&json),
                ElementValue::Integer(123)
            );
        }

        #[test]
        fn test_convert_json_value_multi_select_choice() {
            // Multi-select: [{"Value":1},{"Value":2}] -> [1,2]
            let json = serde_json::json!([{"Value": 1}, {"Value": 2}]);
            let result = DataverseSource::convert_json_value(&json);
            match result {
                ElementValue::List(values) => {
                    assert_eq!(values.len(), 2);
                    assert_eq!(values[0], ElementValue::Integer(1));
                    assert_eq!(values[1], ElementValue::Integer(2));
                }
                _ => panic!("Expected List"),
            }
        }
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "dataverse-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::DataverseSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
