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

//! Agent Host Protocol (AHP) source plugin for Drasi.
//!
//! Connects to an [AHP](https://github.com/microsoft/agent-host-protocol)
//! host over WebSocket and turns its agent sessions, chats, pending input
//! requests, agents, and automations into a live Drasi graph, so continuous
//! queries can react to things like "a session is waiting for my input" or
//! "an agent changed more than 20 files".
//!
//! # Configuration
//!
//! The source is configured via [`AhpSourceConfig`]:
//!
//! - `url`: WebSocket URL of the AHP host (`ws://` or `wss://`)
//! - `bearerToken`: (Optional) sent as `Authorization: Bearer` on connect
//! - `clientId`: (Optional) `initialize` client ID, default `drasi-<source id>`
//! - `resourceTokens`: (Optional) tokens pushed with the AHP `authenticate` command
//! - `providers`: (Optional) only include sessions of these agent providers
//! - `includeArchived`: include archived sessions (default `true`)
//! - `subscribeSessions`: subscribe to session channels for chats and input
//!   requests (default `true`)
//! - `includeAutomations`: mirror the automation catalogue (default `true`)
//! - timeouts, keepalive, and reconnect backoff settings
//!
//! # Data format
//!
//! The graph schema (labels, IDs, properties) is defined in
//! [`drasi_ahp_common::model`] and documented in the crate README:
//!
//! ```text
//! (:Session)-[:USES_AGENT]->(:Agent)
//! (:Session)-[:HAS_CHAT]->(:Chat)
//! (:Session)-[:NEEDS_INPUT]->(:InputRequest)-[:IN_CHAT]->(:Chat)
//! (:Automation)-[:HAS_RUN]->(:AutomationRun)-[:CREATED_SESSION]->(:Session)
//! ```
//!
//! # Example
//!
//! ```rust,no_run
//! use drasi_source_ahp::AhpSource;
//!
//! # fn main() -> anyhow::Result<()> {
//! let source = AhpSource::builder("agents")
//!     .with_url("ws://localhost:4321")
//!     .with_providers(vec!["copilot".to_string()])
//!     .build()?;
//! # Ok(())
//! # }
//! ```

pub mod config;
pub mod descriptor;
mod stream;

pub use config::AhpSourceConfig;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use drasi_ahp_common::{labels, relations, ResourceToken};
use drasi_lib::channels::*;
use drasi_lib::schema::{NodeSchema, RelationSchema, SourceSchema};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::Source;
use tracing::Instrument;

/// Drasi source that mirrors an AHP host into a graph.
///
/// The source keeps one WebSocket connection open, applies every AHP
/// `action` with the official reducers, and emits the resulting graph diff.
/// It is volatile (no replay): after a reconnect it re-reads full snapshots
/// and emits the difference against the last emitted graph.
pub struct AhpSource {
    /// Shared source plumbing (dispatch, status, lifecycle).
    base: SourceBase,
    /// Resolved configuration.
    config: AhpSourceConfig,
}

impl AhpSource {
    /// Start building an AHP source.
    pub fn builder(id: impl Into<String>) -> AhpSourceBuilder {
        AhpSourceBuilder::new(id)
    }

    /// Mutable access to the base, used by the plugin descriptor.
    pub fn base_mut(&mut self) -> &mut SourceBase {
        &mut self.base
    }

    /// The resolved configuration.
    pub fn config(&self) -> &AhpSourceConfig {
        &self.config
    }
}

