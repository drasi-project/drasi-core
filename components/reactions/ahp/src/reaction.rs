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

//! The [`AhpReaction`] type and its [`Reaction`] implementation.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use async_trait::async_trait;
use log::{error, info};
use tokio::sync::Mutex;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::Reaction;

use crate::config::AhpReactionConfig;
use crate::host::AhpHost;
use crate::process::{build_handlebars, host_settings, Processor};
use crate::AhpReactionBuilder;

/// Drasi reaction that turns continuous-query changes into agent work on an
/// Agent Host Protocol (AHP) host.
pub struct AhpReaction {
    pub(crate) base: ReactionBase,
    pub(crate) config: AhpReactionConfig,
}

impl AhpReaction {
    /// Start building an AHP reaction with the given id.
    pub fn builder(id: impl Into<String>) -> AhpReactionBuilder {
        AhpReactionBuilder::new(id)
    }

    /// The reaction's configuration.
    pub fn config(&self) -> &AhpReactionConfig {
        &self.config
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: AhpReactionConfig,
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
}

#[async_trait]
impl Reaction for AhpReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "ahp"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let dto = crate::descriptor::AhpReactionConfigDto::from(&self.config);
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

    async fn start(&self) -> anyhow::Result<()> {
        log_component_start("AHP Reaction", &self.base.id);
        if let Err(error) = self.config.validate(&self.base.queries, None) {
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("Invalid AHP reaction config: {error:#}")),
                )
                .await;
            return Err(error).context("AHP reaction start aborted due to invalid config");
        }
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting AHP reaction".to_string()),
            )
            .await;
        info!(
            "[{}] AHP reaction starting - endpoint: {}, mode: {:?}",
            self.base.id, self.config.endpoint, self.config.mode
        );

        let processor = Arc::new(Mutex::new(Processor {
            reaction_id: self.base.id.clone(),
            base: self.base.clone_shared(),
            config: self.config.clone(),
            host: AhpHost::new(host_settings(&self.base.id, &self.config)),
            handlebars: build_handlebars(),
            cache: HashMap::new(),
        }));
        let shutdown_rx = self.base.create_shutdown_channel().await;
        let policy = self.default_recovery_policy();
        let base = self.base.clone_shared();
        let reaction_id = self.base.id.clone();

        let task = tokio::spawn(async move {
            let result = base
                .run_standard_loop(shutdown_rx, HashMap::new(), policy, |query_result| {
                    let processor = processor.clone();
                    async move { processor.lock().await.process(&query_result).await }
                })
                .await;
            processor.lock().await.host.disconnect().await;
            if let Err(error) = result {
                error!("[{reaction_id}] AHP reaction processing stopped: {error:#}");
            }
        });

        self.base.set_processing_task(task).await;
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("AHP reaction started".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base
            .stop_common()
            .await
            .context("failed stopping AHP reaction")
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        self.base
            .enqueue_query_result(result)
            .await
            .context("failed to enqueue query result into AHP reaction")
    }

    fn is_durable(&self) -> bool {
        self.config.durable
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        self.base
            .resolved_recovery_policy(ReactionRecoveryPolicy::Strict)
    }

    async fn deprovision(&self) -> anyhow::Result<()> {
        self.base.deprovision_common().await
    }
}
