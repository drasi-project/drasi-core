// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use drasi_lib::computation::v1::{
    ComponentCreationError, ComponentFactory, ComponentRole, ComponentSpecification,
    ConfigurationField, ConfigurationSchema, ConfigurationType, ConfigurationValue,
    ConstructedComponent, ConstructionContext, FactoryDescriptor, ImplementationIdentity,
    QuerySourceProgressResource, ResourceHandle, ResourceId, ResourceRequirement, ResourceRole,
    ResourceSpecification, StreamId,
};
use std::{collections::BTreeMap, sync::Arc};

pub struct MySqlSourceFactory {
    descriptor: FactoryDescriptor,
}

impl Default for MySqlSourceFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("drasi/mysql-native", "1")
                    .expect("implementation"),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([
                        (
                            Arc::from("stream"),
                            ConfigurationField {
                                value_type: ConfigurationType::String,
                                required: true,
                                secret: false,
                            },
                        ),
                        (
                            Arc::from("settings"),
                            ConfigurationField {
                                value_type: ConfigurationType::Object,
                                required: true,
                                secret: true,
                            },
                        ),
                    ]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([
                    (
                        Arc::from("source_progress"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<QuerySourceProgressResource>(
                                ResourceRole::Checkpoint,
                            )
                        },
                    ),
                    (
                        Arc::from("snapshot"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<MySqlSnapshot>(
                                ResourceRole::Bootstrap,
                            )
                        },
                    ),
                ]),
            },
        }
    }
}

#[async_trait]
impl ComponentFactory for MySqlSourceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &ComponentSpecification) -> Result<()> {
        if let Some(ConfigurationValue::Literal(stream)) = spec.configuration.get("stream") {
            StreamId::try_new(stream.as_str().context("invalid MySQL stream")?)?;
        }
        if let Some(ConfigurationValue::Literal(settings)) = spec.configuration.get("settings") {
            let config: MySqlConfig = serde_json::from_value(settings.clone())?;
            config.validate()?;
            anyhow::ensure!(
                matches!(config.start, MySqlStartPosition::Snapshot { .. })
                    == spec
                        .dependencies
                        .get("snapshot")
                        .is_some_and(|ids| !ids.is_empty()),
                "MySQL snapshot mode requires its paired bootstrap resource"
            );
            anyhow::ensure!(
                spec.descriptor
                    == MySqlSource::describe(spec.descriptor.id().clone(), config.output)?,
                "MySQL output schema does not match settings"
            );
            anyhow::ensure!(
                config.replay.is_some()
                    == spec
                        .dependencies
                        .get("source_progress")
                        .is_some_and(|ids| !ids.is_empty()),
                "MySQL replay requires its actual consumer progress resource"
            );
        } else {
            anyhow::ensure!(
                spec.descriptor
                    == MySqlSource::describe(spec.descriptor.id().clone(), MySqlOutput::Changes)?
                    || spec.descriptor
                        == MySqlSource::describe(
                            spec.descriptor.id().clone(),
                            MySqlOutput::Transactions
                        )?,
                "invalid MySQL output schema"
            );
        }
        Ok(())
    }
    fn validate_scope(
        &self,
        graph_id: &str,
        spec: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<()> {
        self.validate_resources(spec, declarations, resources)?;
        for id in spec
            .dependencies
            .get("source_progress")
            .into_iter()
            .flatten()
        {
            if let Some(handle) = resources.get(id) {
                anyhow::ensure!(
                    handle.get::<QuerySourceProgressResource>()?.0.graph_id() == graph_id,
                    "MySQL progress belongs to another graph"
                );
            }
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        let create = || -> Result<_> {
            let progress = if context
                .specification
                .dependencies
                .contains_key("source_progress")
            {
                context
                    .resources::<QuerySourceProgressResource>("source_progress")?
                    .pop()
                    .map(|resource| resource.0.clone())
            } else {
                None
            };
            if let Some(progress) = &progress {
                anyhow::ensure!(
                    progress.graph_id() == context.graph_id.as_ref(),
                    "MySQL progress belongs to another graph"
                );
            }
            let stream = context
                .configuration()
                .get("stream")
                .and_then(serde_json::Value::as_str)
                .context("missing MySQL stream")?;
            let config: MySqlConfig = serde_json::from_value(
                context
                    .configuration()
                    .get("settings")
                    .context("missing MySQL settings")?
                    .clone(),
            )?;
            let snapshot = if context.specification.dependencies.contains_key("snapshot") {
                context.resources::<MySqlSnapshot>("snapshot")?.pop()
            } else {
                None
            };
            let source = if let Some(snapshot) = snapshot {
                MySqlSource::with_snapshot(
                    context.component_id.clone(),
                    StreamId::try_new(stream)?,
                    config,
                    progress.context("snapshot requires actual query progress")?,
                    snapshot,
                )?
            } else {
                MySqlSource::new(
                    context.component_id.clone(),
                    StreamId::try_new(stream)?,
                    config,
                    progress,
                )?
            };
            use drasi_lib::computation::v1::ComputationComponent;
            anyhow::ensure!(
                source.descriptor() == &context.specification.descriptor,
                "resolved MySQL schema does not match its declaration"
            );
            Ok(ConstructedComponent::source(Box::new(source)))
        };
        create().map_err(ComponentCreationError::terminal)
    }
}
