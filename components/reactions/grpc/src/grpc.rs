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

//! gRPC reaction (fixed + adaptive batching).

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use log::info;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::Reaction;

pub use super::config::{
    BatchingConfig, GrpcQueryConfig, GrpcReactionConfig, GrpcTemplateExtension, OutputFormat,
    OutputTemplates,
};
use super::GrpcReactionBuilder;
use crate::runner_adaptive::{self, AdaptiveRunnerParams};
use crate::runner_fixed::{self, FixedRunnerParams};

pub struct GrpcReaction {
    pub(crate) base: ReactionBase,
    config: GrpcReactionConfig,
}

impl GrpcReaction {
    pub fn builder(id: impl Into<String>) -> GrpcReactionBuilder {
        GrpcReactionBuilder::new(id)
    }

    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: GrpcReactionConfig,
    ) -> anyhow::Result<Self> {
        config.validate(&queries)?;
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: GrpcReactionConfig,
        priority_queue_capacity: usize,
    ) -> anyhow::Result<Self> {
        config.validate(&queries)?;
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: GrpcReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
        recovery_policy: Option<ReactionRecoveryPolicy>,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        if let Some(policy) = recovery_policy {
            params = params.with_recovery_policy(policy);
        }
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    pub fn config(&self) -> &GrpcReactionConfig {
        &self.config
    }

    /// Mutable access to the underlying [`ReactionBase`]. The dynamic-plugin
    /// descriptor uses this to call `set_raw_config` after construction.
    pub fn base_mut(&mut self) -> &mut ReactionBase {
        &mut self.base
    }
}

#[async_trait]
impl Reaction for GrpcReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "grpc"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use crate::descriptor::GrpcReactionConfigDto;
        let dto = GrpcReactionConfigDto::from(&self.config);
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
        log_component_start("gRPC Reaction", &self.base.id);
        info!(
            "[{}] gRPC reaction starting - sending to endpoint: {} (mode: {})",
            self.base.id,
            self.config.endpoint,
            match &self.config.batching {
                BatchingConfig::Fixed { .. } => "fixed",
                BatchingConfig::Adaptive { .. } => "adaptive",
            }
        );

        // Warn on a no-op `outputTemplates` block — present but with no
        // default template and no routes. Items will emit carrying only the
        // raw `before` / `after` row state, which is the same outcome as
        // omitting `outputTemplates` entirely; surface this so a
        // misconfiguration doesn't silently disable the templating path.
        // Non-fatal.
        if let Some(ref t) = self.config.output_templates {
            if t.default_template.is_none() && t.routes.is_empty() {
                log::warn!(
                    "[{}] `outputTemplates` is configured but contains no `defaultTemplate` and \
                     no `routes` — reaction will emit items carrying only the raw `before` / \
                     `after` row state. Either add a template or omit the `outputTemplates` \
                     block entirely.",
                    self.base.id
                );
            }
        }

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting gRPC reaction".to_string()),
            )
            .await;

        let shutdown_rx = self.base.create_shutdown_channel().await;
        let reaction_name = self.base.id.clone();
        let checkpoints = crate::checkpoint::CheckpointState::load(&self.base).await;
        let policy = self
            .base
            .recovery_policy
            .unwrap_or_else(|| self.default_recovery_policy());
        let base = self.base.clone_shared();
        let config = self.config.clone();

        let handle = match &self.config.batching {
            BatchingConfig::Fixed {
                batch_size,
                batch_flush_timeout_ms,
            } => {
                let params = FixedRunnerParams {
                    reaction_name,
                    batch_size: *batch_size,
                    batch_flush_timeout_ms: *batch_flush_timeout_ms,
                    base,
                    config,
                    shutdown_rx,
                    checkpoints,
                    policy,
                };
                tokio::spawn(async move { runner_fixed::run(params).await })
            }
            adaptive @ BatchingConfig::Adaptive { .. } => {
                let params = AdaptiveRunnerParams {
                    reaction_name,
                    adaptive: adaptive.as_adaptive_config().unwrap_or_default(),
                    base,
                    config,
                    shutdown_rx,
                    checkpoints,
                    policy,
                };
                tokio::spawn(async move { runner_adaptive::run(params).await })
            }
        };

        self.base.set_processing_task(handle).await;

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("gRPC reaction started".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }

    /// The gRPC reaction is a stateless **trigger** reaction (design archetype 2a):
    /// it fires per-change side effects and does not require a durable state store.
    fn is_durable(&self) -> bool {
        false
    }

    /// Trigger reactions must not replay historical state on a fresh start — that
    /// would fire side effects for the entire query history.
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    /// At-least-once delivery: on a sustained delivery failure the reaction fails
    /// fast so the un-acked batch replays from the query outbox on restart.
    /// Operators can override to `AutoSkipGap` for uptime-over-completeness.
    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }
}
