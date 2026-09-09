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

use std::collections::HashMap;

use anyhow::Context;
use async_trait::async_trait;
use log::{error, info};

use drasi_lib::channels::ComponentStatus;
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::CheckpointState;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::Reaction;

use crate::client::A2AClient;
use crate::config::A2AReactionConfig;
use crate::process::{build_handlebars, run_loop};
use crate::A2AReactionBuilder;

pub struct A2AReaction {
    pub(crate) base: ReactionBase,
    pub(crate) config: A2AReactionConfig,
}

impl A2AReaction {
    pub fn builder(id: impl Into<String>) -> A2AReactionBuilder {
        A2AReactionBuilder::new(id)
    }

    pub fn new(id: impl Into<String>, queries: Vec<String>, config: A2AReactionConfig) -> Self {
        let params = ReactionBaseParams::new(id.into(), queries);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: A2AReactionConfig,
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
impl Reaction for A2AReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "a2a"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let dto = crate::descriptor::A2AReactionConfigDto::from(&self.config);
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
        log_component_start("A2A Reaction", &self.base.id);
        info!(
            "[{}] A2A reaction starting - endpoint: {}",
            self.base.id, self.config.endpoint
        );

        if let Err(error) = self.config.validate(&self.base.queries, None) {
            error!("[{}] Invalid A2A reaction config: {error:#}", self.base.id);
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("Invalid A2A reaction config: {error:#}")),
                )
                .await;
            return Err(error).context("A2A reaction start aborted due to invalid config");
        }

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting A2A reaction".to_string()),
            )
            .await;

        let client = A2AClient::new(
            self.config.endpoint.clone(),
            self.config.token.clone(),
            self.config.timeout_ms,
        )
        .context("failed creating A2A client")?;

        let shutdown_rx = self.base.create_shutdown_channel().await;
        let reaction_name = self.base.id.clone();
        let checkpoints = CheckpointState::load(&self.base).await;
        let policy = self
            .base
            .recovery_policy
            .unwrap_or_else(|| self.default_recovery_policy());
        let base = self.base.clone_shared();
        let config = self.config.clone();
        let handlebars = build_handlebars();

        let handle = tokio::spawn(run_loop(
            reaction_name,
            base,
            config,
            client,
            handlebars,
            shutdown_rx,
            checkpoints,
            policy,
        ));

        self.base.set_processing_task(handle).await;
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("A2A reaction started".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base
            .stop_common()
            .await
            .context("failed stopping A2A reaction")
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
            .context("failed to enqueue query result into A2A reaction")
    }

    fn is_durable(&self) -> bool {
        true
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }
}
