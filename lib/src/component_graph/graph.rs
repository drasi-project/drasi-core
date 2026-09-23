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

use crate::{
    channels::{ComponentEvent, ComponentEventBroadcastReceiver},
    computation::{runtime::Runtime, v1::ComputationInspector},
};

use super::{ComponentNode, GraphSnapshot, RelationshipKind};

/// Read-only compatibility view of an instance's ComputationGraph.
///
/// This handle owns no topology, component instances, or lifecycle state.
/// Every snapshot is derived from one authoritative controller publication.
/// Obtain it from [`crate::DrasiLib::component_graph`]; mutate the runtime through
/// DrasiLib operations or its computation controller instead.
#[derive(Clone)]
pub struct ComponentGraph {
    runtime: Weak<Runtime>,
}

impl ComponentGraph {
    pub(crate) fn from_runtime(runtime: &Arc<Runtime>) -> Self {
        Self {
            runtime: Arc::downgrade(runtime),
        }
    }

    fn runtime(&self) -> anyhow::Result<Arc<Runtime>> {
        self.runtime
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("instance runtime was dropped"))
    }

    pub async fn snapshot(&self) -> anyhow::Result<GraphSnapshot> {
        self.runtime()?.graph_snapshot().await
    }

    pub async fn get_events(&self, id: &str) -> anyhow::Result<Vec<ComponentEvent>> {
        Ok(self.runtime()?.component_events(id).await)
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
    pub fn get_component(&self, id: &str) -> Option<&ComponentNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.get_component(id).is_some()
    }

    pub fn get_dependents(&self, id: &str) -> Vec<&ComponentNode> {
        self.get_neighbors(id, &RelationshipKind::Feeds)
    }

    pub fn get_dependencies(&self, id: &str) -> Vec<&ComponentNode> {
        self.get_neighbors(id, &RelationshipKind::SubscribesTo)
    }

    pub fn get_neighbors(&self, id: &str, relationship: &RelationshipKind) -> Vec<&ComponentNode> {
        self.edges
            .iter()
            .filter(|edge| edge.from == id && &edge.relationship == relationship)
            .filter_map(|edge| self.get_component(&edge.to))
            .collect()
    }
}
