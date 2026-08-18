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

//! OpenTelemetry OTLP source for Drasi.
//!
//! Receives OTLP over gRPC (and optionally HTTP protobuf) and projects an
//! allowlisted subset of metrics, traces, and logs into a bounded live graph.
//! Use it as a correlation input beside Kubernetes or PostgreSQL sources — not
//! as a telemetry backend.
//!
//! # Configuration
//!
//! The source is configured via [`OtelSourceConfig`]. Key options:
//!
//! - `grpc_bind`: OTLP/gRPC listen address. Default: `0.0.0.0:4317`. Empty disables gRPC.
//! - `http_bind`: (Optional) OTLP/HTTP protobuf listen address (`/v1/traces|metrics|logs`)
//! - `metric_allowlist`: accepted metric names. Empty rejects all. `*` allows all.
//! - `destination_attributes`: span attributes used as the callee. Default: `peer.service`
//! - `dependency_ttl_secs`: `DEPENDS_ON` expiry unless refreshed. Default: `300`
//! - `log_event_ttl_secs`: `LogEvent` expiry. Default: `60`
//! - `auth_token`: (Optional) inbound bearer token
//! - `durability`: (Optional) WAL replay of projected changes
//!
//! # Example Configuration (YAML)
//!
//! ```yaml
//! source_type: otel
//! properties:
//!   grpcBind: "0.0.0.0:4317"
//!   metricAllowlist: ["latency_p99_ms", "*_p99"]
//!   heartbeatMetric: "health.heartbeat"
//!   dependencyTtlSecs: 300
//! ```
//!
//! # Data Format
//!
//! Inbound payloads are OTLP `Export*ServiceRequest` protobuf messages
//! (`application/x-protobuf` on HTTP). Resource `service.name` is required.
//!
//! ## Gauge / sum (upsert)
//!
//! A data point with Resource `service.name=checkout` and metric
//! `latency_p99_ms=920` becomes:
//!
//! ```text
//! (:Service {name: "checkout"})-[:REPORTS]->
//! (:Metric {name: "latency_p99_ms", unit: "ms", value: 920})
//! ```
//!
//! The next point with the same identity is an `Update`.
//!
//! ## Client span (upsert + TTL)
//!
//! A `SPAN_KIND_CLIENT` span with `peer.service=payments` upserts
//! `(:Service {name: "checkout"})-[:DEPENDS_ON]->(:Service {name: "payments"})`.
//! The edge is deleted if it is not refreshed within `dependency_ttl_secs`.
//!
//! ## Log record (insert + TTL)
//!
//! An allowlisted ERROR log becomes a `LogEvent` node plus `EMITS`, deleted
//! after `log_event_ttl_secs`.
//!
//! OTLP timestamps are nanoseconds and are converted to millisecond
//! `effective_from` values.
//!
//! # Example
//!
//! ```rust,no_run
//! use drasi_source_otel::OtelSource;
//!
//! # fn example() -> anyhow::Result<()> {
//! let source = OtelSource::builder("otel")
//!     .with_grpc_bind("127.0.0.1:4317")
//!     .with_metric_allowlist(["latency_p99_ms"])
//!     .with_heartbeat_metric("health.heartbeat")
//!     .build()?;
//! # let _ = source;
//! # Ok(())
//! # }
//! ```

#![allow(
    unexpected_cfgs,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items
)]

pub mod config;
pub mod descriptor;
pub mod otlp;

mod auth;
mod counters;
mod lifecycle;
mod mapping;
mod receiver;

pub use config::OtelSourceConfig;
pub use counters::OtelCounterSnapshot;
pub use otlp::proto;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use log::{info, warn};
use tokio::sync::RwLock;

use drasi_lib::channels::{ComponentStatus, DispatchMode, SubscriptionResponse};
use drasi_lib::identity::IdentityProvider;
use drasi_lib::managers::{log_component_start, log_component_stop};
use drasi_lib::schema::{NodeSchema, PropertySchema, RelationSchema, SourceSchema};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::sources::{ByteLexPositionComparator, SourceError};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::WalProvider;
use drasi_lib::Source;
use tracing::Instrument;

