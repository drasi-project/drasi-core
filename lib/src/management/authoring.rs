// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use serde::{Deserialize, Serialize};

use super::{ComponentDefinition, DesiredInstance, OrdinaryComponentDefinition};

/// A new definition, not an update or a saved restoration record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "definition",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum NewComponentDefinition {
    /// A native component implemented by a registered factory.
    Native(ComponentDefinition),
    /// An ordinary source, query or reaction definition.
    Ordinary(OrdinaryComponentDefinition),
}

/// Authoring changes accepted atomically by the instance configuration owner.
///
/// `desired` addresses existing assigned members and may remove them by omission.
/// `additions` always creates new members; a matching name never means update.
/// Native programmatic topology batches use the same path through the scoped
/// allocator API, so opaque business configuration is never searched for IDs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationRequest {
    /// Optional replacement for the existing saved definition. `None` retains it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired: Option<DesiredInstance>,
    /// New members whose IDs are optional and governed by the instance policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additions: Vec<NewComponentDefinition>,
}

impl ConfigurationRequest {
    /// Add one or more definitions without replacing existing members.
    pub fn adding(additions: impl IntoIterator<Item = NewComponentDefinition>) -> Self {
        Self {
            desired: None,
            additions: additions.into_iter().collect(),
        }
    }
}
