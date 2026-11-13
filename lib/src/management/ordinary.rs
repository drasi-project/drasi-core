// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    computation::v1::{
        ComponentConflict, ComponentId, ComponentName, ComponentSemanticKind, GraphError,
    },
    config::{AssignedQueryConfig, ComponentReference, QueryConfig, QuerySettings},
    Reaction, Source,
};

pub(super) fn identities(desired: &super::DesiredInstance) -> Vec<ComponentConflict> {
    desired
        .topology
        .components
        .iter()
        .map(ComponentConflict::from_desired)
        .chain(
            desired
                .ordinary
                .iter()
                .map(OrdinaryComponentRecord::conflict),
        )
        .collect()
}

pub(super) fn validate_transition(
    previous: &super::DesiredInstance,
    desired: &super::DesiredInstance,
) -> anyhow::Result<()> {
    let remaining: std::collections::BTreeSet<_> = identities(desired)
        .into_iter()
        .map(|component| component.id)
        .collect();
    let removed: std::collections::BTreeSet<_> = identities(previous)
        .into_iter()
        .map(|component| component.id)
        .filter(|id| !remaining.contains(id))
        .collect();
    for consumer in &desired.ordinary {
        if let Some(old) = previous.ordinary.iter().find(|old| old.id == consumer.id) {
            if let (
                OrdinaryComponent::ProcessLocal {
                    kind: old_kind,
                    binding: old_binding,
                    inputs: old_inputs,
                },
                OrdinaryComponent::ProcessLocal {
                    kind,
                    binding,
                    inputs,
                },
            ) = (&old.component, &consumer.component)
            {
                if old_binding == binding {
                    let identities = identities(desired);
                    anyhow::ensure!(old_kind == kind && old_inputs.len() == inputs.len()
                        && old_inputs.iter().zip(inputs).all(|(old, new)| old == new ||
                            old.as_name().is_some_and(|name| identities.iter()
                                .any(|target| &target.name == name && new.as_id() == Some(&target.id)))),
                        "changing a process-local component's bound inputs requires a new constructor");
                }
            }
        }
        for reference in consumer.references()? {
            if let Some(id) = reference.as_id().filter(|id| removed.contains(*id)) {
                anyhow::bail!("component {id} is depended on by {}; remove its reference before removing the component", consumer.id);
            }
        }
    }
    Ok(())
}

pub(super) fn normalize(desired: &mut super::DesiredInstance) -> anyhow::Result<()> {
    let mut ids = BTreeMap::new();
    let mut names = BTreeMap::new();
    for identity in identities(desired) {
        if let Some(existing) = ids.insert(identity.id.clone(), identity.clone()) {
            return Err(GraphError::DuplicateComponentId {
                existing: Box::new(existing),
            }
            .into());
        }
        if let Some(existing) = names.insert(identity.name.clone(), identity.clone()) {
            return Err(GraphError::DuplicateComponentName {
                existing: Box::new(existing),
            }
            .into());
        }
    }
    let bindings = names
        .into_iter()
        .map(|(name, identity)| (name, identity.id))
        .collect();
    for record in &mut desired.ordinary {
        record.validate()?;
        record.bind_names(&bindings)?;
        record.validate()?;
        if record.kind() == ComponentSemanticKind::Reaction {
            for input in record.references()? {
                if let Some(target) = input.as_id().and_then(|id| ids.get(id)) {
                    anyhow::ensure!(
                        target.kind == ComponentSemanticKind::Query,
                        "reaction {} input {} is not a query",
                        record.id,
                        target.id
                    );
                }
            }
        }
    }
    desired
        .ordinary
        .sort_by(|left, right| left.id.cmp(&right.id));
    Ok(())
}

/// Host-neutral, reconstructible ordinary component settings. Source/reaction
/// configuration belongs to the Host resolver; Core owns query interpretation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum OrdinaryComponent {
    Source {
        kind: String,
        configuration: serde_json::Value,
    },
    /// Query settings without `id`, `name` or `auto_start`; those belong to the
    /// enclosing record. Use `OrdinaryComponentDefinition::query` for Rust authoring.
    Query { configuration: QuerySettings },
    Reaction {
        kind: String,
        configuration: serde_json::Value,
        inputs: Vec<ComponentReference>,
    },
    /// An explicitly non-reconstructible, process-local declaration. The binding
    /// names an actual graph-owned instance supplied by Core, never a restore permit.
    ProcessLocal {
        kind: ProcessLocalComponentKind,
        binding: crate::computation::v1::ResourceId,
        inputs: Vec<ComponentReference>,
    },
}

/// Ordinary roles supported by process-local constructors and objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProcessLocalComponentKind {
    Source,
    Reaction,
}

