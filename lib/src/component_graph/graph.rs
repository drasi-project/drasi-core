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

use std::sync::{Arc, Weak};
use tokio::sync::RwLock;

use crate::{
    channels::{ComponentEvent, ComponentEventBroadcastReceiver, ComponentStatus},
    computation::{
        runtime::Runtime,
        v1::{ComputationInspector, GraphEntityId},
    },
    ComponentId, ComponentName,
};

use super::{ComponentNode, GraphSnapshot, RelationshipKind};

/// Read-only view of an instance's ComputationGraph.
///
/// This handle owns no topology, component instances, or lifecycle state.
/// Component state is derived from one authoritative controller publication;
/// the instance root reflects DrasiLib's running flag, not aggregate readiness.
/// Obtain it from [`crate::DrasiLib::component_graph`]; mutate the runtime through
/// DrasiLib operations or its computation controller instead.
#[derive(Clone)]
pub struct ComponentGraphView {
    runtime: Weak<Runtime>,
    running: Weak<RwLock<bool>>,
}

impl ComponentGraphView {
    pub(crate) fn from_runtime(runtime: &Arc<Runtime>, running: &Arc<RwLock<bool>>) -> Self {
        Self {
            runtime: Arc::downgrade(runtime),
            running: Arc::downgrade(running),
        }
    }

    fn runtime(&self) -> anyhow::Result<Arc<Runtime>> {
        self.runtime
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("instance runtime was dropped"))
    }

    pub async fn snapshot(&self) -> anyhow::Result<GraphSnapshot> {
        let runtime = self.runtime()?;
        let running = self
            .running
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("instance lifecycle was dropped"))?;
        let status = if *running.read().await {
            ComponentStatus::Running
        } else {
            ComponentStatus::Stopped
        };
        runtime.graph_snapshot(status).await
    }

    pub async fn get_events_by_id(&self, id: &ComponentId) -> anyhow::Result<Vec<ComponentEvent>> {
        Ok(self.runtime()?.component_events(id).await)
    }

    pub async fn get_events_by_name(
        &self,
        name: &ComponentName,
    ) -> anyhow::Result<Vec<ComponentEvent>> {
        let runtime = self.runtime()?;
        let id = runtime.component_id_by_name(name, None)?;
        Ok(runtime.component_events(&id).await)
    }

    pub async fn get_all_events(&self) -> anyhow::Result<Vec<ComponentEvent>> {
        Ok(self.runtime()?.events(None).await)
    }

    pub fn subscribe(&self) -> anyhow::Result<ComponentEventBroadcastReceiver> {
        Ok(self.runtime()?.event_sender().subscribe())
    }

    pub fn inspector(&self) -> anyhow::Result<ComputationInspector> {
        self.runtime()?.inspector()
    }
}

impl GraphSnapshot {
    pub fn get_entity(&self, id: &GraphEntityId) -> Option<&ComponentNode> {
        self.nodes.iter().find(|node| &node.id == id)
    }

    pub fn get_component_by_id(&self, id: &ComponentId) -> Option<&ComponentNode> {
        self.get_entity(&GraphEntityId::Component(id.clone()))
    }

    pub fn get_component_by_name(&self, name: &ComponentName) -> Option<&ComponentNode> {
        self.nodes
            .iter()
            .find(|node| node.id.component_id().is_some() && node.name.as_ref() == Some(name))
    }

    pub fn contains_by_id(&self, id: &ComponentId) -> bool {
        self.get_component_by_id(id).is_some()
    }

    pub fn contains_by_name(&self, name: &ComponentName) -> bool {
        self.get_component_by_name(name).is_some()
    }

    pub fn get_dependents_by_id(&self, id: &ComponentId) -> Vec<&ComponentNode> {
        self.get_neighbors(
            &GraphEntityId::Component(id.clone()),
            &RelationshipKind::Feeds,
        )
    }

    pub fn get_dependents_by_name(&self, name: &ComponentName) -> Option<Vec<&ComponentNode>> {
        Some(self.get_neighbors(
            &self.get_component_by_name(name)?.id,
            &RelationshipKind::Feeds,
        ))
    }

    pub fn get_dependencies_by_id(&self, id: &ComponentId) -> Vec<&ComponentNode> {
        self.get_neighbors(
            &GraphEntityId::Component(id.clone()),
            &RelationshipKind::SubscribesTo,
        )
    }

    pub fn get_dependencies_by_name(&self, name: &ComponentName) -> Option<Vec<&ComponentNode>> {
        Some(self.get_neighbors(
            &self.get_component_by_name(name)?.id,
            &RelationshipKind::SubscribesTo,
        ))
    }

    pub fn get_neighbors(
        &self,
        id: &GraphEntityId,
        relationship: &RelationshipKind,
    ) -> Vec<&ComponentNode> {
        self.edges
            .iter()
            .filter(|edge| &edge.from == id && &edge.relationship == relationship)
            .filter_map(|edge| self.get_entity(&edge.to))
            .collect()
    }
}
