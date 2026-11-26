// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod indexed;
mod logic;
mod observations;
mod update;
mod view;

use std::{num::NonZeroUsize, sync::Arc};

use anyhow::Context;
use drasi_computation_contracts::ComputationQueryLanguage as QueryLanguage;
use drasi_computation_contracts::{
    ComponentRole, ComponentSemanticKind, ComponentUpdateMode, GraphChangeCodec,
    ImplementationIdentity, PipeRequirements, PortDescriptor, PortDirection, PortId,
    QueryDescription, QueryExecutionSettings, QueryRecoveryPolicy, StreamId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, transformer, Capabilities, Component, ConfigDetails, ConfigSchema,
    ConfigType, ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata,
    NativeProcessing, PluginDefinition, ServiceBindings, UpdateRequest,
};
use drasi_core::{middleware::MiddlewareTypeRegistry, query::QueryEvaluator};
use drasi_query_common::{query_builder, ContinuousQueryDefinition, QueryExecutionSettingsExt};

const INDEXED_IMPLEMENTATION: &str = "drasi.query/indexed";

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    plugin_with_middleware_registry(default_middleware_registry())
}

pub fn default_middleware_registry() -> Arc<MiddlewareTypeRegistry> {
    let mut registry = MiddlewareTypeRegistry::new();
    for factory in drasi_middleware::source_middleware_factories() {
        registry.register(factory);
    }
    Arc::new(registry)
}

pub fn plugin_with_middleware_registry(
    middleware: Arc<MiddlewareTypeRegistry>,
) -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(IndexedQueryFactory::new(middleware))],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

