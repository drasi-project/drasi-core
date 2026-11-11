// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use serde::{Deserialize, Serialize};

use super::{ComponentId, ComponentName, ComponentReference};

/// Ordinary authoring input. Strings select names; `{"id":"UUID"}` selects an
/// identity. Execution settings contain only resolved UUID strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryInputConfig {
    pub component: ComponentReference,
    pub port: Option<String>,
    pub nodes: Vec<String>,
    pub relations: Vec<String>,
    pub pipeline: Vec<String>,
}

impl QueryInputConfig {
    pub fn new(component: ComponentReference) -> Self {
        Self {
            component,
            port: None,
            nodes: Vec::new(),
            relations: Vec::new(),
            pipeline: Vec::new(),
        }
    }

    pub fn by_name(name: impl Into<std::sync::Arc<str>>) -> anyhow::Result<Self> {
        Ok(Self::new(ComponentReference::Name(ComponentName::try_new(
            name,
        )?)))
    }

    pub fn by_id(id: ComponentId) -> Self {
        Self::new(ComponentReference::by_id(id))
    }

    pub fn with_port(mut self, port: impl Into<String>) -> Self {
        self.port = Some(port.into());
        self
    }

    pub fn with_pipeline(mut self, pipeline: Vec<String>) -> Self {
        self.pipeline = pipeline;
        self
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.execution_input().validate()
    }

    pub(crate) fn id(&self) -> anyhow::Result<&ComponentId> {
        self.component
            .as_id()
            .ok_or_else(|| anyhow::anyhow!("unresolved {}", self.component))
    }

    pub(crate) fn id_str(&self) -> anyhow::Result<&str> {
        Ok(self.id()?.as_str())
    }

    pub(crate) fn execution_input(&self) -> drasi_computation_contracts::QueryInputConfig {
        drasi_computation_contracts::QueryInputConfig {
            component: match &self.component {
                ComponentReference::Name(name) => name.to_string(),
                ComponentReference::Id { id } => id.to_string(),
            },
            port: self.port.clone(),
            nodes: self.nodes.clone(),
            relations: self.relations.clone(),
            pipeline: self.pipeline.clone(),
        }
    }

    /// Cross into the evaluator/plugin settings contract only after name binding.
    pub fn to_execution_input(
        &self,
    ) -> anyhow::Result<drasi_computation_contracts::QueryInputConfig> {
        self.id()?;
        Ok(self.execution_input())
    }

    /// Import an execution input, whose component field must be a UUID.
    pub fn from_execution(
        input: drasi_computation_contracts::QueryInputConfig,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            component: ComponentReference::by_id(ComponentId::try_new(input.component)?),
            port: input.port,
            nodes: input.nodes,
            relations: input.relations,
            pipeline: input.pipeline,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpandedInput {
    component: ComponentReference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    nodes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    relations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pipeline: Vec<String>,
}

impl Serialize for QueryInputConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        if self.port.is_none()
            && self.nodes.is_empty()
            && self.relations.is_empty()
            && self.pipeline.is_empty()
        {
            return self.component.serialize(serializer);
        }
        ExpandedInput {
            component: self.component.clone(),
            port: self.port.clone(),
            nodes: self.nodes.clone(),
            relations: self.relations.clone(),
            pipeline: self.pipeline.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for QueryInputConfig {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Input {
            Reference(ComponentReference),
            Expanded(ExpandedInput),
        }
        Ok(match Input::deserialize(deserializer)? {
            Input::Reference(reference) => Self::new(reference),
            Input::Expanded(input) => Self {
                component: input.component,
                port: input.port,
                nodes: input.nodes,
                relations: input.relations,
                pipeline: input.pipeline,
            },
        })
    }
}