use crate::counters::OtelCounters;
use crate::descriptor::OtelSourceConfigDto;
use crate::lifecycle::LifecycleState;
use crate::receiver::OtelRuntime;

/// OpenTelemetry OTLP source.
///
/// Owns the inbound OTLP listeners, maps allowlisted signals to
/// [`SourceChange`](drasi_core::models::SourceChange) values, and dispatches
/// them through [`SourceBase`]. Lifecycle state (seen ids and TTL records) is
/// kept in process and optionally persisted to the state store. There is no
/// bootstrap snapshot.
pub struct OtelSource {
    /// Common source functionality (dispatchers, status, identity, WAL hooks).
    pub(crate) base: SourceBase,
    /// OTLP bind, allowlist, TTL, and durability settings.
    config: OtelSourceConfig,
    /// Seen-id, cardinality, and TTL tracking.
    lifecycle: Arc<RwLock<LifecycleState>>,
    /// Accepted / rejected / dropped / expired counters.
    counters: Arc<OtelCounters>,
    /// WAL provider when durability is enabled.
    wal: tokio::sync::RwLock<Option<Arc<dyn WalProvider>>>,
    /// Background WAL prune task.
    prune_task: tokio::sync::RwLock<Option<tokio::task::JoinHandle<()>>>,
    /// Identity provider set on the builder, applied in `initialize()`.
    pending_identity: tokio::sync::RwLock<Option<Arc<dyn IdentityProvider>>>,
}

impl OtelSource {
    /// Create a builder for [`OtelSource`].
    ///
    /// # Arguments
    ///
    /// * `id` - Unique source instance id used in element references
    pub fn builder(id: impl Into<String>) -> OtelSourceBuilder {
        OtelSourceBuilder::new(id)
    }

    /// Create a new source with the given ID and configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if [`OtelSourceConfig::validate`] fails.
    pub fn new(id: impl Into<String>, config: OtelSourceConfig) -> Result<Self> {
        Self::builder(id).with_config(config).build()
    }

    /// Snapshot of admission counters (accepted, rejected, dropped, expired).
    pub fn counters(&self) -> OtelCounterSnapshot {
        self.counters.snapshot()
    }

    pub(crate) fn base_mut(&mut self) -> &mut SourceBase {
        &mut self.base
    }
}

