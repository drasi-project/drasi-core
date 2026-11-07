// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use async_trait::async_trait;
use drasi_computation_plugin_sdk::{CreateRequest, Scope, UpdateRequest};
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

fn add_execution(
    configuration: &mut serde_json::Value,
    execution: &QueryExecutionSettings,
) -> anyhow::Result<()> {
    let settings = serde_json::to_value(execution)?;
    let settings = settings
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("query execution settings are not an object"))?;
    for (key, value) in settings.iter().filter(|(_, value)| !value.is_null()) {
        configuration[key] = value.clone();
    }
    Ok(())
}

pub(super) fn configuration_for_update(
    specification: &ComponentSpecification,
    volatile: Option<bool>,
) -> anyhow::Result<serde_json::Value> {
    let configuration = super::factory::literal_configuration(specification)?;
    let values = configuration
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("query configuration must be an object"))?;
    let mut configuration = serde_json::json!({
        "query": values.get("query").ok_or_else(|| anyhow::anyhow!("query text is missing"))?,
        "stream": values.get("stream").ok_or_else(|| anyhow::anyhow!("query stream is missing"))?,
        "language": values.get("language").cloned().unwrap_or_else(|| serde_json::json!("cypher")),
        "outbox_capacity": values.get("outbox_capacity").cloned().unwrap_or_else(|| serde_json::json!(1000)),
        "recovery": values.get("recovery").cloned().unwrap_or_else(|| serde_json::json!("strict")),
        "publication": values.get("publication").cloned().unwrap_or_else(|| serde_json::json!("atomic")),
        "reset_configuration": values.get("reset_configuration").cloned().unwrap_or(serde_json::json!(false)),
        "bootstrap": specification.dependencies.get("bootstrap").is_some_and(|ids| !ids.is_empty()),
    });
    if let Some(volatile) = volatile {
        configuration["volatile"] = serde_json::json!(volatile);
    }
    let execution = values
        .get("execution")
        .map(|value| serde_json::from_value::<QueryExecutionSettings>(value.clone()))
        .transpose()?
        .unwrap_or_default();
    add_execution(&mut configuration, &execution)?;
    Ok(configuration)
}

