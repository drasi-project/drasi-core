// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use async_trait::async_trait;
use drasi_computation_plugin_sdk::Scope;
use drasi_lib::computation::v1::*;

use super::{NativeFactory, NativePlugin};

pub(crate) fn select_query_factory<'a>(
    factories: impl IntoIterator<Item = &'a Arc<NativeFactory>>,
) -> anyhow::Result<Option<Arc<dyn ComponentFactory>>> {
    let mut candidates = factories
        .into_iter()
        .filter(|factory| factory.supports_ordinary_queries());
    let Some(factory) = candidates.next() else {
        return Ok(None);
    };
    if let Some(other) = candidates.next() {
        anyhow::bail!(
            "multiple native query factories are loaded ('{}' and '{}'); explicitly select a query factory for this instance",
            factory.metadata().implementation.name, other.metadata().implementation.name,
        );
    }
    factory.for_ordinary_queries().map(Some)
}

impl NativePlugin {
    /// Select this plugin's unique ordinary-query implementation for explicit
    /// registration. Absence and ambiguity are errors, never a linked fallback.
    pub fn query_factory(&self) -> anyhow::Result<Arc<dyn ComponentFactory>> {
        select_query_factory(self.factories())?
            .ok_or_else(|| anyhow::anyhow!("native plugin has no ordinary query factory"))
    }
}

struct NativeQueryConstructor {
    factory: Arc<NativeFactory>,
}

pub(super) struct QueryBinding {
    pub(super) construction: QueryConstruction,
    pub(super) session: Option<QuerySchedulingSession>,
    pub(super) observer: Option<QuerySchedulingObservation>,
    pub(super) draining: bool,
}

impl NativeFactory {
    pub fn supports_ordinary_queries(&self) -> bool {
        self.metadata().role == ComponentRole::Transformer
            && self.metadata().capabilities.semantic_kind == Some(ComponentSemanticKind::Query)
            && self.metadata().capabilities.indexed_processing
            && self
                .metadata()
                .capabilities
                .committed_input_progress
                .is_some()
            && self.metadata().capabilities.durable_output
    }

    /// Use this plugin for ordinary query declarations and their actual graph
    /// resources. Middleware implementations belong to the selected plugin;
    /// host-process Rust factories are not transferred through the native ABI.
    pub fn for_ordinary_queries(self: &Arc<Self>) -> anyhow::Result<Arc<dyn ComponentFactory>> {
        anyhow::ensure!(
            self.supports_ordinary_queries(),
            "ordinary indexed queries require the native query processing and output capabilities"
        );
        let mut cached = self
            .query_authoring
            .lock()
            .map_err(|_| anyhow::anyhow!("query authoring factory cache poisoned"))?;
        if let Some(factory) = cached.as_ref().and_then(std::sync::Weak::upgrade) {
            return Ok(factory);
        }
        let implementation = ImplementationIdentity::try_new(
            format!("{}/ordinary", self.metadata().implementation.name),
            self.metadata().implementation.version.clone(),
        )?;
        let factory: Arc<dyn ComponentFactory> =
            Arc::new(ContinuousQueryFactory::with_constructor(
                implementation,
                Arc::new(NativeQueryConstructor {
                    factory: self.clone(),
                }),
            ));
        *cached = Some(Arc::downgrade(&factory));
        Ok(factory)
    }
}

#[async_trait]
impl QueryConstructor for NativeQueryConstructor {
    fn query_descriptor(&self, id: ComponentId) -> anyhow::Result<ComponentDescriptor> {
        self.factory.metadata().descriptor(id)
    }

    fn validate_execution(
        &self,
        execution: &QueryExecutionSettings,
        _: Option<&drasi_core::middleware::MiddlewareTypeRegistry>,
    ) -> anyhow::Result<()> {
        drasi_query_common::QueryExecutionSettingsExt::validate(execution, None)
    }

