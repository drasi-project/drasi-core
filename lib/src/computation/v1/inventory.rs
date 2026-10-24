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

use super::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// A registered root graph and the component ownership path to a nested graph.
/// Names in separate graphs never alias, even when graph/component IDs repeat.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComputationScope {
    pub root_graph: Arc<str>,
    pub owners: Vec<ComponentId>,
}

impl ComputationScope {
    pub fn root(graph: impl Into<Arc<str>>) -> Self {
        Self {
            root_graph: graph.into(),
            owners: Vec::new(),
        }
    }

    pub fn nested(&self, owner: ComponentId) -> Self {
        let mut scope = self.clone();
        scope.owners.push(owner);
        scope
    }

    pub fn entity(&self, entity: GraphEntityId) -> ScopedGraphEntityId {
        ScopedGraphEntityId {
            scope: self.clone(),
            entity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScopedGraphEntityId {
    pub scope: ComputationScope,
    pub entity: GraphEntityId,
}

#[derive(Debug, Clone)]
pub struct ComputationScopeOwner {
    pub component: ScopedGraphEntityId,
    pub generation: ComponentGeneration,
}

#[derive(Debug, Clone)]
pub struct ComputationScopeSnapshot {
    /// `None` denotes a graph registered directly with the DrasiLib instance.
    pub owner: Option<ComputationScopeOwner>,
    pub topology: ComputationTopology,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScopedGraphEntityLink {
    pub from: ScopedGraphEntityId,
    pub to: ScopedGraphEntityId,
    pub kind: GraphEntityLinkKind,
}

/// Instance-wide view of host-visible native scopes, including query execution
/// graphs. Membership comes exclusively from graph-owned instances.
/// Each scope is a coherent publication; this is not a transaction across graphs.
/// No runtime handles, resolved credentials or construction recipes are retained.
#[derive(Debug, Clone)]
pub struct ComputationInventory {
    pub instance_id: Arc<str>,
    pub scopes: BTreeMap<ComputationScope, ComputationScopeSnapshot>,
    /// Includes local topology links and exact host-known cross-scope references.
    pub links: BTreeSet<ScopedGraphEntityLink>,
}

impl ComputationInventory {
    pub(crate) fn new(instance_id: impl Into<Arc<str>>) -> Self {
        Self {
            instance_id: instance_id.into(),
            scopes: BTreeMap::new(),
            links: BTreeSet::new(),
        }
    }

    pub fn entities(&self) -> impl Iterator<Item = (ScopedGraphEntityId, &GraphEntity)> {
        self.scopes.iter().flat_map(|(scope, snapshot)| {
            snapshot
                .topology
                .nodes
                .iter()
                .map(move |(id, entity)| (scope.entity(id.clone()), entity))
        })
    }

    pub fn dependencies<'a>(
        &'a self,
        dependent: &'a ScopedGraphEntityId,
    ) -> impl Iterator<Item = &'a ScopedGraphEntityLink> {
        self.links
            .iter()
            .filter(move |link| &link.from == dependent && link.kind.is_dependency())
    }

    pub fn dependents<'a>(
        &'a self,
        dependency: &'a ScopedGraphEntityId,
    ) -> impl Iterator<Item = &'a ScopedGraphEntityLink> {
        self.links
            .iter()
            .filter(move |link| &link.to == dependency && link.kind.is_dependency())
    }

    pub fn plugin_dependents(&self, plugin: &PluginIdentity) -> BTreeSet<ScopedGraphEntityId> {
        self.links
            .iter()
            .filter(|link| {
                link.kind == GraphEntityLinkKind::DependsOnPlugin
                    && link.to.entity == GraphEntityId::Plugin(plugin.clone())
            })
            .map(|link| link.from.clone())
            .collect()
    }

    /// Distinct component dependents across all represented versions and scopes.
    pub fn plugin_family_dependents(&self, plugin_id: &str) -> BTreeSet<ScopedGraphEntityId> {
        let family = GraphEntityId::PluginFamily(Arc::from(plugin_id));
        let versions: BTreeSet<_> = self
            .links
            .iter()
            .filter(|link| {
                link.kind == GraphEntityLinkKind::VersionOfPlugin && link.to.entity == family
            })
            .map(|link| link.from.clone())
            .collect();
        self.links
            .iter()
            .filter(|link| {
                link.kind == GraphEntityLinkKind::DependsOnPlugin && versions.contains(&link.to)
            })
            .map(|link| link.from.clone())
            .collect()
    }

    pub(crate) fn insert(
        &mut self,
        scope: ComputationScope,
        owner: Option<ComputationScopeOwner>,
        topology: ComputationTopology,
    ) -> GraphResult<()> {
        if self.scopes.contains_key(&scope) {
            return Err(GraphError::Topology {
                reason: "duplicate computation inventory scope".into(),
            });
        }
        self.links
            .extend(topology.links.iter().map(|link| ScopedGraphEntityLink {
                from: scope.entity(link.from.clone()),
                to: scope.entity(link.to.clone()),
                kind: link.kind.clone(),
            }));
        self.scopes
            .insert(scope, ComputationScopeSnapshot { owner, topology });
        Ok(())
    }
}
