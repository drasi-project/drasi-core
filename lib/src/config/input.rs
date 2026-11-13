// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_contracts::PortId;
use serde::{Deserialize, Serialize};

use super::{ComponentId, ComponentName, ComponentReference};

/// Ordinary authoring input. Strings select names; `{"id":"UUID"}` selects an
/// identity. Execution settings contain only resolved component identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryInputConfig {
    pub component: ComponentReference,
    pub port: Option<PortId>,
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

    pub fn by_name(name: ComponentName) -> Self {
        Self::new(ComponentReference::by_name(name))
    }

    pub fn by_id(id: ComponentId) -> Self {
        Self::new(ComponentReference::by_id(id))
    }

    pub fn with_port(mut self, port: PortId) -> Self {
        self.port = Some(port);
        self
    }

    pub fn with_pipeline(mut self, pipeline: Vec<String>) -> Self {
        self.pipeline = pipeline;
        self
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        drasi_computation_contracts::validate_query_input_annotations(
            &self.nodes,
            &self.relations,
            &self.pipeline,
        )
    }

    pub(crate) fn id(&self) -> anyhow::Result<&ComponentId> {
        self.component
            .as_id()
            .ok_or_else(|| anyhow::anyhow!("unresolved {}", self.component))
    }

    /// Cross into the evaluator/plugin settings contract only after name binding.
    pub fn to_execution_input(
        &self,
    ) -> anyhow::Result<drasi_computation_contracts::QueryInputConfig> {
        Ok(drasi_computation_contracts::QueryInputConfig {
            component: self.id()?.clone(),
            port: self.port.clone(),
            nodes: self.nodes.clone(),
            relations: self.relations.clone(),
            pipeline: self.pipeline.clone(),
        })
    }

    /// Import an execution input, whose component field must be a UUID.
    pub fn from_execution(input: drasi_computation_contracts::QueryInputConfig) -> Self {
        Self {
            component: ComponentReference::by_id(input.component),
            port: input.port,
            nodes: input.nodes,
            relations: input.relations,
            pipeline: input.pipeline,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpandedInput {
    component: ComponentReference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<PortId>,
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
