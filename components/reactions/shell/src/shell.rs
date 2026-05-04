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
#![cfg(target_os = "linux")]

use crate::executor::ShellExecutor;

use super::config::ShellReactionConfig;
use anyhow::{Result};
use async_trait::async_trait;
use drasi_lib::{ComponentStatus, ReactionBase, ReactionBaseParams, config, managers::log_component_start, reactions::Reaction};
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
        Self::create_internal(id, queries, config, priority_queue_capacity, Some(auto_start))
    }

    fn create_internal(id: impl Into<String>, queries: Vec<String>, config: ShellReactionConfig, priority_queue_capacity: Option<usize>, auto_start: Option<bool>) -> anyhow::Result<Self> {
        let id = id.into();

        // validate the config
        Self::validate_config(&config, &queries)?;

        let mut params = ReactionBaseParams::new(id, queries);
        if let Some(cap) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(cap);
        }

        if let Some(auto) = auto_start {
            params = params.with_auto_start(auto);
        }

        Ok(
            Self { base: ReactionBase::new(params), executor: ShellExecutor::new(config.clone()) }
        )
    }

    fn validate_config(config: &ShellReactionConfig, queries: &Vec<String>) -> anyhow::Result<()> {
        Ok(())
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
        HashMap::new()
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
        self.base.set_status(ComponentStatus::Starting, Some("Starting Shell Reaction".to_string())).await;

        // Create shutdown signal channel
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Start the processing task
        let executor_clone = self.executor.clone();
        let reaction_id = self.base.id.clone();
        let priority_queue = self.base.priority_queue.clone();


        // Transition to running
        self.base.set_status(ComponentStatus::Running, Some("Shell Reaction is running".to_string())).await;

        Ok(())
    }   

    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: drasi_lib::channels::QueryResult) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }

}