    async fn create_query(
        &self,
        construction: QueryConstruction,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let create = async {
            anyhow::ensure!(
                construction.provider.is_volatile()
                    || construction.options.publication == QueryPublicationMode::Atomic
                    || self.factory.metadata().capabilities.non_atomic_publication,
                "selected native query factory does not support persistent non-atomic publication"
            );
            anyhow::ensure!(
                !construction.provider.is_volatile()
                    || self.factory.metadata().capabilities.volatile_processing,
                "selected native query factory does not support volatile indexed processing"
            );
            let mut configuration = serde_json::json!({
                "query": construction.definition.query,
                "volatile": construction.provider.is_volatile(),
                "publication": match construction.options.publication {
                    QueryPublicationMode::Atomic => "atomic",
                    QueryPublicationMode::NonAtomic => "non_atomic",
                },
                "language": match construction.definition.language {
                    ComputationQueryLanguage::Cypher => "cypher",
                    ComputationQueryLanguage::Gql => "gql",
                },
                "stream": construction.definition.output_stream,
                "outbox_capacity": construction.definition.outbox_capacity,
                "bootstrap": construction.bootstrap.is_some(),
                "reset_configuration": construction.reset_configuration,
                "recovery": match construction.options.recovery {
                    QueryRecoveryPolicy::Strict => "strict",
                    QueryRecoveryPolicy::AutoReset => "auto_reset",
                },
            });
            let settings = serde_json::to_value(&construction.execution)?;
            let settings = settings
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("query execution settings are not an object"))?;
            for (key, value) in settings.iter().filter(|(_, value)| !value.is_null()) {
                configuration[key] = value.clone();
            }
            let mut component = self.factory.create_component_with_processing_publication(
                construction.definition.id.clone(),
                configuration,
                Scope {
                    instance_id: construction.instance_id.to_string(),
                    graph_id: construction.definition.graph_id.clone(),
                    generation: construction.generation.0,
                },
                construction.provider.clone(),
                construction.source_progress.clone(),
                construction.options.publication,
            )?;
            let defer_build = construction.defer_build;
            component.bind_query_construction(construction)?;
            if !defer_build {
                component.prepare_processing().await?;
            }
            Ok::<_, anyhow::Error>(ConstructedComponent::transformer(Box::new(component)))
        };
        create.await.map_err(ComponentCreationError::terminal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_plugins_restore_native_query_authoring_recipes() -> anyhow::Result<()> {
        let plugin = NativePlugin::from_definition(drasi_computation_transformer_query::plugin()?)?;
        let authoring = plugin.query_factory()?;
        let specification = authoring.specification_from_config(
            ComponentId::try_new("restored-query")?,
            serde_json::json!({
                "query": "MATCH (n:Item) RETURN n.value AS value",
                "execution": {"inputs": ["inventory"]}
            }),
        )?;
        let mut plugins = crate::PluginRegistry::new();
        plugins.register_computation_plugin(plugin.clone())?;
        let registry = plugins.computation_factory_registry()?;
        let registered = registry
            .get(&specification.implementation)
            .ok_or_else(|| anyhow::anyhow!("native query recipe factory is not registered"))?;
        assert!(Arc::ptr_eq(registered, &authoring));
        drop(authoring);
        drop(plugins);
        drop(plugin);

        let restored: ComponentSpecification =
            serde_json::from_value(serde_json::to_value(specification)?)?;
        let configuration = registered
            .query_authoring()
            .ok_or_else(|| {
                anyhow::anyhow!("registered query recipe lost its authoring capability")
            })?
            .query_configuration(&restored, "restored-graph")?;
        assert_eq!(configuration.id, "restored-query");
        assert_eq!(
            configuration.query,
            "MATCH (n:Item) RETURN n.value AS value"
        );
        assert_eq!(configuration.inputs[0].component, "inventory");
        Ok(())
    }

    #[test]
    fn query_selection_is_explicit_unambiguous_and_reuses_the_registered_factory(
    ) -> anyhow::Result<()> {
        let plugin = NativePlugin::from_definition(drasi_computation_transformer_query::plugin()?)?;
        let first = plugin.query_factory()?;
        assert_eq!(
            first.descriptor().implementation.name.as_ref(),
            "drasi.experimental/indexed-query/ordinary"
        );
        assert!(Arc::ptr_eq(&first, &plugin.query_factory()?));
        let mut registry = crate::PluginRegistry::new();
        registry.register_computation_plugin(plugin.clone())?;
        assert!(Arc::ptr_eq(
            &first,
            &registry.query_factory()?.expect("registered")
        ));
        assert!(select_query_factory(std::iter::empty())?.is_none());
        let indexed = plugin
            .factories()
            .iter()
            .find(|factory| factory.supports_ordinary_queries())
            .expect("indexed factory");
        assert!(select_query_factory([indexed, indexed]).is_err());
        Ok(())
    }
}
