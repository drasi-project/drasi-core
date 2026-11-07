// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::{
    ComponentLifecycle, ComponentName, ComponentSemanticKind, RealizationState,
};

/// Safe metadata identifying an existing component in an ID or name conflict.
/// Configuration, resource bindings and failure causes are deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ComponentConflict {
    pub id: ComponentId,
    pub name: ComponentName,
    pub kind: ComponentSemanticKind,
    pub implementation: Option<ImplementationIdentity>,
    pub plugin: Option<PluginIdentity>,
    pub realization: Option<RealizationState>,
    pub lifecycle: Option<ComponentLifecycle>,
}

impl std::fmt::Display for ComponentConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "'{}' ({}, {:?})", self.name, self.id, self.kind)
    }
}

impl ComponentConflict {
    pub(super) fn new(
        descriptor: &ComponentDescriptor,
        name: &ComponentName,
        role: ComponentRole,
        specification: Option<&ComponentSpecification>,
    ) -> Self {
        Self {
            id: descriptor.id().clone(),
            name: name.clone(),
            kind: descriptor.semantic_kind().unwrap_or_else(|| role.into()),
            implementation: specification.map(|spec| spec.implementation.clone()),
            plugin: specification
                .and_then(|spec| spec.implementation.plugin.as_ref())
                .or_else(|| descriptor.plugin_identity())
                .cloned(),
            realization: None,
            lifecycle: None,
        }
    }

    pub(crate) fn from_desired(component: &DesiredComponent) -> Self {
        Self::new(
            &component.descriptor,
            &component.name,
            component.role,
            match &component.construction {
                ComponentConstruction::Factory(specification) => Some(specification),
                ComponentConstruction::External { .. } => None,
            },
        )
    }
}

impl DesiredTopology {
    pub(crate) fn validate_component_identities(&self) -> GraphResult<()> {
        let mut ids = BTreeMap::new();
        let mut names = BTreeMap::new();
        for component in &self.components {
            if let Some(existing) = ids.insert(component.descriptor.id(), component) {
                return Err(GraphError::DuplicateComponentId {
                    existing: Box::new(ComponentConflict::from_desired(existing)),
                });
            }
            if let Some(existing) = names.insert(&component.name, component) {
                return Err(GraphError::DuplicateComponentName {
                    existing: Box::new(ComponentConflict::from_desired(existing)),
                });
            }
        }
        Ok(())
    }
}

impl GraphSnapshot {
    /// Look up the current graph-owned metadata by immutable UUID.
    pub fn component(&self, id: &ComponentId) -> Option<&NodeSnapshot> {
        self.nodes.iter().find(|node| node.descriptor.id() == id)
    }

    /// Look up a case-sensitive name in this coherent graph publication.
    /// Names are never interpreted as UUID references.
    pub fn component_by_name(&self, name: &ComponentName) -> Option<&NodeSnapshot> {
        self.nodes.iter().find(|node| &node.name == name)
    }

    pub(crate) fn conflict(
        &self,
        node: &NodeSnapshot,
        observed: &ObservedGraph,
    ) -> ComponentConflict {
        let id = node.descriptor.id();
        let mut existing = ComponentConflict::new(
            &node.descriptor,
            &node.name,
            node.role,
            self.specifications.get(id),
        );
        if let Some(plugin) = self.component_plugins.get(id) {
            existing.plugin = Some(plugin.clone());
        }
        if let Some(state) = observed.components.get(id) {
            existing.realization = Some(state.realization);
            existing.lifecycle = Some(state.lifecycle);
        }
        existing
    }
}

