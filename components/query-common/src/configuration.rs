// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use drasi_computation_contracts::{
    validate_identifier, validate_query_joins, ComputationQueryLanguage, InputBinding,
    InputEnvelope, PortId, QueryExecutionSettings, QueryInputConfig, StreamId,
};
use drasi_core::{
    evaluation::functions::FunctionRegistry, middleware::MiddlewareTypeRegistry,
    models::SourceMiddlewareConfig, query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_functions_gql::GQLFunctionSet;
use drasi_query_ast::api::QueryParser;
use drasi_query_cypher::CypherParser;
use drasi_query_gql::GQLParser;

pub fn validate_middleware_configs<'a>(
    configs: &'a [SourceMiddlewareConfig],
    registry: Option<&MiddlewareTypeRegistry>,
) -> anyhow::Result<BTreeSet<&'a str>> {
    let mut names = BTreeSet::new();
    for config in configs {
        validate_identifier("middleware", &config.name)?;
        validate_identifier("middleware kind", &config.kind)?;
        anyhow::ensure!(
            names.insert(config.name.as_ref()),
            "duplicate middleware name"
        );
        if registry.is_some_and(|registry| registry.get(&config.kind).is_none()) {
            anyhow::bail!("middleware kind {} is not registered", config.kind);
        }
    }
    Ok(names)
}

pub trait QueryExecutionSettingsExt {
    fn validate(&self, registry: Option<&MiddlewareTypeRegistry>) -> anyhow::Result<()>;
    fn configure(
        &self,
        builder: QueryBuilder,
        registry: Option<Arc<MiddlewareTypeRegistry>>,
    ) -> anyhow::Result<QueryBuilder>;
}

impl QueryExecutionSettingsExt for QueryExecutionSettings {
    fn validate(&self, registry: Option<&MiddlewareTypeRegistry>) -> anyhow::Result<()> {
        validate_query_joins(&self.joins)?;
        let middleware = validate_middleware_configs(&self.middleware, registry)?;
        self.result.validate()?;
        let mut inputs = BTreeSet::new();
        for input in &self.inputs {
            input.validate()?;
            if !inputs.insert(&input.component) {
                anyhow::bail!("duplicate query input '{}'", input.component);
            }
            for name in &input.pipeline {
                if !middleware.contains(name.as_str()) {
                    anyhow::bail!("input pipeline references undeclared middleware {name}");
                }
            }
        }
        Ok(())
    }

    fn configure(
        &self,
        mut builder: QueryBuilder,
        registry: Option<Arc<MiddlewareTypeRegistry>>,
    ) -> anyhow::Result<QueryBuilder> {
        self.validate(registry.as_deref())?;
        if !self.middleware.is_empty() {
            builder = builder.with_middleware_registry(registry.ok_or_else(|| {
                anyhow::anyhow!("query middleware requires a registered middleware resource")
            })?);
        }
        for config in &self.middleware {
            builder = builder.with_source_middleware(Arc::new(config.clone()));
        }
        for input in &self.inputs {
            builder = builder.with_source_pipeline(&input.component, &input.pipeline);
        }
        Ok(builder.with_joins(self.joins.iter().cloned().map(Into::into).collect()))
    }
}

pub fn parser(language: ComputationQueryLanguage) -> (Arc<dyn QueryParser>, Arc<FunctionRegistry>) {
    match language {
        ComputationQueryLanguage::Cypher => {
            let functions = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
            (Arc::new(CypherParser::new(functions.clone())), functions)
        }
        ComputationQueryLanguage::Gql => {
            let functions = Arc::new(FunctionRegistry::new()).with_gql_function_set();
            (Arc::new(GQLParser::new(functions.clone())), functions)
        }
    }
}

pub fn query_builder(
    query: &str,
    language: ComputationQueryLanguage,
    execution: &QueryExecutionSettings,
    registry: Option<Arc<MiddlewareTypeRegistry>>,
) -> anyhow::Result<QueryBuilder> {
    let (parser, functions) = parser(language);
    execution.configure(
        QueryBuilder::new(query, parser).with_function_registry(functions),
        registry,
    )
}

pub type QueryInputScopes = BTreeMap<(PortId, StreamId), Arc<str>>;

pub fn query_input_scopes(
    query: &str,
    inputs: &[QueryInputConfig],
    bindings: &[InputBinding],
) -> anyhow::Result<QueryInputScopes> {
    let mut aliases = BTreeMap::new();
    for input in inputs {
        aliases.insert(
            drasi_computation_contracts::query_source_component_id(query, &input.component)?,
            Arc::<str>::from(input.component.as_str()),
        );
    }
    let mut scopes = BTreeMap::new();
    for binding in bindings {
        let scope = aliases
            .get(&binding.output.component)
            .cloned()
            .unwrap_or_else(|| Arc::from(binding.output.component.as_str()));
        let key = (binding.input_port.clone(), binding.stream.clone());
        if let Some(previous) = scopes.insert(key, scope.clone()) {
            anyhow::ensure!(
                previous == scope,
                "query input port '{}' stream '{}' has ambiguous producer scopes",
                binding.input_port,
                binding.stream,
            );
        }
    }
    Ok(scopes)
}

pub fn query_input_scope(
    scopes: Option<&QueryInputScopes>,
    input: &InputEnvelope,
) -> anyhow::Result<Option<Arc<str>>> {
    scopes
        .map(|scopes| {
            scopes
                .get(&(input.port.clone(), input.envelope.system().stream().clone()))
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "query input port '{}' stream '{}' has no declared middleware scope",
                        input.port,
                        input.envelope.system().stream(),
                    )
                })
        })
        .transpose()
}