#[async_trait]
impl Source for OtelSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "otel"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use drasi_plugin_sdk::ConfigValue;

        let dto = OtelSourceConfigDto {
            grpc_bind: ConfigValue::Static(self.config.grpc_bind.clone()),
            http_bind: self
                .config
                .http_bind
                .as_ref()
                .map(|v| ConfigValue::Static(v.clone())),
            tls_cert_path: self
                .config
                .tls_cert_path
                .as_ref()
                .map(|v| ConfigValue::Static(v.clone())),
            tls_key_path: self
                .config
                .tls_key_path
                .as_ref()
                .map(|v| ConfigValue::Static(v.clone())),
            tls_client_ca_path: self
                .config
                .tls_client_ca_path
                .as_ref()
                .map(|v| ConfigValue::Static(v.clone())),
            auth_token: self
                .config
                .auth_token
                .as_ref()
                .map(|v| ConfigValue::Static(v.clone())),
            metric_allowlist: self.config.metric_allowlist.clone(),
            metric_identity_attributes: self.config.metric_identity_attributes.clone(),
            destination_attributes: self.config.destination_attributes.clone(),
            span_kinds: self.config.span_kinds.clone(),
            heartbeat_metric: self
                .config
                .heartbeat_metric
                .as_ref()
                .map(|v| ConfigValue::Static(v.clone())),
            heartbeat_event_name: self
                .config
                .heartbeat_event_name
                .as_ref()
                .map(|v| ConfigValue::Static(v.clone())),
            log_min_severity: ConfigValue::Static(self.config.log_min_severity.clone()),
            log_event_name_allowlist: self.config.log_event_name_allowlist.clone(),
            log_event_ttl_secs: ConfigValue::Static(self.config.log_event_ttl_secs),
            dependency_ttl_secs: ConfigValue::Static(self.config.dependency_ttl_secs),
            max_services: ConfigValue::Static(self.config.max_services),
            max_metrics: ConfigValue::Static(self.config.max_metrics),
            max_dependencies: ConfigValue::Static(self.config.max_dependencies),
            max_log_events: ConfigValue::Static(self.config.max_log_events),
            reject_derived: ConfigValue::Static(self.config.reject_derived),
            durability: self.config.durability.clone(),
        };
        self.base.properties_or_serialize(&dto)
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    fn supports_replay(&self) -> bool {
        self.config.durability_enabled()
    }

    fn describe_schema(&self) -> Option<SourceSchema> {
        Some(otel_schema())
    }

    async fn start(&self) -> Result<()> {
        log_component_start("OTel Source", &self.base.id);
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting OpenTelemetry source".to_string()),
            )
            .await;

        if let Some(store) = self.base.state_store().await {
            if let Ok(Some(bytes)) = store.get(&self.base.id, "lifecycle").await {
                match LifecycleState::from_bytes(&bytes) {
                    Ok(state) => *self.lifecycle.write().await = state,
                    Err(e) => warn!("[{}] failed to load lifecycle state: {e}", self.base.id),
                }
            }
        }

        let wal_ref: Option<Arc<dyn WalProvider>> = if self.config.durability_enabled() {
            let ctx = self
                .base
                .context()
                .await
                .ok_or_else(|| anyhow::anyhow!("Context not initialized"))?;
            let wal = ctx.wal_provider.clone().ok_or_else(|| {
                anyhow::anyhow!("Durability enabled but no WAL provider configured on DrasiLib")
            })?;
            let wal_config = self
                .config
                .durability
                .as_ref()
                .expect("durability checked above")
                .to_wal_config();
            wal.register(&self.base.id, wal_config.clone())
                .await
                .with_context(|| format!("Failed to register WAL for source '{}'", self.base.id))?;
            let head = wal.head_sequence(&self.base.id).await.unwrap_or(0);
            if head > 0 {
                self.base.set_next_sequence(head);
            }
            self.base
                .set_position_comparator(ByteLexPositionComparator)
                .await;
            *self.wal.write().await = Some(wal.clone());
            Some(wal)
        } else {
            None
        };

        let endpoints = match receiver::bind_endpoints(&self.config).await {
            Ok(endpoints) => endpoints,
            Err(e) => {
                self.base
                    .set_status(ComponentStatus::Error, Some(e.to_string()))
                    .await;
                return Err(e);
            }
        };

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let runtime = OtelRuntime {
            source_id: self.base.id.clone(),
            config: Arc::new(self.config.clone()),
            base: self.base.clone_shared(),
            lifecycle: self.lifecycle.clone(),
            counters: self.counters.clone(),
            wal: wal_ref.clone(),
        };

        let reporter = self.base.status_handle();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();
        let source_id = self.base.id.clone();
        let span = tracing::info_span!(
            "otel_source_server",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("OpenTelemetry source listening".to_string()),
            )
            .await;
        let task = tokio::spawn(
            async move {
                if let Err(e) = receiver::serve(runtime, endpoints, shutdown_rx).await {
                    error_log(&e);
                    reporter
                        .set_status(ComponentStatus::Error, Some(e.to_string()))
                        .await;
                } else {
                    reporter.set_status(ComponentStatus::Stopped, None).await;
                }
            }
            .instrument(span),
        );
        self.base.set_task_handle(task).await;

        if let Some(wal) = wal_ref {
            let base = self.base.clone_shared();
            let source_id = self.base.id.clone();
            let prune_handle = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if let Some(confirmed) = base.compute_confirmed_position().await {
                        if let Err(e) = wal.prune_up_to(&source_id, confirmed).await {
                            warn!("[{source_id}] WAL prune failed: {e}");
                        }
                    }
                }
            });
            *self.prune_task.write().await = Some(prune_handle);
        }

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log_component_stop("OTel Source", &self.base.id);
        if let Some(handle) = self.prune_task.write().await.take() {
            handle.abort();
        }
        persist_lifecycle(&self.base, &self.lifecycle).await;
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        let wal_guard = self.wal.read().await;
        if let (Some(wal), Some(ref resume_from)) = (wal_guard.as_ref(), &settings.resume_from) {
            if resume_from.len() >= 8 {
                let resume_seq =
                    u64::from_be_bytes(resume_from[..8].try_into().unwrap_or_default());
                let wal_clone = wal.clone();
                drop(wal_guard);
                match wal_clone
                    .read_from(&self.base.id, resume_seq.saturating_add(1))
                    .await
                {
                    Err(drasi_lib::wal::WalError::PositionUnavailable {
                        oldest_available, ..
                    }) => {
                        return Err(SourceError::PositionUnavailable {
                            source_id: self.base.id.clone(),
                            requested: resume_from.clone(),
                            earliest_available: oldest_available
                                .map(|seq| bytes::Bytes::copy_from_slice(&seq.to_be_bytes())),
                        }
                        .into());
                    }
                    Err(e) => return Err(e.into()),
                    Ok(_) => {}
                }
                return self
                    .base
                    .subscribe_with_replay(&settings, wal_clone.as_ref(), resume_seq, "OTel")
                    .await;
            }
            drop(wal_guard);
            return Err(anyhow::anyhow!(
                "Invalid resume_from position: expected at least 8 bytes, got {}",
                resume_from.len()
            ));
        }
        drop(wal_guard);
        self.base.subscribe_with_bootstrap(&settings, "OTel").await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn deprovision(&self) -> Result<()> {
        let wal_guard = self.wal.read().await;
        if let Some(ref wal) = *wal_guard {
            info!("[{}] Deprovisioning: deleting WAL data", self.base.id);
            if let Err(e) = wal.delete_wal(&self.base.id).await {
                warn!(
                    "[{}] Failed to delete WAL during deprovision: {}",
                    self.base.id, e
                );
            }
        }
        drop(wal_guard);
        self.base.deprovision_common().await
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
        if let Some(identity) = self.pending_identity.write().await.take() {
            self.base.set_identity_provider(identity).await;
        }
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }

    async fn set_identity_provider(&self, provider: Arc<dyn IdentityProvider>) {
        self.base.set_identity_provider(provider).await;
    }

    async fn remove_position_handle(&self, query_id: &str) {
        self.base.remove_position_handle(query_id).await;
    }
}

