// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_core_types::models::SourceMiddlewareConfig;
use serde::{Deserialize, Serialize};

use crate::{
    normalize_query_joins, ComponentId, ComponentIdentityError, PortId, QueryJoinConfig,
    SourceTransactionLimits,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ComputationQueryLanguage {
    #[default]
    Cypher,
    Gql,
}

/// A resolved execution input. Authoring names must be bound before construction.
/// Input order remains significant for the same-timestamp merge policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryInputConfig {
    pub component: ComponentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<PortId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pipeline: Vec<String>,
}

impl QueryInputConfig {
    pub fn new(component: ComponentId) -> Self {
        Self {
            component,
            port: None,
            nodes: Vec::new(),
            relations: Vec::new(),
            pipeline: Vec::new(),
        }
    }

    pub fn with_port(mut self, port: PortId) -> Self {
        self.port = Some(port);
        self
    }

    pub fn with_pipeline(mut self, pipeline: Vec<String>) -> Self {
        self.pipeline = pipeline;
        self
    }

    pub fn validate(&self) -> Result<()> {
        validate_query_input_annotations(&self.nodes, &self.relations, &self.pipeline)
            .with_context(|| format!("query input '{}'", self.component))
    }
}

/// Validate data labels and middleware names shared by authoring and execution.
pub fn validate_query_input_annotations(
    nodes: &[String],
    relations: &[String],
    pipeline: &[String],
) -> Result<()> {
    for (kind, labels) in [("node", nodes), ("relation", relations)] {
        anyhow::ensure!(!labels.iter().any(String::is_empty), "empty {kind} label");
    }
    anyhow::ensure!(
        !pipeline.iter().any(String::is_empty),
        "empty pipeline name"
    );
    Ok(())
}

/// The graph node label of result rows; the output schema remains graph-change.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryResultConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl QueryResultConfig {
    pub fn is_default(&self) -> bool {
        self.label.is_none()
    }

    pub fn validate(&self) -> Result<()> {
        if self.label.as_deref().is_some_and(str::is_empty) {
            anyhow::bail!("query result label must not be empty");
        }
        Ok(())
    }
}

/// Query execution settings without graph, provider or evaluator ownership.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryExecutionSettings {
    #[serde(default)]
    pub joins: Vec<QueryJoinConfig>,
    #[serde(default)]
    pub middleware: Vec<SourceMiddlewareConfig>,
    #[serde(default)]
    pub inputs: Vec<QueryInputConfig>,
    #[serde(default, skip_serializing_if = "QueryResultConfig::is_default")]
    pub result: QueryResultConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_transactions: Option<SourceTransactionLimits>,
}

/// Host-side authoring adapters retain their own source-subscription types.
pub trait QueryConfigurationSource {
    type Subscription;
    fn execution_settings(&self) -> Result<QueryExecutionSettings>;
    fn subscription_settings(&self) -> Result<Vec<Self::Subscription>>;
}

/// Identify the query-owned reader for an actual source UUID.
/// Editable component names are not valid inputs or part of the helper identity.
pub fn query_source_component_id(
    query: &ComponentId,
    source: &ComponentId,
) -> std::result::Result<ComponentId, ComponentIdentityError> {
    ComponentId::derive_owned(query, &format!("query-input/{source}"))
}

impl<T> QueryConfigurationSource for T
where
    T: std::ops::Deref,
    T::Target: QueryConfigurationSource,
{
    type Subscription = <T::Target as QueryConfigurationSource>::Subscription;

    fn execution_settings(&self) -> Result<QueryExecutionSettings> {
        QueryConfigurationSource::execution_settings(self.deref())
    }

    fn subscription_settings(&self) -> Result<Vec<Self::Subscription>> {
        QueryConfigurationSource::subscription_settings(self.deref())
    }
}

impl QueryExecutionSettings {
    pub fn fingerprint_settings(&self) -> Self {
        let mut settings = self.clone();
        for input in &mut settings.inputs {
            input.nodes.sort();
            input.nodes.dedup();
            input.relations.sort();
            input.relations.dedup();
        }

        normalize_query_joins(&mut settings.joins);
        settings
    }

    pub fn from_configuration(config: &impl QueryConfigurationSource) -> Result<Self> {
        config.execution_settings()
    }

    pub fn subscriptions<C: QueryConfigurationSource>(config: &C) -> Result<Vec<C::Subscription>> {
        config.subscription_settings()
    }

    pub fn is_empty(&self) -> bool {
        self.joins.is_empty()
            && self.middleware.is_empty()
            && self.inputs.is_empty()
            && self.result.is_default()
            && self.source_transactions.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUERY: &str = "00112233-4455-4677-8899-aabbccddeeff";
    const SOURCE: &str = "11223344-5566-4788-99aa-bbccddeeff00";

    #[test]
    fn component_source_helper_depends_on_actual_uuid_owners() {
        let reader = query_source_component_id(
            &drasi_core_types::ComponentId::try_new("00112233-4455-4677-8899-aabbccddeeff")
                .expect("fixture UUID"),
            &drasi_core_types::ComponentId::try_new("11223344-5566-4788-99aa-bbccddeeff00")
                .expect("fixture UUID"),
        )
        .unwrap();
        assert_eq!(
            reader,
            query_source_component_id(
                &drasi_core_types::ComponentId::try_new(QUERY.to_ascii_uppercase())
                    .expect("uppercase fixture UUID"),
                &drasi_core_types::ComponentId::try_new("11223344-5566-4788-99aa-bbccddeeff00")
                    .expect("fixture UUID")
            )
            .unwrap()
        );
        assert_ne!(reader.as_str(), QUERY);
        assert_ne!(reader.as_str(), SOURCE);
        assert_ne!(
            reader,
            query_source_component_id(
                &drasi_core_types::ComponentId::try_new("11223344-5566-4788-99aa-bbccddeeff00")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00112233-4455-4677-8899-aabbccddeeff")
                    .expect("fixture UUID")
            )
            .unwrap()
        );
        assert_ne!(
            reader,
            query_source_component_id(
                &drasi_core_types::ComponentId::try_new("00112233-4455-4677-8899-aabbccddeeff")
                    .expect("fixture UUID"),
                &crate::ComponentId::generate()
            )
            .unwrap()
        );
    }

    #[test]
    fn component_source_helper_requires_uuid_inputs_before_derivation() {
        assert!(ComponentId::try_new("query").is_err());
        assert!(ComponentId::try_new("source").is_err());
    }
}
