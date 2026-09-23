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

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;

use crate::{
    channels::{ComponentEvent, ComponentStatus, ComponentType},
    computation::runtime::Runtime,
    config::ReactionRuntime,
    managers::LogMessage,
    metrics::{LifecycleMetrics, ReactionMetricsSnapshot},
    reactions::Reaction,
};

/// Reaction operations on the instance's sole ComputationGraph.
///
/// Recovery, subscription tasks, and cleanup are owned by graph-hosted plugin
/// adapters, never by this facade.
#[derive(Clone)]
pub struct ReactionManager {
    runtime: Arc<Runtime>,
}

impl ReactionManager {
    pub(crate) fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    pub async fn provision_reaction(&self, reaction: impl Reaction + 'static) -> Result<()> {
        self.runtime.declare_reaction(Box::new(reaction)).await
    }

    pub async fn start_reaction(&self, id: String) -> Result<()> {
        self.runtime.start_component(&id, "reaction").await
    }

    pub async fn stop_reaction(&self, id: String) -> Result<()> {
        self.runtime.stop_component(&id, "reaction").await
    }

    pub async fn get_reaction_status(&self, id: String) -> Result<ComponentStatus> {
        self.runtime.component_status(&id, "reaction").await
    }

    pub async fn get_reaction(&self, id: String) -> Result<ReactionRuntime> {
        self.runtime.reaction_info(&id).await
    }

    pub async fn get_reaction_instance(&self, id: &str) -> Result<Arc<dyn Reaction>> {
        self.runtime.reaction(id).await
    }

    pub async fn teardown_reaction(&self, id: String, cleanup: bool) -> Result<()> {
        self.runtime
            .remove_component(&id, "reaction", cleanup)
            .await
    }

    pub async fn update_reaction(
        &self,
        id: String,
        reaction: impl Reaction + 'static,
    ) -> Result<()> {
        self.runtime.update_reaction(&id, Box::new(reaction)).await
    }

    pub async fn list_reactions(&self) -> Vec<(String, ComponentStatus)> {
        self.runtime
            .list_components("reaction")
            .await
            .expect("initialized computation registry")
    }

    pub async fn start_all(&self) -> Result<()> {
        self.runtime.start_kind("reaction").await
    }

    pub async fn stop_all(&self) -> Result<()> {
        self.runtime.stop_kind("reaction").await
    }

    pub async fn get_reaction_events(&self, id: &str) -> Vec<ComponentEvent> {
        self.runtime.component_events(id).await
    }

    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        self.runtime.events(Some(ComponentType::Reaction)).await
    }

    pub async fn get_reaction_metrics(
        &self,
        id: &str,
    ) -> Result<HashMap<String, ReactionMetricsSnapshot>> {
        self.runtime.reaction_metrics(id).await
    }

    pub fn lifecycle_metrics(&self) -> &Arc<LifecycleMetrics> {
        &self.runtime.lifecycle_metrics
    }

    pub async fn subscribe_logs(
        &self,
        id: &str,
    ) -> Option<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        self.runtime.subscribe_logs(id, "reaction").await.ok()
    }

    pub async fn subscribe_events(
        &self,
        id: &str,
    ) -> Option<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.runtime.subscribe_events(id, "reaction").await.ok()
    }
}