#[async_trait]
impl Source for AhpSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "ahp"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.base.properties_or_serialize(&self.config)
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    fn supports_replay(&self) -> bool {
        false
    }

    fn describe_schema(&self) -> Option<SourceSchema> {
        let rel = |label: &str, from: &str, to: &str| RelationSchema {
            label: label.to_string(),
            from: Some(from.to_string()),
            to: Some(to.to_string()),
            properties: Vec::new(),
        };
        Some(SourceSchema {
            nodes: [
                labels::AGENT,
                labels::SESSION,
                labels::CHAT,
                labels::INPUT_REQUEST,
                labels::AUTOMATION,
                labels::AUTOMATION_RUN,
            ]
            .into_iter()
            .map(NodeSchema::new)
            .collect(),
            relations: vec![
                rel(relations::USES_AGENT, labels::SESSION, labels::AGENT),
                rel(relations::HAS_CHAT, labels::SESSION, labels::CHAT),
                rel(
                    relations::NEEDS_INPUT,
                    labels::SESSION,
                    labels::INPUT_REQUEST,
                ),
                rel(relations::IN_CHAT, labels::INPUT_REQUEST, labels::CHAT),
                rel(
                    relations::HAS_RUN,
                    labels::AUTOMATION,
                    labels::AUTOMATION_RUN,
                ),
                rel(
                    relations::CREATED_SESSION,
                    labels::AUTOMATION_RUN,
                    labels::SESSION,
                ),
            ],
        })
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }
        self.config.validate()?;
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting AHP source".to_string()),
            )
            .await;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        self.base.set_shutdown_tx(shutdown_tx).await;
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

        let source_id = self.base.id.clone();
        let client_config = self.config.client_config(&source_id);
        let timing = stream::StreamTiming {
            keepalive: (self.config.keepalive_interval_ms > 0)
                .then(|| Duration::from_millis(self.config.keepalive_interval_ms)),
            reconnect_initial: Duration::from_millis(self.config.reconnect_initial_delay_ms),
            reconnect_max: Duration::from_millis(self.config.reconnect_max_delay_ms),
        };
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();
        let span = tracing::info_span!(
            "ahp_source_task",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );
        let task = tokio::spawn(
            stream::run(
                self.base.clone_shared(),
                source_id.clone(),
                client_config,
                timing,
                shutdown_rx,
                ready_tx,
            )
            .instrument(span),
        );
        self.base.set_task_handle(task).await;

        // Wait for the initial snapshot so the streaming baseline precedes any
        // query bootstrap snapshot; otherwise changes in between would be lost.
        let wait = Duration::from_millis(self.config.startup_sync_timeout_ms);
        match tokio::time::timeout(wait, ready_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(fatal))) => {
                return Err(anyhow::anyhow!(
                    "AHP source '{source_id}' failed to start: {fatal}"
                ));
            }
            Ok(Err(_)) => {
                return Err(anyhow::anyhow!(
                    "AHP source '{source_id}' streaming task exited during startup"
                ));
            }
            Err(_) => log::warn!(
                "AHP source '{source_id}' has not synchronised with {} after {wait:?}; \
                 continuing in the background (the first snapshot will be emitted as inserts)",
                self.config.url
            ),
        }
        self.base
            .set_status(
                ComponentStatus::Running,
                Some(format!("Streaming from AHP host {}", self.config.url)),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let status = self.base.get_status().await;
        if status != ComponentStatus::Running && status != ComponentStatus::Error {
            return Ok(());
        }
        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping AHP source".to_string()),
            )
            .await;
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base.subscribe_with_bootstrap(&settings, "AHP").await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn deprovision(&self) -> Result<()> {
        self.base.deprovision_common().await
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

/// Builder for [`AhpSource`].
pub struct AhpSourceBuilder {
    id: String,
    config: AhpSourceConfig,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl AhpSourceBuilder {
    /// Create a builder with default configuration.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: AhpSourceConfig::default(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    /// Replace the whole configuration.
    pub fn with_config(mut self, config: AhpSourceConfig) -> Self {
        self.config = config;
        self
    }

    /// AHP host WebSocket URL.
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.config.url = url.into();
        self
    }

    /// Bearer token for the WebSocket upgrade request.
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.config.bearer_token = Some(token.into());
        self
    }

    /// Client ID sent in `initialize`.
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.config.client_id = Some(client_id.into());
        self
    }

    /// Add a token pushed with the AHP `authenticate` command.
    pub fn with_resource_token(
        mut self,
        resource: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        self.config.resource_tokens.push(ResourceToken {
            resource: resource.into(),
            token: token.into(),
        });
        self
    }

    /// Restrict sessions to these agent providers.
    pub fn with_providers(mut self, providers: Vec<String>) -> Self {
        self.config.providers = providers;
        self
    }

    /// Include or exclude archived sessions.
    pub fn with_include_archived(mut self, include: bool) -> Self {
        self.config.include_archived = include;
        self
    }

    /// Subscribe to session channels for chats and input requests.
    pub fn with_subscribe_sessions(mut self, subscribe: bool) -> Self {
        self.config.subscribe_sessions = subscribe;
        self
    }

    /// Mirror the automation catalogue.
    pub fn with_include_automations(mut self, include: bool) -> Self {
        self.config.include_automations = include;
        self
    }

    /// Keepalive ping interval; `Duration::ZERO` disables it.
    pub fn with_keepalive_interval(mut self, interval: Duration) -> Self {
        self.config.keepalive_interval_ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX);
        self
    }

    /// Reconnect backoff bounds.
    pub fn with_reconnect_delay(mut self, initial: Duration, max: Duration) -> Self {
        self.config.reconnect_initial_delay_ms =
            u64::try_from(initial.as_millis()).unwrap_or(u64::MAX);
        self.config.reconnect_max_delay_ms = u64::try_from(max.as_millis()).unwrap_or(u64::MAX);
        self
    }

    /// How long `start()` waits for the initial snapshot.
    pub fn with_startup_sync_timeout(mut self, timeout: Duration) -> Self {
        self.config.startup_sync_timeout_ms =
            u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        self
    }

    /// Dispatch mode (Channel by default).
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Dispatch buffer capacity.
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Bootstrap provider (normally the `drasi-bootstrap-ahp` provider).
    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Whether DrasiLib starts the source automatically.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Validate and build the source.
    pub fn build(self) -> Result<AhpSource> {
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
        Ok(AhpSource {
            base: SourceBase::new(params)?,
            config: self.config,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_applies_settings() {
        let source = AhpSource::builder("agents")
            .with_url("ws://example:1234")
            .with_bearer_token("secret")
            .with_providers(vec!["copilot".into()])
            .with_include_archived(false)
            .with_resource_token("https://api", "tok")
            .build()
            .expect("valid");
        assert_eq!(source.id(), "agents");
        assert_eq!(source.type_name(), "ahp");
        assert!(!source.supports_replay());
        let props = source.properties();
        assert_eq!(props["url"], serde_json::json!("ws://example:1234"));
        // properties() is the persistence hook and must include secrets.
        assert_eq!(props["bearerToken"], serde_json::json!("secret"));
        assert_eq!(props["includeArchived"], serde_json::json!(false));
        assert_eq!(source.config().resource_tokens.len(), 1);
    }

    #[test]
    fn builder_rejects_invalid_url() {
        assert!(AhpSource::builder("x")
            .with_url("not a url")
            .build()
            .is_err());
    }

    #[test]
    fn schema_lists_labels() {
        let source = AhpSource::builder("a").build().expect("valid");
        let schema = source.describe_schema().expect("schema");
        assert_eq!(schema.nodes.len(), 6);
        assert_eq!(schema.relations.len(), 6);
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "ahp-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::AhpSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
