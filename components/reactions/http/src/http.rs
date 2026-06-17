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

//! HTTP reaction supporting single per-result notifications and
//! adaptive batched delivery.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use log::{error, info};
use reqwest::Client;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;

use crate::adaptive_batcher::AdaptiveBatcherConfig;
use crate::adaptive_loop::run_adaptive_loop;
use crate::config::{AdaptiveBatchConfig, HttpReactionConfig};
use crate::process::build_handlebars;
use crate::standard_loop::run_standard_loop;
use crate::HttpReactionBuilder;

pub struct HttpReaction {
    pub(crate) base: ReactionBase,
    pub(crate) config: HttpReactionConfig,
}

impl HttpReaction {
    /// Create a builder for HttpReaction.
    pub fn builder(id: impl Into<String>) -> HttpReactionBuilder {
        HttpReactionBuilder::new(id)
    }

    /// Create a new HTTP reaction.
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: HttpReactionConfig) -> Self {
        let params = ReactionBaseParams::new(id.into(), queries);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    /// Create a new HTTP reaction with a custom priority queue capacity.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: HttpReactionConfig,
        priority_queue_capacity: usize,
    ) -> Self {
        let params = ReactionBaseParams::new(id.into(), queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: HttpReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    /// Build the pooled reqwest client.
    ///
    /// HTTP/2 is negotiated automatically via ALPN for `https://` targets;
    /// `http://` targets use HTTP/1.1. We deliberately do **not** force
    /// `http2_prior_knowledge()` (cleartext h2c), which would break the
    /// common case of HTTP/1.1 webhooks and TLS endpoints.
    fn build_client(&self) -> Result<Client> {
        Ok(Client::builder()
            .timeout(Duration::from_millis(self.config.timeout_ms))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .build()?)
    }
}

/// Convert the public `AdaptiveBatchConfig` (scalar fields used in
/// config / serialization) to the runtime `AdaptiveBatchConfig` used by
/// the batcher (`Duration`-based + enable flag).
pub(crate) fn to_runtime_adaptive(cfg: &AdaptiveBatchConfig) -> AdaptiveBatcherConfig {
    AdaptiveBatcherConfig {
        min_batch_size: cfg.adaptive_min_batch_size,
        max_batch_size: cfg.adaptive_max_batch_size,
        throughput_window: Duration::from_millis(cfg.adaptive_window_size as u64 * 100),
        max_wait_time: Duration::from_millis(cfg.adaptive_batch_timeout_ms),
        min_wait_time: Duration::from_millis(100),
        adaptive_enabled: true,
    }
}

#[async_trait]
impl Reaction for HttpReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "http"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let dto = crate::descriptor::HttpReactionConfigDto::from(&self.config);
        self.base.properties_or_serialize(&dto)
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("HTTP Reaction", &self.base.id);

        let mode = if self.config.adaptive.is_some() {
            "adaptive"
        } else {
            "standard"
        };
        info!(
            "[{}] HTTP reaction starting in {mode} mode - base URL: {}",
            self.base.id, self.config.base_url
        );

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some(format!("Starting HTTP reaction ({mode} mode)")),
            )
            .await;

        let client = match self.build_client() {
            Ok(c) => c,
            Err(e) => {
                error!("[{}] Failed to create HTTP client: {e}", self.base.id);
                self.base
                    .set_status(
                        ComponentStatus::Stopped,
                        Some(format!("Failed to create HTTP client: {e}")),
                    )
                    .await;
                return Err(e);
            }
        };

        self.base
            .set_status(
                ComponentStatus::Running,
                Some(format!("HTTP reaction running ({mode} mode)")),
            )
            .await;

        let shutdown_rx = self.base.create_shutdown_channel().await;
        let reaction_name = self.base.id.clone();
        let base = self.base.clone_shared();
        let config = self.config.clone();
        let handlebars = build_handlebars();

        let handle = if let Some(adaptive_cfg) = self.config.adaptive.as_ref() {
            let runtime_adaptive = to_runtime_adaptive(adaptive_cfg);
            tokio::spawn(run_adaptive_loop(
                reaction_name,
                base,
                config,
                runtime_adaptive,
                client,
                handlebars,
                shutdown_rx,
            ))
        } else {
            tokio::spawn(run_standard_loop(
                reaction_name,
                base,
                config,
                client,
                handlebars,
                shutdown_rx,
            ))
        };

        self.base.set_processing_task(handle).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: drasi_lib::channels::QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }

    async fn deprovision(&self) -> Result<()> {
        self.base.deprovision_common().await
    }
}
