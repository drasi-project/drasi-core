// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Durable author-assessed update intent, separate from ordinary authoring input.

use serde::{Deserialize, Serialize};

use crate::computation::v1::{
    ComponentId, ComponentSpecification, ComponentUpdateDirective, ComponentUpdatePlan,
};

use super::OrdinaryComponentRecord;

/// Authoring description retained to resolve an interrupted accepted update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "definition",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum ComponentUpdateDefinition {
    /// A native registered factory recipe, without resolved secrets or runtime handles.
    Native(ComponentSpecification),
    /// An ordinary registered recipe, without generated runtime resources.
    Ordinary(OrdinaryComponentRecord),
}

/// The configuration owner's assessment accepted atomically with the target definition.
///
/// These records are execution data, not caller-issued update or restoration permits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedComponentUpdate {
    /// Immutable logical component identity.
    pub component_id: ComponentId,
    /// Stable execution identity across retries and process recovery.
    pub operation_id: String,
    /// Exact author-provided strategy accepted by the owner.
    pub plan: ComponentUpdatePlan,
    /// Previously accepted recipe.
    pub current: ComponentUpdateDefinition,
    /// Newly accepted recipe.
    pub proposed: ComponentUpdateDefinition,
}

impl AcceptedComponentUpdate {
    pub(crate) fn validate(&self, desired: &super::DesiredInstance) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.operation_id.is_empty() && self.operation_id.len() <= 1024,
            "accepted update has an invalid operation identity"
        );
        anyhow::ensure!(
            self.plan.data == crate::computation::v1::ComponentUpdateData::Preserve
                || self.plan.mode == crate::computation::v1::ComponentUpdateMode::Recreate,
            "stored data reconstruction requires Recreate"
        );
        match (&self.current, &self.proposed) {
            (
                ComponentUpdateDefinition::Native(current),
                ComponentUpdateDefinition::Native(proposed),
            ) => {
                anyhow::ensure!(
                    current.descriptor.id() == &self.component_id
                        && proposed.descriptor.id() == &self.component_id
                        && current.implementation == proposed.implementation,
                    "accepted update changed immutable identity or implementation"
                );
                if let Some(target) = desired
                    .topology
                    .components
                    .iter()
                    .find(|target| target.descriptor.id() == &self.component_id)
                {
                    anyhow::ensure!(
                        matches!(&target.construction,
                        crate::computation::v1::ComponentConstruction::Factory(spec) if spec == proposed),
                        "accepted native update does not match the stored target"
                    );
                }
            }
            (
                ComponentUpdateDefinition::Ordinary(current),
                ComponentUpdateDefinition::Ordinary(proposed),
            ) => {
                anyhow::ensure!(
                    current.id == self.component_id
                        && proposed.id == self.component_id
                        && current.kind() == proposed.kind()
                        && !matches!(
                            current.component,
                            super::OrdinaryComponent::ProcessLocal { .. }
                        )
                        && !matches!(
                            proposed.component,
                            super::OrdinaryComponent::ProcessLocal { .. }
                        ),
                    "accepted ordinary update has no registered immutable identity"
                );
                if let Some(target) = desired
                    .ordinary
                    .iter()
                    .find(|target| target.id == self.component_id)
                {
                    anyhow::ensure!(
                        proposed.matches_bound_definition(target)?,
                        "accepted ordinary update does not match the stored target"
                    );
                }
            }
            _ => anyhow::bail!("accepted update cannot change its construction owner"),
        }
        Ok(())
    }

    pub(crate) fn directive(&self) -> ComponentUpdateDirective {
        ComponentUpdateDirective {
            operation_id: self.operation_id.clone().into(),
            plan: self.plan,
        }
    }

    pub(crate) fn completion(&self) -> ComponentUpdateCompletion {
        ComponentUpdateCompletion {
            component_id: self.component_id.clone(),
            operation_id: self.operation_id.clone(),
        }
    }
}

/// Exact execution acknowledged after its hook completes and before processing resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentUpdateCompletion {
    /// Component whose accepted operation completed.
    pub component_id: ComponentId,
    /// The original accepted operation, never a new attempt identity.
    pub operation_id: String,
}
