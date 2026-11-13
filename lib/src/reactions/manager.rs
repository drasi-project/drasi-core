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

use crate::{ComponentId, ComponentName};
use anyhow::Result;

use crate::{
    channels::{ComponentEvent, ComponentStatus, ComponentType},
    computation::runtime::Runtime,
    config::ReactionRuntime,
    metrics::{LifecycleMetrics, ReactionMetricsSnapshot},
    observability::LogMessage,
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

    pub async fn provision_reaction(
        &self,
        name: crate::config::ComponentName,
        reaction: impl Reaction + 'static,
    ) -> Result<()> {
        self.runtime
            .declare_reaction(name, Box::new(reaction))
            .await
    }

    pub async fn start_reaction_by_id(&self, id: &ComponentId) -> Result<()> {
        self.runtime.start_component(id, "reaction").await
    }

    pub async fn stop_reaction_by_id(&self, id: &ComponentId) -> Result<()> {
        self.runtime.stop_component(id, "reaction").await
    }

    pub async fn get_reaction_status_by_id(&self, id: &ComponentId) -> Result<ComponentStatus> {
        self.runtime.component_status(id, "reaction").await
    }

    pub async fn get_reaction_by_id(&self, id: &ComponentId) -> Result<ReactionRuntime> {
        self.runtime.reaction_info(id).await
    }

    pub async fn get_reaction_instance_by_id(&self, id: &ComponentId) -> Result<Arc<dyn Reaction>> {
        self.runtime.reaction(id).await
    }

    pub async fn teardown_reaction_by_id(&self, id: &ComponentId, cleanup: bool) -> Result<()> {
        self.runtime.remove_component(id, "reaction", cleanup).await
    }

    pub async fn update_reaction_by_id(
        &self,
        id: &ComponentId,
        definition: crate::management::OrdinaryComponentRecord,
    ) -> Result<()> {
        self.runtime
            .update_ordinary_definition(id, "reaction", definition)
            .await
    }

    pub async fn list_reactions(
        &self,
    ) -> crate::error::Result<Vec<crate::config::ComponentListEntry>> {
        Ok(self.runtime.list_components("reaction").await?)
    }

    pub async fn start_all(&self) -> Result<()> {
        self.runtime.start_kind("reaction").await
    }

    pub async fn stop_all(&self) -> Result<()> {
        self.runtime.stop_kind("reaction").await
    }

    pub async fn get_reaction_events_by_id(&self, id: &ComponentId) -> Result<Vec<ComponentEvent>> {
        Ok(self.runtime.component_events(id).await)
    }

    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        self.runtime.events(Some(ComponentType::Reaction)).await
    }

    pub async fn get_reaction_metrics_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<HashMap<ComponentId, ReactionMetricsSnapshot>> {
        self.runtime.reaction_metrics(id).await
    }

    pub fn lifecycle_metrics(&self) -> &Arc<LifecycleMetrics> {
        &self.runtime.lifecycle_metrics
    }

    pub async fn subscribe_logs_by_id(
        &self,
        id: &ComponentId,
    ) -> crate::error::Result<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        Ok(self.runtime.subscribe_logs(id, "reaction").await?)
    }

    pub async fn subscribe_events_by_id(
        &self,
        id: &ComponentId,
    ) -> crate::error::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        Ok(self.runtime.subscribe_events(id, "reaction").await?)
    }

    pub async fn start_reaction_by_name(&self, name: &ComponentName) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.start_reaction_by_id(&id).await
    }

    pub async fn stop_reaction_by_name(&self, name: &ComponentName) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.stop_reaction_by_id(&id).await
    }

    pub async fn get_reaction_status_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<ComponentStatus> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.get_reaction_status_by_id(&id).await
    }

    pub async fn get_reaction_by_name(&self, name: &ComponentName) -> Result<ReactionRuntime> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.get_reaction_by_id(&id).await
    }

    pub async fn get_reaction_instance_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<Arc<dyn Reaction>> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.get_reaction_instance_by_id(&id).await
    }

    pub async fn teardown_reaction_by_name(
        &self,
        name: &ComponentName,
        cleanup: bool,
    ) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.teardown_reaction_by_id(&id, cleanup).await
    }

    pub async fn update_reaction_by_name(
        &self,
        name: &ComponentName,
        definition: crate::management::OrdinaryComponentRecord,
    ) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.update_reaction_by_id(&id, definition).await
    }

    pub async fn get_reaction_events_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<Vec<ComponentEvent>> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.get_reaction_events_by_id(&id).await
    }

    pub async fn get_reaction_metrics_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<HashMap<ComponentId, ReactionMetricsSnapshot>> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.get_reaction_metrics_by_id(&id).await
    }

    pub async fn subscribe_logs_by_name(
        &self,
        name: &ComponentName,
    ) -> crate::error::Result<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.subscribe_logs_by_id(&id).await
    }

    pub async fn subscribe_events_by_name(
        &self,
        name: &ComponentName,
    ) -> crate::error::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        let id = self.runtime.component_id_by_name(name, Some("reaction"))?;
        self.subscribe_events_by_id(&id).await
    }
}
