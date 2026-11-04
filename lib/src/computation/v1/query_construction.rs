// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{any::Any, num::NonZeroUsize, result::Result, sync::Arc};

use super::*;
use crate::config::QueryConfig;
use drasi_core::{computation::ComputationIndexProvider, middleware::MiddlewareTypeRegistry};
use drasi_query_common::QueryExecutionSettingsExt;

#[derive(Debug, thiserror::Error)]
#[error("Query creation requires an explicitly registered query factory; use with_query_factory on DrasiLibBuilder or query_factory on ComputationPipelineBuilder")]
pub struct QueryFactoryNotRegistered;

/// Ordinary query authoring is separate from the selected execution implementation.
pub trait QueryAuthoring: Send + Sync {
    fn query_descriptor(&self, id: ComponentId) -> anyhow::Result<ComponentDescriptor>;

    fn query_configuration(
        &self,
        specification: &ComponentSpecification,
        graph_id: &str,
    ) -> anyhow::Result<QueryConfig>;

    fn validate_execution(
        &self,
        execution: &QueryExecutionSettings,
        middleware: Option<&MiddlewareTypeRegistry>,
    ) -> anyhow::Result<()>;
}

/// Resolved query authoring inputs and the graph's actual declared resources.
/// Preparing this value neither creates an evaluator nor opens storage.
pub struct QueryConstruction {
    pub instance_id: Arc<str>,
    pub generation: ComponentGeneration,
    pub definition: ContinuousQueryDefinition,
    pub provider: Arc<dyn ComputationIndexProvider>,
    pub execution: QueryExecutionSettings,
    pub options: QueryOptions,
    pub middleware: Option<Arc<MiddlewareTypeRegistry>>,
    pub bootstrap: Option<Arc<dyn ComputationBootstrapProvider>>,
    pub source_progress: Option<Arc<QuerySourceProgress>>,
    pub catalog: Option<QueryResultsCatalog>,
    pub transport_sequence: Option<Arc<std::sync::atomic::AtomicU64>>,
    pub scheduling: Option<Arc<QuerySchedulingResource>>,
    pub configuration: Option<QueryConfig>,
    pub source_hosts: Vec<Arc<SourcePluginHost>>,
    pub default_dispatch_capacity: usize,
    pub reset_configuration: bool,
    pub defer_build: bool,
    pub(super) declaration: Option<Arc<super::query_configuration::OrdinaryQueryDeclaration>>,
}

/// Execution construction behind the existing query authoring and validation path.
#[async_trait::async_trait]
pub trait QueryConstructor: Send + Sync {
    fn query_descriptor(&self, id: ComponentId) -> anyhow::Result<ComponentDescriptor> {
        Ok(drasi_query_common::query_descriptor(id))
    }

    fn validate_execution(
        &self,
        execution: &QueryExecutionSettings,
        middleware: Option<&MiddlewareTypeRegistry>,
    ) -> anyhow::Result<()> {
        execution.validate(middleware)
    }

    async fn create_query(
        &self,
        construction: QueryConstruction,
    ) -> Result<ConstructedComponent, ComponentCreationError>;
}

pub fn validate_query_sources(
    persistent: bool,
    query: &ComponentId,
    sources: &[Arc<SourcePluginHost>],
) -> anyhow::Result<()> {
    if persistent {
        for source in sources {
            anyhow::ensure!(
                source.source()?.supports_replay(),
                "IncompatibleSource: sources that do not support replay cannot feed persistent query '{}' (source '{}', supports_replay=false)",
                query,
                source.id()
            );
        }
    }
    Ok(())
}

fn optional<T: Any + Send + Sync>(
    context: &ConstructionContext,
    name: &str,
) -> Result<Option<Arc<T>>, ComponentCreationError> {
    if context.specification.dependencies.contains_key(name) {
        Ok(context
            .resources::<T>(name)
            .map_err(ComponentCreationError::terminal)?
            .pop())
    } else {
        Ok(None)
    }
}

impl QueryConstruction {
    pub(super) fn prepare(context: ConstructionContext) -> Result<Self, ComponentCreationError> {
        let provider =
            optional::<QueryIndexProviderResource>(&context, "indexes")?.ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing index provider"))
            })?;
        let config = context.configuration();
        let text = config
            .get("query")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ComponentCreationError::terminal(anyhow::anyhow!("missing query")))?;
        let stream = config
            .get("stream")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ComponentCreationError::terminal(anyhow::anyhow!("missing stream")))?;
        let capacity = match config.get("outbox_capacity") {
            None => 1000,
            Some(value) => value.as_u64().ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("invalid outbox capacity"))
            })?,
        };
        let capacity = usize::try_from(capacity)
            .ok()
            .and_then(NonZeroUsize::new)
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("invalid outbox capacity"))
            })?;
        let definition = ContinuousQueryDefinition {
            graph_id: context.graph_id.to_string(),
            id: context.component_id.clone(),
            query: text.to_owned(),
            language: super::query::language(config.get("language"))
                .map_err(ComponentCreationError::terminal)?,
            output_stream: StreamId::try_new(stream).map_err(ComponentCreationError::terminal)?,
            outbox_capacity: capacity,
        };
        let execution = config
            .get("execution")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(ComponentCreationError::terminal)?
            .unwrap_or_default();
        let middleware = optional::<QueryMiddlewareResource>(&context, "middleware")?
            .map(|resource| resource.0.clone());
        let mut declaration = optional::<super::query_configuration::OrdinaryQueryDeclaration>(
            &context,
            "declaration",
        )?
        .map(|declaration| (*declaration).clone());
        if let Some(declaration) = &mut declaration {
            if context
                .specification
                .dependencies
                .contains_key("source_hosts")
            {
                declaration.sources = context
                    .resources::<SourcePluginHost>("source_hosts")
                    .map_err(ComponentCreationError::terminal)?;
            }
        }
        let source_hosts = declaration
            .as_ref()
            .map(|declaration| declaration.sources.clone())
            .unwrap_or_default();
        let default_dispatch_capacity = declaration
            .as_ref()
            .map_or(1000, |declaration| declaration.default_dispatch_capacity);
        let catalog =
            optional::<QueryResultsCatalog>(&context, "catalog")?.map(|catalog| (*catalog).clone());
        let transport_sequence = catalog
            .as_ref()
            .map(|catalog| catalog.transport_sequence(&definition.output_stream))
            .transpose()
            .map_err(ComponentCreationError::terminal)?;
        Ok(Self {
            instance_id: context.instance_id.clone(),
            generation: context.generation,
            definition,
            provider: provider.0.clone(),
            execution,
            options: super::query::options(config).map_err(ComponentCreationError::terminal)?,
            middleware,
            bootstrap: optional::<QueryBootstrapResource>(&context, "bootstrap")?
                .map(|resource| resource.0.clone()),
            source_progress: optional::<QuerySourceProgressResource>(&context, "source_progress")?
                .map(|resource| resource.0.clone()),
            catalog,
            transport_sequence,
            scheduling: optional::<QuerySchedulingResource>(&context, "scheduling")?,
            configuration: config
                .get("query_config")
                .map(|config| serde_json::from_value(config.clone()))
                .transpose()
                .map_err(ComponentCreationError::terminal)?,
            source_hosts,
            default_dispatch_capacity,
            reset_configuration: config
                .get("reset_configuration")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            defer_build: config
                .get("defer_build")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            declaration: declaration.map(Arc::new),
        })
    }
}