/// Builder for [`OtelSource`].
///
/// Starts from [`OtelSourceConfig`] defaults. Call [`OtelSourceBuilder::build`]
/// to validate the configuration and construct the source.
pub struct OtelSourceBuilder {
    id: String,
    config: OtelSourceConfig,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    auto_start: bool,
    state_store: Option<Arc<dyn StateStoreProvider>>,
    identity: Option<Arc<dyn IdentityProvider>>,
}

impl OtelSourceBuilder {
    /// Create a builder with default configuration for `id`.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: OtelSourceConfig::default(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            auto_start: true,
            state_store: None,
            identity: None,
        }
    }

    /// Set the OTLP/gRPC listen address (`host:port`). Empty disables gRPC.
    pub fn with_grpc_bind(mut self, bind: impl Into<String>) -> Self {
        self.config.grpc_bind = bind.into();
        self
    }

    /// Set the optional OTLP/HTTP protobuf listen address.
    pub fn with_http_bind(mut self, bind: impl Into<String>) -> Self {
        self.config.http_bind = Some(bind.into());
        self
    }

    /// Set a static inbound bearer token. An identity provider overrides this.
    pub fn with_auth_token(mut self, token: impl Into<String>) -> Self {
        self.config.auth_token = Some(token.into());
        self
    }

    /// Set accepted metric names. Empty rejects all. `*` allows all.
    pub fn with_metric_allowlist<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.metric_allowlist = names.into_iter().map(Into::into).collect();
        self
    }

    /// Use a single span attribute as the destination service name.
    pub fn with_destination_attribute(mut self, attr: impl Into<String>) -> Self {
        self.config.destination_attributes = vec![attr.into()];
        self
    }

    /// Metric name that refreshes `Heartbeat.lastSeen`.
    pub fn with_heartbeat_metric(mut self, name: impl Into<String>) -> Self {
        self.config.heartbeat_metric = Some(name.into());
        self
    }

    /// Log `event_name` that refreshes `Heartbeat.lastSeen`.
    pub fn with_heartbeat_event_name(mut self, name: impl Into<String>) -> Self {
        self.config.heartbeat_event_name = Some(name.into());
        self
    }

    /// How long a `DEPENDS_ON` edge lives without a refreshing client span.
    pub fn with_dependency_ttl_secs(mut self, secs: u64) -> Self {
        self.config.dependency_ttl_secs = secs;
        self
    }

    /// How long `LogEvent` nodes live before the sweeper deletes them.
    pub fn with_log_event_ttl_secs(mut self, secs: u64) -> Self {
        self.config.log_event_ttl_secs = secs;
        self
    }

    /// Minimum log severity for `LogEvent` admission (`INFO`, `WARN`, `ERROR`).
    pub fn with_log_min_severity(mut self, severity: impl Into<String>) -> Self {
        self.config.log_min_severity = severity.into();
        self
    }

    /// Attach an inbound identity provider (applied during `initialize()`).
    pub fn with_identity_provider(mut self, provider: Arc<dyn IdentityProvider>) -> Self {
        self.identity = Some(provider);
        self
    }

    /// Persist lifecycle state (seen ids and TTL records) across restarts.
    pub fn with_state_store(mut self, store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// Enable optional WAL durability for projected changes.
    pub fn with_durability(mut self, config: drasi_lib::DurabilityConfig) -> Self {
        self.config.durability = Some(config);
        self
    }

    /// Whether the source should start with `DrasiLib::start()`. Default: `true`.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set dispatch mode. Default is Channel.
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Replace the entire configuration object.
    pub fn with_config(mut self, config: OtelSourceConfig) -> Self {
        self.config = config;
        self
    }

    /// Validate configuration and construct the source.
    ///
    /// # Errors
    ///
    /// Returns an error if binds, TTLs, or TLS paths are invalid.
    pub fn build(self) -> Result<OtelSource> {
        self.config.validate()?;
        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(store) = self.state_store {
            params = params.with_state_store(store);
        }
        let source = OtelSource {
            base: SourceBase::new(params)?,
            config: self.config,
            lifecycle: Arc::new(RwLock::new(LifecycleState::default())),
            counters: Arc::new(OtelCounters::default()),
            wal: tokio::sync::RwLock::new(None),
            prune_task: tokio::sync::RwLock::new(None),
            pending_identity: tokio::sync::RwLock::new(self.identity),
        };
        Ok(source)
    }
}

