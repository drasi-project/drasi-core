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

use crate::executor::ShellExecutor;

use super::config::ShellReactionConfig;
use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::{
    config, managers::log_component_start, reactions::Reaction, ComponentStatus, ReactionBase,
    ReactionBaseParams,
};
use std::collections::HashMap;
pub struct ShellReaction {
    base: ReactionBase,
    executor: ShellExecutor,
}

impl ShellReaction {
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: ShellReactionConfig,
    ) -> anyhow::Result<Self> {
        Self::create_internal(id, queries, config, None, None)
    }

    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: ShellReactionConfig,
        priority_queue_capacity: usize,
    ) -> anyhow::Result<Self> {
        Self::create_internal(id, queries, config, Some(priority_queue_capacity), None)
    }

    pub fn from_builder(
        id: impl Into<String>,
        queries: Vec<String>,
        config: ShellReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> anyhow::Result<Self> {
        Self::create_internal(
            id,
            queries,
            config,
            priority_queue_capacity,
            Some(auto_start),
        )
    }

    fn create_internal(
        id: impl Into<String>,
        queries: Vec<String>,
        config: ShellReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: Option<bool>,
    ) -> anyhow::Result<Self> {
        let id = id.into();

        // validate the config
        Self::validate_config(&config, &queries, id.clone())?;

        let mut params = ReactionBaseParams::new(id.clone(), queries);
        if let Some(cap) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(cap);
        }

        if let Some(auto) = auto_start {
            params = params.with_auto_start(auto);
        }

        let executor = ShellExecutor::new(id, config.clone());

        Ok(Self {
            base: ReactionBase::new(params),
            executor,
        })
    }

    fn validate_config(
        config: &ShellReactionConfig,
        queries: &Vec<String>,
        reaction_id: String,
    ) -> anyhow::Result<()> {
        config.validate(queries, &reaction_id)
    }
}

#[async_trait]
impl Reaction for ShellReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "shell"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();

        // add config properties
        props.insert(
            "max_concurrent".to_string(),
            serde_json::json!(self.executor.config.max_concurrent),
        );
        props.insert(
            "max_stdin_bytes".to_string(),
            serde_json::json!(self.executor.config.max_stdin_bytes),
        );
        props.insert(
            "max_recent_invocations".to_string(),
            serde_json::json!(self.executor.config.max_recent_invocations),
        );
        props.insert(
            "capture_limit".to_string(),
            serde_json::json!(self.executor.config.capture_limit),
        );
        props.insert(
            "timeout_s".to_string(),
            serde_json::json!(self.executor.config.timeout_s),
        );
        props.insert(
            "kill_on_drop".to_string(),
            serde_json::json!(self.executor.config.kill_on_drop),
        );

        // get dynamic state properties
        let state_properties = self.executor.state.properties();

        // final properties are a combination of config and state
        props.into_iter().chain(state_properties).collect()
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
        log_component_start("Shell Reaction", &self.base.id);

        // Transition to starting
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting Shell Reaction".to_string()),
            )
            .await;

        // Create shutdown signal channel
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Start the processing task
        let reaction_id = self.base.id.clone();
        let priority_queue = self.base.priority_queue.clone();

        // Start executor processing loop in a separate task
        let processing_task_handle = self
            .executor
            .start_processing_loop(priority_queue, shutdown_rx)
            .await?;

        // Set the processing task handle
        self.base.set_processing_task(processing_task_handle).await;

        // Transition to running
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Shell Reaction is running".to_string()),
            )
            .await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await?;

        tokio::time::sleep(tokio::time::Duration::from_micros(500)).await;

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Shell Reaction has stopped".to_string()),
            )
            .await;

        Ok(())
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
}