impl GraphControl {
    pub(crate) fn check_new_component(
        &self,
        id: &ComponentId,
        name: &ComponentName,
        created: bool,
    ) -> GraphResult<()> {
        let publication = self.registry_snapshot();
        if let Some(node) = publication.desired.component(id) {
            return Err(GraphError::DuplicateComponentId {
                existing: Box::new(publication.desired.conflict(node, &publication.observed)),
            });
        }
        if let Some(node) = publication.desired.component_by_name(name) {
            return Err(GraphError::DuplicateComponentName {
                existing: Box::new(publication.desired.conflict(node, &publication.observed)),
            });
        }
        if !self.allow_external_component_ids && !created {
            return Err(GraphError::ExternalComponentId {
                component: id.clone(),
            });
        }
        Ok(())
    }
}

impl ComputationGraph {
    pub(super) fn check_identity_admission(
        &self,
        component: &ComponentId,
        created: bool,
    ) -> GraphResult<()> {
        if !self.allow_external_component_ids && !created {
            return Err(GraphError::ExternalComponentId {
                component: component.clone(),
            });
        }
        Ok(())
    }

    pub(super) fn contextualize_conflict(&self, error: GraphError) -> GraphError {
        match error {
            GraphError::DuplicateComponentId { existing } => GraphError::DuplicateComponentId {
                existing: self
                    .snapshot
                    .component(&existing.id)
                    .map(|node| Box::new(self.snapshot.conflict(node, &self.observed())))
                    .unwrap_or(existing),
            },
            GraphError::DuplicateComponentName { existing } => GraphError::DuplicateComponentName {
                existing: self
                    .snapshot
                    .component_by_name(&existing.name)
                    .map(|node| Box::new(self.snapshot.conflict(node, &self.observed())))
                    .unwrap_or(existing),
            },
            other => other,
        }
    }

    pub(super) fn check_component_identity(
        &self,
        id: &ComponentId,
        name: &ComponentName,
    ) -> GraphResult<()> {
        if let Some(node) = self.snapshot.component(id) {
            return Err(GraphError::DuplicateComponentId {
                existing: Box::new(self.snapshot.conflict(node, &self.observed())),
            });
        }
        self.check_component_name(id, name)
    }

    pub(super) fn check_component_name(
        &self,
        id: &ComponentId,
        name: &ComponentName,
    ) -> GraphResult<()> {
        if let Some(node) = self
            .snapshot
            .component_by_name(name)
            .filter(|node| node.descriptor.id() != id)
        {
            return Err(GraphError::DuplicateComponentName {
                existing: Box::new(self.snapshot.conflict(node, &self.observed())),
            });
        }
        Ok(())
    }

    pub(super) fn rename_component(
        &mut self,
        id: &ComponentId,
        generation: ComponentGeneration,
        name: ComponentName,
    ) -> GraphResult<()> {
        let index = *self.ids.get(id).ok_or(GraphError::StaleGeneration)?;
        if self.components[index].generation != generation {
            return Err(GraphError::StaleGeneration);
        }
        if self.reserved_components.contains(id.as_str()) {
            return Err(topology(format!(
                "component ID {id} is reserved by the host"
            )));
        }
        self.check_component_name(id, &name)?;
        if self.nodes[index].name == name {
            return Ok(());
        }
        let revision = GraphRevision(
            self.snapshot
                .revision
                .0
                .checked_add(1)
                .ok_or_else(|| topology("graph revision exhausted"))?,
        );
        let mut nodes = self.snapshot.nodes.to_vec();
        let node = nodes
            .iter_mut()
            .find(|node| node.descriptor.id() == id)
            .ok_or(GraphError::StaleGeneration)?;
        node.name = name.clone();
        self.nodes[index].name = name;
        self.snapshot.nodes = nodes.into();
        self.snapshot.revision = revision;
        self.desired.send_replace(Arc::new(self.snapshot.clone()));
        controller::update(self, |state| {
            state.revision = revision;
            for id in state.components.keys().cloned().collect::<Vec<_>>() {
                if let Some(component) = state.components.get_mut(&id) {
                    component.revision = revision;
                }
            }
            for id in state.resources.keys().cloned().collect::<Vec<_>>() {
                if let Some(resource) = state.resources.get_mut(&id) {
                    resource.revision = revision;
                }
            }
        });
        Ok(())
    }
}