fn otel_schema() -> SourceSchema {
    SourceSchema {
        nodes: vec![
            node(
                "Service",
                &[
                    "name",
                    "namespace",
                    "environment",
                    "instanceId",
                    "registeredAt",
                    "lastSeen",
                ],
            ),
            node(
                "Metric",
                &["name", "unit", "value", "observedAt", "receivedAt"],
            ),
            node("Heartbeat", &["lastSeen"]),
            node(
                "LogEvent",
                &["service", "severity", "body", "eventName", "observedAt"],
            ),
        ],
        relations: vec![
            rel("REPORTS", Some("Service"), Some("Metric"), &[]),
            rel("HEARTBEAT", Some("Service"), Some("Heartbeat"), &[]),
            rel(
                "DEPENDS_ON",
                Some("Service"),
                Some("Service"),
                &["lastSeen"],
            ),
            rel("EMITS", Some("Service"), Some("LogEvent"), &[]),
        ],
    }
}

fn node(label: &str, props: &[&str]) -> NodeSchema {
    NodeSchema {
        label: label.to_string(),
        properties: props.iter().map(|p| PropertySchema::new(*p)).collect(),
    }
}

fn rel(label: &str, from: Option<&str>, to: Option<&str>, props: &[&str]) -> RelationSchema {
    RelationSchema {
        label: label.to_string(),
        from: from.map(str::to_string),
        to: to.map(str::to_string),
        properties: props.iter().map(|p| PropertySchema::new(*p)).collect(),
    }
}