/// Ordinary authoring input. Omitted IDs are assigned by the configuration owner.
/// The assigned specialization is the actual saved record, not a restore permit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrdinaryComponentDefinition {
    pub id: Option<ComponentId>,
    pub name: ComponentName,
    pub auto_start: bool,
    pub component: OrdinaryComponent,
}

/// Fully assigned definition persisted with the existing desired-state receipt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrdinaryComponentRecord {
    pub id: ComponentId,
    pub name: ComponentName,
    pub auto_start: bool,
    pub component: OrdinaryComponent,
}

impl OrdinaryComponentDefinition {
    pub fn query(configuration: QueryConfig) -> anyhow::Result<Self> {
        configuration.validate_declarations()?;
        let id = configuration.id.clone();
        let name = configuration.name.clone();
        let auto_start = configuration.auto_start;
        let configuration = configuration.into_settings();
        Ok(Self {
            id,
            name,
            auto_start,
            component: OrdinaryComponent::Query { configuration },
        })
    }

    /// Construct assigned data. Admission authority is still checked separately.
    pub fn with_assigned_id(self, id: ComponentId) -> OrdinaryComponentRecord {
        OrdinaryComponentRecord {
            id,
            name: self.name,
            auto_start: self.auto_start,
            component: self.component,
        }
    }
}

impl OrdinaryComponentRecord {
    pub(crate) fn kind(&self) -> ComponentSemanticKind {
        match self.component {
            OrdinaryComponent::Source { .. } => ComponentSemanticKind::Source,
            OrdinaryComponent::Query { .. } => ComponentSemanticKind::Query,
            OrdinaryComponent::Reaction { .. } => ComponentSemanticKind::Reaction,
            OrdinaryComponent::ProcessLocal {
                kind: ProcessLocalComponentKind::Source,
                ..
            } => ComponentSemanticKind::Source,
            OrdinaryComponent::ProcessLocal {
                kind: ProcessLocalComponentKind::Reaction,
                ..
            } => ComponentSemanticKind::Reaction,
        }
    }

    pub(crate) fn conflict(&self) -> ComponentConflict {
        ComponentConflict {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind(),
            implementation: None,
            plugin: None,
            realization: None,
            lifecycle: None,
        }
    }

