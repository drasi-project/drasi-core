// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use std::collections::BTreeMap;

pub struct SqliteSourceFactory {
    descriptor: FactoryDescriptor,
}

impl Default for SqliteSourceFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("drasi/sqlite-native", "1")
                    .expect("implementation"),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([
                        (
                            Arc::from("coordinated_snapshot"),
                            ConfigurationField {
                                value_type: ConfigurationType::Boolean,
                                required: false,
                                secret: false,
                            },
                        ),
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
                                secret: false,
                            },
                        ),
                    ]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([
                    (
                        Arc::from("snapshot"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<SqliteSnapshot>(
                                ResourceRole::Bootstrap,
                            )
                        },
                    ),
                    (
                        Arc::from("client"),
                        ResourceRequirement::exactly_one::<SqliteClientResource>(
                            ResourceRole::Component,
                        ),
                    ),
                    (
                        Arc::from("source_progress"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<QuerySourceProgressResource>(
                                ResourceRole::Checkpoint,
                            )
                        },
                    ),
                ]),
            },
        }
    }
}

#[async_trait]
impl ComponentFactory for SqliteSourceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, specification: &ComponentSpecification) -> Result<()> {
        let coordinated = match specification.configuration.get("coordinated_snapshot") {
            Some(ConfigurationValue::Reference { .. }) => None,
            Some(ConfigurationValue::Literal(value)) => Some(
                value
                    .as_bool()
                    .context("invalid SQLite coordinated snapshot flag")?,
            ),
            None => Some(false),
        };
        if let Some(coordinated) = coordinated {
            anyhow::ensure!(
                coordinated
                    == specification
                        .dependencies
                        .get("snapshot")
                        .is_some_and(|ids| !ids.is_empty()),
                "coordinated SQLite mode requires its paired snapshot resource"
            );
            anyhow::ensure!(
                !coordinated
                    || specification
                        .dependencies
                        .get("source_progress")
                        .is_some_and(|ids| !ids.is_empty()),
                "coordinated SQLite initialization requires persistent source progress"
            );
        }
        if let Some(ConfigurationValue::Literal(stream)) = specification.configuration.get("stream")
        {
            StreamId::try_new(stream.as_str().context("invalid native SQLite stream")?)?;
        }
        if let Some(ConfigurationValue::Literal(settings)) =
            specification.configuration.get("settings")
        {
            let config: SqliteConfig = serde_json::from_value(settings.clone())?;
            config.validate()?;
            anyhow::ensure!(
                specification.descriptor
                    == SqliteSource::describe(
                        specification.descriptor.id().clone(),
                        config.output
                    )?,
                "native SQLite output schema does not match settings"
            );
            anyhow::ensure!(
                config.replay.is_some()
                    == specification
                        .dependencies
                        .get("source_progress")
                        .is_some_and(|resources| !resources.is_empty()),
                "native SQLite replay settings and progress resource must be configured together"
            );
        } else {
            let descriptor = &specification.descriptor;
            anyhow::ensure!(
                descriptor
                    == &SqliteSource::describe(descriptor.id().clone(), SqliteOutput::Changes)?
                    || descriptor
                        == &SqliteSource::describe(
                            descriptor.id().clone(),
                            SqliteOutput::Transactions
                        )?,
                "invalid native SQLite output schema"
            );
        }
        Ok(())
    }
    fn validate_scope(
        &self,
        graph_id: &str,
        specification: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<()> {
        self.validate_resources(specification, declarations, resources)?;
        for resource in specification
            .dependencies
            .get("source_progress")
            .into_iter()
            .flatten()
        {
            if let Some(handle) = resources.get(resource) {
                anyhow::ensure!(
                    handle.get::<QuerySourceProgressResource>()?.0.graph_id() == graph_id,
                    "native SQLite progress belongs to another graph"
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
            let stream = context
                .configuration()
                .get("stream")
                .and_then(serde_json::Value::as_str)
                .context("missing native SQLite stream")?;
            let config: SqliteConfig = serde_json::from_value(
                context
                    .configuration()
                    .get("settings")
                    .context("missing native SQLite settings")?
                    .clone(),
            )?;
            let progress = if context
                .specification
                .dependencies
                .contains_key("source_progress")
            {
                context
                    .resources::<QuerySourceProgressResource>("source_progress")?
                    .pop()
            } else {
                None
            };
            anyhow::ensure!(
                config.replay.is_some() == progress.is_some(),
                "native SQLite replay requires its actual consumer progress"
            );
            let mut source = match progress {
                Some(progress) => {
                    anyhow::ensure!(
                        progress.0.graph_id() == context.graph_id.as_ref(),
                        "native SQLite progress belongs to another graph"
                    );
                    SqliteSource::new_replayable(
                        context.component_id.clone(),
                        StreamId::try_new(stream)?,
                        config,
                        progress.0.clone(),
                    )?
                }
                None => SqliteSource::new(
                    context.component_id.clone(),
                    StreamId::try_new(stream)?,
                    config,
                )?,
            };
            let coordinated = context
                .configuration()
                .get("coordinated_snapshot")
                .map(|value| {
                    value
                        .as_bool()
                        .context("invalid SQLite coordinated snapshot flag")
                })
                .transpose()?
                .unwrap_or(false);
            let snapshot = if context.specification.dependencies.contains_key("snapshot") {
                context.resources::<SqliteSnapshot>("snapshot")?.pop()
            } else {
                None
            };
            anyhow::ensure!(
                coordinated == snapshot.is_some(),
                "SQLite snapshot configuration/resource mismatch"
            );
            if let Some(snapshot) = snapshot {
                snapshot.validate_source(&source)?;
                source.snapshot = Some(snapshot);
            }
            let client = context
                .resources::<SqliteClientResource>("client")?
                .pop()
                .context("missing native SQLite client resource")?;
            let source = source.with_client_resource(client)?;
            anyhow::ensure!(
                source.descriptor() == &context.specification.descriptor,
                "native SQLite resolved output schema mismatch"
            );
            Ok(ConstructedComponent::source(Box::new(source)))
        };
        create().map_err(ComponentCreationError::terminal)
    }
}