async fn persist_lifecycle(base: &SourceBase, lifecycle: &RwLock<LifecycleState>) {
    let Some(store) = base.state_store().await else {
        return;
    };
    let bytes = {
        let state = lifecycle.read().await;
        match state.to_bytes() {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!("[{}] failed to serialize lifecycle on stop: {e}", base.id);
                return;
            }
        }
    };
    if let Err(e) = store.set(&base.id, "lifecycle", bytes).await {
        warn!("[{}] failed to persist lifecycle on stop: {e}", base.id);
    }
}

fn error_log(err: &anyhow::Error) {
    log::error!("OpenTelemetry source failed: {err}");
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "otel-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::OtelSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);

#[cfg(test)]
mod tests {
    use super::*;

    mod construction {
        use super::*;

        #[test]
        fn new_with_valid_config() {
            let source = OtelSource::new("otel", OtelSourceConfig::default());
            assert!(source.is_ok());
        }

        #[test]
        fn builder_defaults() {
            let source = OtelSource::builder("otel").build();
            assert!(source.is_ok());
            let source = source.unwrap();
            assert_eq!(source.id(), "otel");
            assert_eq!(source.type_name(), "otel");
            assert!(!source.supports_replay());
        }
    }

    mod properties {
        use super::*;

        #[test]
        fn properties_contain_bind_and_allowlist() {
            let source = OtelSource::builder("otel")
                .with_grpc_bind("127.0.0.1:4317")
                .with_metric_allowlist(["latency_p99_ms"])
                .with_auth_token("s3cret")
                .build()
                .unwrap();
            let props = source.properties();
            assert!(props.contains_key("grpcBind"));
            assert!(props.contains_key("authToken"));
            assert!(props.contains_key("metricAllowlist"));
        }
    }

    mod builder {
        use super::*;

        #[test]
        fn builder_chaining_overrides() {
            let source = OtelSource::builder("otel")
                .with_grpc_bind("127.0.0.1:1")
                .with_grpc_bind("127.0.0.1:4317")
                .with_auto_start(false)
                .with_dispatch_mode(DispatchMode::Channel)
                .build()
                .unwrap();
            assert!(!source.auto_start());
            let bind = source.properties().get("grpcBind").cloned().unwrap();
            assert!(bind.to_string().contains("127.0.0.1:4317"));
        }
    }

    mod config {
        use super::*;

        #[test]
        fn config_roundtrip() {
            let config = OtelSourceConfig {
                metric_allowlist: vec!["latency_p99_ms".to_string()],
                ..OtelSourceConfig::default()
            };
            let json = serde_json::to_string(&config).unwrap();
            let decoded: OtelSourceConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(config, decoded);
        }

        #[test]
        fn config_deserializes_defaults() {
            let config: OtelSourceConfig = serde_json::from_str("{}").unwrap();
            assert_eq!(config.grpc_bind, "0.0.0.0:4317");
            assert!(config.metric_allowlist.is_empty());
            assert_eq!(config.dependency_ttl_secs, 300);
        }
    }

    #[test]
    fn empty_binds_fail_validation() {
        let err = match OtelSource::builder("otel").with_grpc_bind("").build() {
            Ok(_) => panic!("expected validation error"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("grpc_bind") || err.to_string().contains("http_bind"));
    }

    #[tokio::test]
    async fn initial_status_is_stopped() {
        let source = OtelSource::builder("otel")
            .with_grpc_bind("127.0.0.1:0")
            .build()
            .unwrap();
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }

    #[test]
    fn config_debug_redacts_auth_token() {
        let config = OtelSourceConfig {
            auth_token: Some("super-s3cret".to_string()),
            ..OtelSourceConfig::default()
        };
        assert!(!format!("{config:?}").contains("super-s3cret"));
    }

    #[test]
    fn durability_enables_replay() {
        let source = OtelSource::builder("otel")
            .with_durability(drasi_lib::DurabilityConfig {
                enabled: true,
                ..Default::default()
            })
            .build()
            .unwrap();
        assert!(source.supports_replay());
    }

    #[tokio::test]
    async fn start_fails_when_bind_fails() {
        let source = OtelSource::builder("otel")
            .with_grpc_bind("127.0.0.1:1")
            .build()
            .unwrap();
        let err = source
            .start()
            .await
            .expect_err("privileged port should fail");
        assert!(
            err.to_string().contains("bind") || err.to_string().contains("Permission"),
            "{err}"
        );
        assert_eq!(source.status().await, ComponentStatus::Error);
    }

    #[tokio::test]
    async fn resume_from_short_position_is_rejected() {
        let source = OtelSource::builder("otel")
            .with_durability(drasi_lib::DurabilityConfig {
                enabled: true,
                ..Default::default()
            })
            .build()
            .unwrap();
        *source.wal.write().await = Some(Arc::new(MemoryWal::default()));
        let settings = drasi_lib::config::SourceSubscriptionSettings {
            source_id: "otel".to_string(),
            query_id: "q".to_string(),
            nodes: Default::default(),
            relations: Default::default(),
            enable_bootstrap: false,
            resume_from: Some(bytes::Bytes::from_static(&[1, 2, 3])),
            request_position_handle: false,
        };
        let err = match source.subscribe(settings).await {
            Ok(_) => panic!("short resume"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("8 bytes"));
    }

    #[tokio::test]
    async fn resume_from_pruned_position_is_unavailable() {
        let source = OtelSource::builder("otel")
            .with_durability(drasi_lib::DurabilityConfig {
                enabled: true,
                ..Default::default()
            })
            .build()
            .unwrap();
        *source.wal.write().await = Some(Arc::new(MemoryWal {
            oldest: 10,
            unavailable: true,
        }));
        let settings = drasi_lib::config::SourceSubscriptionSettings {
            source_id: "otel".to_string(),
            query_id: "q".to_string(),
            nodes: Default::default(),
            relations: Default::default(),
            enable_bootstrap: false,
            resume_from: Some(bytes::Bytes::copy_from_slice(&1u64.to_be_bytes())),
            request_position_handle: false,
        };
        let err = match source.subscribe(settings).await {
            Ok(_) => panic!("pruned resume"),
            Err(err) => err,
        };
        let source_err = err
            .downcast_ref::<SourceError>()
            .expect("SourceError::PositionUnavailable");
        assert!(matches!(
            source_err,
            SourceError::PositionUnavailable { .. }
        ));
    }

    #[derive(Default)]
    struct MemoryWal {
        oldest: u64,
        unavailable: bool,
    }

    #[async_trait]
    impl WalProvider for MemoryWal {
        async fn register(
            &self,
            _source_id: &str,
            _config: drasi_lib::wal::WriteAheadLogConfig,
        ) -> std::result::Result<(), drasi_lib::wal::WalError> {
            Ok(())
        }

        async fn append(
            &self,
            _source_id: &str,
            _event: &drasi_core::models::SourceChange,
        ) -> std::result::Result<u64, drasi_lib::wal::WalError> {
            Ok(1)
        }

        async fn read_from(
            &self,
            source_id: &str,
            sequence: u64,
        ) -> std::result::Result<
            Vec<(u64, drasi_core::models::SourceChange)>,
            drasi_lib::wal::WalError,
        > {
            if self.unavailable {
                return Err(drasi_lib::wal::WalError::PositionUnavailable {
                    source_id: source_id.to_string(),
                    requested: sequence,
                    oldest_available: Some(self.oldest),
                });
            }
            Ok(vec![])
        }

        async fn prune_up_to(
            &self,
            _source_id: &str,
            _sequence: u64,
        ) -> std::result::Result<u64, drasi_lib::wal::WalError> {
            Ok(0)
        }

        async fn head_sequence(
            &self,
            _source_id: &str,
        ) -> std::result::Result<u64, drasi_lib::wal::WalError> {
            Ok(0)
        }

        async fn oldest_sequence(
            &self,
            _source_id: &str,
        ) -> std::result::Result<Option<u64>, drasi_lib::wal::WalError> {
            Ok(Some(self.oldest))
        }

        async fn event_count(
            &self,
            _source_id: &str,
        ) -> std::result::Result<u64, drasi_lib::wal::WalError> {
            Ok(0)
        }

        async fn delete_wal(
            &self,
            _source_id: &str,
        ) -> std::result::Result<(), drasi_lib::wal::WalError> {
            Ok(())
        }
    }
}