/// The `middleware` field declares the registry's kinds, so the graph rejects a
/// definition naming an unregistered kind when it binds the query.
fn query_configuration_schema(
    fields: impl IntoIterator<Item = (&'static str, ConfigType, bool)>,
    middleware: &MiddlewareTypeRegistry,
) -> ConfigSchema {
    let mut schema = ConfigSchema::from_fields(fields);
    schema
        .constrain(
            "inputs",
            drasi_computation_contracts::ConfigurationConstraint::query_inputs(),
        )
        .expect("query schema declares inputs");
    schema
        .constrain(
            "middleware",
            drasi_computation_contracts::ConfigurationConstraint::middleware_declarations(
                &middleware.kinds(),
            )
            .expect("middleware kinds are valid schema values"),
        )
        .expect("query schema declares middleware");
    schema
}

/// `sourceTransactions`: the camelCase form of the shared
/// [`drasi_computation_contracts::SourceTransactionLimits`].
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceTransactionSettings {
    max_changes: NonZeroUsize,
    max_bytes: NonZeroUsize,
    max_duration_ms: std::num::NonZeroU64,
}

impl From<SourceTransactionSettings> for drasi_computation_contracts::SourceTransactionLimits {
    fn from(settings: SourceTransactionSettings) -> Self {
        Self {
            max_changes: settings.max_changes,
            max_bytes: settings.max_bytes,
            max_duration_ms: settings.max_duration_ms,
        }
    }
}

pub struct IndexedQueryFactory {
    middleware: Arc<MiddlewareTypeRegistry>,
}

impl IndexedQueryFactory {
    pub fn new(middleware: Arc<MiddlewareTypeRegistry>) -> Self {
        Self { middleware }
    }
}

impl Default for IndexedQueryFactory {
    fn default() -> Self {
        Self::new(default_middleware_registry())
    }
}

/// Descriptions of every indexed-query configuration field.
fn query_configuration_details() -> ConfigDetails {
    let describe =
        |description: &str| drasi_computation_plugin_sdk::configuration::ConfigFieldDetails {
            description: description.into(),
            ..Default::default()
        };
    ConfigDetails {
        fields: [
            ("query", describe("The continuous query text.")),
            (
                "volatile",
                describe("Use the explicitly volatile processing profile (in-memory providers)."),
            ),
            (
                "publication",
                describe("Output publication: atomic (default) or explicit non-atomic."),
            ),
            ("language", describe("Query language: cypher (default) or gql.")),
            ("joins", describe("Synthetic joins between input labels.")),
            ("inputs", describe("The query's declared inputs and their labels.")),
            ("middleware", describe("Input-local middleware applied before evaluation.")),
            ("result", describe("Result dataset settings, such as its label.")),
            ("stream", describe("Output stream; defaults to <component>/out.")),
            (
                "outboxCapacity",
                describe("Retained output capacity; a positive count, one by default."),
            ),
            (
                "bootstrap",
                describe("Initialize from the provider bound on the bootstrap input."),
            ),
            (
                "sourceTransactions",
                describe("Limits for complete source transactions (maxChanges, maxBytes, maxDurationMs)."),
            ),
            ("recovery", describe("Recovery policy: strict (default) or auto_reset.")),
            (
                "resetConfiguration",
                describe("Request a reset of retained state for a changed configuration."),
            ),
        ]
        .into_iter()
        .map(|(name, details)| (name.to_string(), details))
        .collect(),
    }
}

impl Factory for IndexedQueryFactory {
    fn supports_query_instrumentation(&self) -> bool {
        true
    }

    fn configuration_details(&self) -> ConfigDetails {
        query_configuration_details()
    }
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        transformer::describe(request)
    }
    fn metadata(&self) -> FactoryMetadata {
        let output = || PortId::try_new(transformer::OUTPUT_PORT).expect("constant port");
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(INDEXED_IMPLEMENTATION, "1")
                .expect("constant implementation"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: query_configuration_schema(
                [
                    ("query", ConfigType::String, true),
                    ("language", ConfigType::String, false),
                    ("joins", ConfigType::Array, false),
                    ("inputs", ConfigType::Array, false),
                    ("middleware", ConfigType::Array, false),
                    ("result", ConfigType::Object, false),
                    ("sourceTransactions", ConfigType::Object, false),
                    ("stream", ConfigType::String, false),
                ]
                .into_iter()
                .chain(transformer::configuration_fields()),
                &self.middleware,
            ),
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(name, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(name).expect("constant port"),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: None,
            // A query is a stateful transformer; it adds its query reads.
            capabilities: Capabilities {
                semantic_kind: Some(ComponentSemanticKind::Query),
                output_queries: vec![output()],
                query_observations: true,
                query_recovery_views: true,
                update_modes: vec![ComponentUpdateMode::InPlace, ComponentUpdateMode::Recreate],
                ..transformer::capabilities(true)
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        services: ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        create_query(
            request,
            self.metadata(),
            services.processing.ok_or_else(|| {
                anyhow::anyhow!("indexed query requires host-owned indexed processing")
            })?,
            self.middleware.clone(),
        )
    }
    fn update_planner(&self) -> Option<&dyn drasi_computation_plugin_sdk::UpdatePlanner> {
        Some(self)
    }
}

impl drasi_computation_plugin_sdk::UpdatePlanner for IndexedQueryFactory {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        update::plan(request, &self.middleware, &self.metadata()).map(Assessment::from)
    }
}

fn query_configuration(
    request: &CreateRequest,
    middleware: &MiddlewareTypeRegistry,
) -> anyhow::Result<(ContinuousQueryDefinition, QueryExecutionSettings)> {
    let query = request.configuration["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("query must be a string"))?;
    let language = match request.configuration.get("language") {
        None => QueryLanguage::Cypher,
        Some(serde_json::Value::String(language)) if language == "cypher" => QueryLanguage::Cypher,
        Some(serde_json::Value::String(language)) if language == "gql" => QueryLanguage::Gql,
        Some(_) => anyhow::bail!("query language must be 'cypher' or 'gql'"),
    };
    let mut execution: QueryExecutionSettings = serde_json::from_value(serde_json::Value::Object(
        ["joins", "result", "inputs", "middleware"]
            .into_iter()
            .filter_map(|name| {
                request
                    .configuration
                    .get(name)
                    .cloned()
                    .map(|value| (name.into(), value))
            })
            .collect(),
    ))
    .context("invalid query execution settings")?;
    execution.source_transactions = request
        .configuration
        .get("sourceTransactions")
        .map(|value| serde_json::from_value::<SourceTransactionSettings>(value.clone()))
        .transpose()
        .context("invalid sourceTransactions")?
        .map(Into::into);
    execution.validate(Some(middleware))?;
    let id = request.id.clone();
    let stream = match request.configuration.get("stream") {
        Some(value) => StreamId::try_new(value.as_str().context("query stream must be a string")?)?,
        None => StreamId::try_new(format!("{id}/out"))?,
    };
    let capacity = request
        .configuration
        .get("outboxCapacity")
        .map(|value| serde_json::from_value::<NonZeroUsize>(value.clone()))
        .transpose()
        .context("invalid query output retention capacity")?
        .unwrap_or_else(|| NonZeroUsize::new(1).expect("positive default retention"));
    let definition = ContinuousQueryDefinition {
        graph_id: request
            .scope
            .as_ref()
            .map_or("native-standalone", |scope| scope.graph_id.as_str())
            .into(),
        id: id.clone(),
        query: query.to_owned(),
        language,
        output_stream: stream.clone(),
        outbox_capacity: capacity,
    };
    Ok((definition, execution))
}

fn create_query(
    request: &CreateRequest,
    metadata: FactoryMetadata,
    processing: NativeProcessing,
    middleware: Arc<MiddlewareTypeRegistry>,
) -> anyhow::Result<CreatedComponent> {
    transformer::validate_reconstruction(request)?;
    let (definition, execution) = query_configuration(request, &middleware)?;
    let definition = Arc::new(definition);
    let input_scopes = if !request.inputs.is_empty()
        && execution
            .inputs
            .iter()
            .any(|input| !input.pipeline.is_empty())
    {
        Some(drasi_query_common::query_input_scopes(
            &request.id,
            &execution.inputs,
            &request.inputs,
        )?)
    } else {
        None
    };
    let instance = request
        .scope
        .as_ref()
        .map_or("native-standalone", |scope| scope.instance_id.as_str());
    let projector = definition.projector(instance, &execution)?;
    let options = transformer::Options::parse(&request.configuration)?;
    let description = QueryDescription {
        graph_id: definition.graph_id.clone(),
        query: definition.query.clone(),
        language: definition.language,
        execution: execution.clone(),
        output_stream: definition.output_stream.clone(),
        outbox_capacity: definition.outbox_capacity,
        bootstrap: options.bootstrap,
        auto_reset: options.recovery == QueryRecoveryPolicy::AutoReset,
        volatile: options.volatile,
        configuration_hash: definition.configuration_hash(&execution)?,
    };
    let settings = transformer::Settings {
        stream: definition.output_stream.clone(),
        capacity: definition.outbox_capacity,
        dataset: projector.dataset().to_owned(),
        identity: serde_json::from_slice(&definition.configuration_bytes(&execution)?)?,
        source_transactions: execution.source_transactions,
    };
    let descriptor = metadata.descriptor(request.id.clone())?;
    let logic = logic::QueryLogic {
        definition,
        execution: Arc::new(execution),
        middleware,
        projector,
        indexes: Arc::new(indexed::Indexes::default()),
        input_scopes: std::sync::RwLock::new(input_scopes),
        description,
        metadata,
    };
    Ok(
        Component::Transformer(Box::new(transformer::StatefulTransformer::new(
            request, descriptor, processing, logic, settings,
        )?))
        .into(),
    )
}

async fn evaluator(
    query: &str,
    language: QueryLanguage,
    execution: &QueryExecutionSettings,
    indexes: Option<Arc<indexed::Indexes>>,
    middleware: Arc<MiddlewareTypeRegistry>,
) -> anyhow::Result<QueryEvaluator> {
    let mut builder = query_builder(query, language, execution, Some(middleware))?;
    if let Some(indexes) = indexes {
        builder = builder
            .with_element_index(indexes.clone())
            .with_archive_index(indexes.clone())
            .with_result_index(indexes.clone())
            .with_future_queue(indexes);
    }
    Ok(builder.try_build_evaluator().await?)
}

#[cfg(test)]
mod declared_middleware_tests {
    use super::*;
    use serde_json::json;

    fn configuration(kind: &str) -> serde_json::Value {
        json!({"query": "MATCH (n) RETURN n",
               "middleware": [{"name": "m", "kind": kind, "config": {}}]})
    }

    #[test]
    fn the_factory_declares_exactly_the_registered_middleware_kinds() {
        let factory = IndexedQueryFactory::default();
        let schema = factory.metadata().configuration;
        for kind in default_middleware_registry().kinds() {
            assert!(schema.validate(&configuration(kind)).is_ok(), "{kind}");
        }
        assert!(schema.validate(&configuration("unregistered")).is_err());
        assert!(schema
            .validate(&json!({"query": "MATCH (n) RETURN n",
                              "middleware": [{"name": "m", "kind": "map"}]}))
            .is_err());

        let mut registry = MiddlewareTypeRegistry::new();
        registry.register(Arc::new(
            drasi_middleware::relabel::RelabelMiddlewareFactory::new(),
        ));
        let narrowed = IndexedQueryFactory::new(Arc::new(registry))
            .metadata()
            .configuration;
        assert!(narrowed.validate(&configuration("relabel")).is_ok());
        assert!(narrowed.validate(&configuration("map")).is_err());
    }
}