fn retained_host_configuration_matches(
    current: &ComponentSpecification,
    proposed: &ComponentSpecification,
) -> bool {
    let host_only = |specification: &ComponentSpecification| {
        specification
            .configuration
            .iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_ref(),
                    "query"
                        | "stream"
                        | "language"
                        | "outbox_capacity"
                        | "execution"
                        | "recovery"
                        | "publication"
                        | "reset_configuration"
                )
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    current.dependencies == proposed.dependencies && host_only(current) == host_only(proposed)
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
    fn plan_update(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        let metadata = self.factory.metadata();
        let implementation = ImplementationIdentity::try_new(
            format!("{}/ordinary", metadata.implementation.name),
            metadata.implementation.version.clone(),
        )?;
        anyhow::ensure!(
            current.implementation == implementation
                && proposed.implementation == implementation
                && current.configuration_version == proposed.configuration_version
                && current.descriptor == proposed.descriptor
                && current.role == ComponentRole::Transformer
                && proposed.role == ComponentRole::Transformer
                && current.completion.is_none()
                && proposed.completion.is_none(),
            "ordinary native query update cannot change its factory, identity or ports"
        );
        let request = |specification: &ComponentSpecification| -> anyhow::Result<CreateRequest> {
            Ok(CreateRequest {
                id: specification.descriptor.id().clone(),
                implementation: metadata.implementation.clone(),
                configuration_version: metadata.configuration_version,
                configuration: configuration_for_update(specification, None)?,
                scope: None,
                inputs: Vec::new(),
                activation_credentials_bound: false,
                update_directive: None,
            })
        };
        let plan = self.factory.plan_update_request(&UpdateRequest {
            current: request(current)?,
            proposed: request(proposed)?,
        })?;
        anyhow::ensure!(
            plan.mode == ComponentUpdateMode::Recreate
                || retained_host_configuration_matches(current, proposed),
            "retained native query updates cannot change host resources, declaration or dispatch settings"
        );
        Ok(plan)
    }

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
            add_execution(&mut configuration, &construction.execution)?;
            let resource = super::NativeProcessingResource::new(construction.provider.clone())
                .with_publication(construction.options.publication);
            let mut component = self.factory.create_processing_component(
                construction.definition.id.clone(),
                configuration,
                Scope {
                    instance_id: construction.instance_id.to_string(),
                    graph_id: construction.definition.graph_id.clone(),
                    generation: construction.generation.0,
                },
                &resource,
                construction.source_progress.clone(),
                construction
                    .update_directive
                    .as_ref()
                    .map(super::factory::update_directive),
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
    fn ordinary_query_update_planning_uses_the_native_factory_and_keeps_host_settings(
    ) -> anyhow::Result<()> {
        let plugin = NativePlugin::from_definition(drasi_computation_transformer_query::plugin()?)?;
        let factory = plugin.query_factory()?;
        let current = factory.specification_from_config(
            ComponentId::try_new("a6e2d3f0-21c4-4322-a751-9b76a553d83a")?,
            serde_json::json!({"query": "MATCH (n) RETURN n", "outbox_capacity": 2}),
        )?;
        let mut proposed = current.clone();
        proposed.configuration.insert(
            "outbox_capacity".into(),
            ConfigurationValue::Literal(serde_json::json!(4)),
        );
        assert_eq!(
            factory.plan_update(&current, &proposed)?,
            ComponentUpdatePlan {
                mode: ComponentUpdateMode::InPlace,
                data: ComponentUpdateData::Preserve,
            }
        );
        proposed.configuration.insert(
            "default_dispatch_capacity".into(),
            ConfigurationValue::Literal(serde_json::json!(32)),
        );
        assert!(factory.plan_update(&current, &proposed).is_err());
        Ok(())
    }

    #[test]
    fn ordinary_semantic_query_updates_require_declared_bootstrap() -> anyhow::Result<()> {
        let plugin = NativePlugin::from_definition(drasi_computation_transformer_query::plugin()?)?;
        let factory = plugin.query_factory()?;
        let mut current = factory.specification_from_config(
            ComponentId::try_new("2bf52676-2c7d-4c33-aa78-a0765a72c020")?,
            serde_json::json!({"query": "MATCH (n:Item) RETURN n.value AS value"}),
        )?;
        let mut proposed = current.clone();
        proposed.configuration.insert(
            "query".into(),
            ConfigurationValue::Literal(serde_json::json!(
                "MATCH (n:Item) WHERE n.value > 3 RETURN n.value AS value"
            )),
        );
        assert!(factory.plan_update(&current, &proposed).is_err());
        let bootstrap = vec![ResourceId::try_new("bootstrap")?];
        current
            .dependencies
            .insert("bootstrap".into(), bootstrap.clone());
        proposed.dependencies.insert("bootstrap".into(), bootstrap);
        assert_eq!(
            factory.plan_update(&current, &proposed)?,
            ComponentUpdatePlan {
                mode: ComponentUpdateMode::Recreate,
                data: ComponentUpdateData::Rebootstrap,
            }
        );
        Ok(())
    }

    #[test]
    fn registered_plugins_restore_native_query_authoring_recipes() -> anyhow::Result<()> {
        let plugin = NativePlugin::from_definition(drasi_computation_transformer_query::plugin()?)?;
        let authoring = plugin.query_factory()?;
        let specification = authoring.specification_from_config(
            ComponentId::try_new("585170f0-3260-46db-afcd-fc33b43573b6")?,
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
        assert_eq!(
            configuration.id.as_str(),
            "585170f0-3260-46db-afcd-fc33b43573b6"
        );
        assert_eq!(
            configuration.query,
            "MATCH (n:Item) RETURN n.value AS value"
        );
        assert_eq!(
            configuration.inputs[0].component,
            drasi_lib::config::ComponentReference::by_name("inventory")?
        );
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