    /// Decode assigned query settings for a Host adapter. This is data conversion,
    /// not permission to adopt the recorded UUID through an ordinary creation API.
    pub fn query_configuration(&self) -> anyhow::Result<AssignedQueryConfig> {
        let OrdinaryComponent::Query { configuration } = &self.component else {
            anyhow::bail!("component {} is not a query", self.id);
        };
        Ok(configuration
            .clone()
            .with_identity(self.id.clone(), self.name.clone(), self.auto_start))
    }

    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        match &self.component {
            OrdinaryComponent::Source { kind, .. } | OrdinaryComponent::Reaction { kind, .. } => {
                drasi_computation_contracts::validate_identifier("ordinary component kind", kind)?
            }
            OrdinaryComponent::Query { .. } => {
                self.query_configuration()?.validate_declarations()?
            }
            OrdinaryComponent::ProcessLocal { kind, inputs, .. } => {
                anyhow::ensure!(
                    *kind != ProcessLocalComponentKind::Source || inputs.is_empty(),
                    "a process-local source cannot declare reaction inputs"
                );
            }
        }
        let mut references = std::collections::BTreeSet::new();
        for reference in self.references()? {
            anyhow::ensure!(
                reference.as_id() != Some(&self.id),
                "component {} cannot consume itself",
                self.id
            );
            anyhow::ensure!(
                references.insert(serde_json::to_string(&reference)?),
                "component {} has duplicate inputs",
                self.id
            );
        }
        Ok(())
    }

    pub(crate) fn references(&self) -> anyhow::Result<Vec<ComponentReference>> {
        Ok(match &self.component {
            OrdinaryComponent::Source { .. } => Vec::new(),
            OrdinaryComponent::Reaction { inputs, .. } => inputs.clone(),
            OrdinaryComponent::ProcessLocal { inputs, .. } => inputs.clone(),
            OrdinaryComponent::Query { .. } => self
                .query_configuration()?
                .inputs
                .into_iter()
                .map(|input| input.component)
                .collect(),
        })
    }

    pub(crate) fn bind_names(
        &mut self,
        names: &BTreeMap<ComponentName, ComponentId>,
    ) -> anyhow::Result<()> {
        let bind = |reference: &mut ComponentReference| {
            if let Some(id) = reference.as_name().and_then(|name| names.get(name)) {
                *reference = ComponentReference::by_id(id.clone());
            }
        };
        match &mut self.component {
            OrdinaryComponent::Source { .. } => {}
            OrdinaryComponent::Reaction { inputs, .. } => inputs.iter_mut().for_each(bind),
            OrdinaryComponent::ProcessLocal { inputs, .. } => inputs.iter_mut().for_each(bind),
            OrdinaryComponent::Query { configuration } => {
                for input in &mut configuration.inputs {
                    bind(&mut input.component);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn same_construction(&self, other: &Self) -> bool {
        self.id == other.id
            && match (&self.component, &other.component) {
                (
                    OrdinaryComponent::ProcessLocal { kind, binding, .. },
                    OrdinaryComponent::ProcessLocal {
                        kind: previous_kind,
                        binding: previous_binding,
                        ..
                    },
                ) => kind == previous_kind && binding == previous_binding,
                _ => self.component == other.component,
            }
    }

    /// Validate bindings already recorded on the target. Looking up the original
    /// names again would let later name reuse retarget an unfinished update.
    pub(crate) fn matches_bound_definition(&self, target: &Self) -> anyhow::Result<bool> {
        let declared = self.references()?;
        let bound = target.references()?;
        if declared.len() != bound.len() {
            return Ok(false);
        }
        let mut bindings = BTreeMap::new();
        for (declared, bound) in declared.iter().zip(&bound) {
            if declared == bound {
                continue;
            }
            let (Some(name), Some(id)) = (declared.as_name(), bound.as_id()) else {
                return Ok(false);
            };
            if bindings.insert(name.clone(), id.clone()).is_some() {
                return Ok(false);
            }
        }
        let mut resolved = self.clone();
        resolved.bind_names(&bindings)?;
        Ok(resolved.same_construction(target))
    }
}

/// The configured Host resolves stored source/reaction recipes. Core supplies the
/// recorded UUID and, for reactions, already-bound query UUIDs. No caller-supplied
/// restore flag exists. Futures must release partial acquisitions on cancellation.
#[async_trait]
pub trait OrdinaryComponentResolver: Send + Sync {
    /// Pure assessment delegated to the registered component author's factory.
    /// The default rejects live updates; creation support is not update support.
    fn plan_update(
        &self,
        _instance_id: &str,
        _current: &OrdinaryComponentRecord,
        _proposed: &OrdinaryComponentRecord,
    ) -> anyhow::Result<crate::computation::v1::ComponentUpdatePlan> {
        anyhow::bail!("the registered ordinary component factory does not support managed updates")
    }

    async fn create_source(
        &self,
        instance_id: &str,
        definition: &OrdinaryComponentRecord,
        update: Option<&crate::computation::v1::ComponentUpdateDirective>,
    ) -> anyhow::Result<(Box<dyn Source>, HashMap<String, String>)>;

    async fn create_reaction(
        &self,
        instance_id: &str,
        definition: &OrdinaryComponentRecord,
        inputs: &[ComponentId],
        update: Option<&crate::computation::v1::ComponentUpdateDirective>,
    ) -> anyhow::Result<(Box<dyn Reaction>, HashMap<String, String>)>;
}

/// Accepted execution context for an ordinary component's retained-object update.
/// Only the configuration owner issues operation IDs and invokes this operation.
pub struct OrdinaryComponentUpdateContext {
    /// Stable operation ID across interruption and process recovery.
    pub operation_id: std::sync::Arc<str>,
    /// Instance scope of the registered component.
    pub instance_id: std::sync::Arc<str>,
    /// Explicit, validated update strategy from its registered factory.
    pub plan: crate::computation::v1::ComponentUpdatePlan,
    /// The recorded creation definition before the update.
    pub current: std::sync::Arc<OrdinaryComponentRecord>,
    /// The accepted target definition, retaining the logical UUID and name.
    pub proposed: std::sync::Arc<OrdinaryComponentRecord>,
}

pub(super) struct NoOrdinaryComponents;

#[async_trait]
impl OrdinaryComponentResolver for NoOrdinaryComponents {
    async fn create_source(
        &self,
        _: &str,
        _: &OrdinaryComponentRecord,
        _: Option<&crate::computation::v1::ComponentUpdateDirective>,
    ) -> anyhow::Result<(Box<dyn Source>, HashMap<String, String>)> {
        anyhow::bail!("no ordinary component resolver is configured")
    }
    async fn create_reaction(
        &self,
        _: &str,
        _: &OrdinaryComponentRecord,
        _: &[ComponentId],
        _: Option<&crate::computation::v1::ComponentUpdateDirective>,
    ) -> anyhow::Result<(Box<dyn Reaction>, HashMap<String, String>)> {
        anyhow::bail!("no ordinary component resolver is configured")
    }
}
