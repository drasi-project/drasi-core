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

/// A component's execution role: its port shape and lifecycle contract.
///
/// Continuous queries execute as [`ComponentRole::Transformer`] and declare
/// [`ComponentSemanticKind::Query`] on their descriptor. A source that
/// consumes upstream changes or a reaction that emits results uses that
/// same input/output execution contract with its own semantic kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComponentRole {
    Source,
    Transformer,
    Sink,
    Service,
    /// Serves bootstrap snapshots through bootstrap output ports only. It has
    /// no data ports and answers addressed bootstrap requests per session.
    BootstrapProvider,
}

impl ComponentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Transformer => "transformer",
            Self::Sink => "sink",
            Self::Service => "service",
            Self::BootstrapProvider => "bootstrap_provider",
        }
    }
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
    BootstrapProvider,
}

impl From<ComponentRole> for ComponentSemanticKind {
    fn from(role: ComponentRole) -> Self {
        match role {
            ComponentRole::Source => Self::Source,
            ComponentRole::Transformer => Self::Transformer,
            ComponentRole::Sink => Self::Sink,
            ComponentRole::Service => Self::Service,
            ComponentRole::BootstrapProvider => Self::BootstrapProvider,
        }
    }
}

impl ComponentSemanticKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Transformer => "transformer",
            Self::Query => "query",
            Self::Sink => "sink",
            Self::Reaction => "reaction",
            Self::Service => "service",
            Self::BootstrapProvider => "bootstrap_provider",
        }
    }

    pub fn for_component(role: ComponentRole, descriptor: &ComponentDescriptor) -> Self {
        descriptor.semantic_kind().unwrap_or_else(|| role.into())
    }
}
