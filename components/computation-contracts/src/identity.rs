// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{data::validate_identifier, ComponentDescriptor};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct PluginIdentity {
    pub id: Arc<str>,
    pub version: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ImplementationIdentity {
    pub name: Arc<str>,
    pub version: Arc<str>,
    pub plugin: Option<PluginIdentity>,
}

impl ImplementationIdentity {
    pub fn try_new(name: impl Into<Arc<str>>, version: impl Into<Arc<str>>) -> crate::Result<Self> {
        let value = Self {
            name: name.into(),
            version: version.into(),
            plugin: None,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> crate::Result<()> {
        validate_identifier("implementation", &self.name)?;
        validate_identifier("implementation version", &self.version)?;
        if let Some(plugin) = &self.plugin {
            validate_identifier("plugin", &plugin.id)?;
            validate_identifier("plugin version", &plugin.version)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComponentRole {
    Source,
    Transformer,
    Query,
    Sink,
    Service,
}

/// A component's semantic category, separate from its port/lifecycle execution
/// role. Hosts can annotate wrappers through [`ComponentDescriptor::with_semantic_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComponentSemanticKind {
    Source,
    Transformer,
    Query,
    Sink,
    Reaction,
    Service,
}

impl From<ComponentRole> for ComponentSemanticKind {
    fn from(role: ComponentRole) -> Self {
        match role {
            ComponentRole::Source => Self::Source,
            ComponentRole::Transformer => Self::Transformer,
            ComponentRole::Query => Self::Query,
            ComponentRole::Sink => Self::Sink,
            ComponentRole::Service => Self::Service,
        }
    }
}

impl ComponentSemanticKind {
    pub fn for_component(role: ComponentRole, descriptor: &ComponentDescriptor) -> Self {
        descriptor.semantic_kind().unwrap_or_else(|| role.into())
    }
}
