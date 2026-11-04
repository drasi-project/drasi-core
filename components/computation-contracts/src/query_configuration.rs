// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use drasi_core_types::models::SourceMiddlewareConfig;
use serde::{Deserialize, Serialize};

use crate::{normalize_query_joins, QueryJoinConfig, SourceTransactionLimits};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ComputationQueryLanguage {
    #[default]
    Cypher,
    Gql,
}

/// One query input, including source interests and its ordered middleware pipeline.
///
/// A component name is the shorthand form. The expanded form accepts only
/// `component`, `port`, `nodes`, `relations` and `pipeline`. Input order remains
/// significant for the query's existing same-timestamp merge policy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryInputConfig {
    pub component: String,
    pub port: Option<String>,
    pub nodes: Vec<String>,
    pub relations: Vec<String>,
    pub pipeline: Vec<String>,
}

impl QueryInputConfig {
    pub fn new(component: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            ..Self::default()
        }
    }

    pub fn with_port(mut self, port: impl Into<String>) -> Self {
        self.port = Some(port.into());
        self
    }

    pub fn with_pipeline(mut self, pipeline: Vec<String>) -> Self {
        self.pipeline = pipeline;
        self
    }

    pub fn validate(&self) -> Result<()> {
        crate::validate_identifier("query input component", &self.component)?;
        if let Some(port) = &self.port {
            crate::validate_identifier("query input port", port)?;
        }
        for (kind, labels) in [("node", &self.nodes), ("relation", &self.relations)] {
            if labels.iter().any(|label| label.is_empty()) {
                anyhow::bail!("query input '{}' has an empty {kind} label", self.component);
            }
        }
        if self.pipeline.iter().any(|name| name.is_empty()) {
            anyhow::bail!(
                "query input '{}' has an empty pipeline name",
                self.component
            );
        }
        Ok(())
    }

    fn is_shorthand(&self) -> bool {
        self.port.is_none()
            && self.nodes.is_empty()
            && self.relations.is_empty()
            && self.pipeline.is_empty()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpandedQueryInput {
    component: String,
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
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.is_shorthand() {
            return serializer.serialize_str(&self.component);
        }
        ExpandedQueryInput {
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
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct InputVisitor;
        impl<'de> serde::de::Visitor<'de> for InputVisitor {
            type Value = QueryInputConfig;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a component name or an input object")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(QueryInputConfig::new(value))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                let expanded = ExpandedQueryInput::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(QueryInputConfig {
                    component: expanded.component,
                    port: expanded.port,
                    nodes: expanded.nodes,
                    relations: expanded.relations,
                    pipeline: expanded.pipeline,
                })
            }
        }
        deserializer.deserialize_any(InputVisitor)
    }
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
    fn execution_settings(&self) -> QueryExecutionSettings;
    fn subscription_settings(&self) -> Result<Vec<Self::Subscription>>;
}

pub fn query_source_component_id(
    query: &str,
    source: &str,
) -> std::result::Result<crate::ComponentId, crate::ContractError> {
    let encoded = |value: &str| {
        value
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    crate::ComponentId::try_new(format!("source/{}/{}", encoded(query), encoded(source)))
}

impl<T> QueryConfigurationSource for T
where
    T: std::ops::Deref,
    T::Target: QueryConfigurationSource,
{
    type Subscription = <T::Target as QueryConfigurationSource>::Subscription;

    fn execution_settings(&self) -> QueryExecutionSettings {
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

    pub fn from_legacy_config(config: &impl QueryConfigurationSource) -> Self {
        config.execution_settings()
    }

    pub fn legacy_subscriptions<C: QueryConfigurationSource>(
        config: &C,
    ) -> Result<Vec<C::Subscription>> {
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
