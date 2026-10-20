// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use drasi_lib::computation::v1::{
    ComponentCreationError, ComponentFactory, ComponentRole, ComponentSpecification,
    ConfigurationField, ConfigurationSchema, ConfigurationType, ConfigurationValue,
    ConstructedComponent, ConstructionContext, ConsumerProgressSet, FactoryDescriptor,
    ImplementationIdentity, QuerySourceProgressResource, ResourceHandle, ResourceId,
    ResourceRequirement, ResourceRole, ResourceSpecification, StreamId,
    MAX_CONSUMER_PROGRESS_MEMBERS,
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
                        Arc::from("consumer_progress"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<ConsumerProgressSet>(
                                ResourceRole::Checkpoint,
                            )
                        },
                    ),
                    (
                        Arc::from("consumer_snapshots"),
                        ResourceRequirement {
                            minimum: 0,
                            maximum: Some(MAX_CONSUMER_PROGRESS_MEMBERS),
                            ..ResourceRequirement::exactly_one::<MySqlSnapshot>(
                                ResourceRole::Bootstrap,
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
        let has = |slot: &str| {
            spec.dependencies
                .get(slot)
                .is_some_and(|ids| !ids.is_empty())
        };
        anyhow::ensure!(
            !(has("source_progress") && has("consumer_progress")),
            "MySQL cannot bind both single-owner and consumer-set progress"
        );
        anyhow::ensure!(
            !has("snapshot") || (has("source_progress") && !has("consumer_progress")),
            "MySQL single-owner snapshot requires its single-owner progress binding"
        );
        anyhow::ensure!(
            !has("consumer_snapshots") || (has("consumer_progress") && !has("snapshot")),
            "MySQL consumer snapshots require the actual consumer set"
        );
        if let Some(ConfigurationValue::Literal(stream)) = spec.configuration.get("stream") {
            StreamId::try_new(stream.as_str().context("invalid MySQL stream")?)?;
        }
        if let Some(ConfigurationValue::Literal(settings)) = spec.configuration.get("settings") {
            let config: MySqlConfig = serde_json::from_value(settings.clone())?;
            config.validate()?;
            anyhow::ensure!(
                matches!(config.start, MySqlStartPosition::Snapshot { .. })
                    == (has("snapshot") || has("consumer_snapshots")),
                "MySQL snapshot mode requires its paired bootstrap resource"
            );
            anyhow::ensure!(
                spec.descriptor
                    == MySqlSource::describe(spec.descriptor.id().clone(), config.output)?,
                "MySQL output schema does not match settings"
            );
            anyhow::ensure!(
                config.replay.is_some() == (has("source_progress") || has("consumer_progress")),
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
            for id in spec
                .dependencies
                .get("consumer_progress")
                .into_iter()
                .flatten()
            {
                if let Some(handle) = resources.get(id) {
                    let consumers = handle.get::<ConsumerProgressSet>()?;
                    anyhow::ensure!(
                        consumers.graph_id() == graph_id
                            && consumers.producer_id() == spec.descriptor.id(),
                        "MySQL consumer progress belongs to another graph or producer"
                    );
                }
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
            let consumers = if context
                .specification
                .dependencies
                .contains_key("consumer_progress")
            {
                context
                    .resources::<ConsumerProgressSet>("consumer_progress")?
                    .pop()
            } else {
                None
            };
            anyhow::ensure!(
                !(progress.is_some() && consumers.is_some()),
                "MySQL cannot bind both single-owner and consumer-set progress"
            );
            if let Some(consumers) = &consumers {
                anyhow::ensure!(
                    consumers.graph_id() == context.graph_id.as_ref()
                        && consumers.producer_id() == &context.component_id,
                    "MySQL consumer progress belongs to another graph or producer"
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
            let consumer_snapshots = if context
                .specification
                .dependencies
                .contains_key("consumer_snapshots")
            {
                context.resources::<MySqlSnapshot>("consumer_snapshots")?
            } else {
                Vec::new()
            };
            anyhow::ensure!(
                (consumer_snapshots.is_empty() || consumers.is_some())
                    && (snapshot.is_none() || consumers.is_none()),
                "MySQL snapshot and progress binding modes differ"
            );
            let source = if let Some(consumers) = consumers {
                if matches!(config.start, MySqlStartPosition::Snapshot { .. })
                    || !consumer_snapshots.is_empty()
                {
                    MySqlSource::with_consumer_snapshots(
                        context.component_id.clone(),
                        StreamId::try_new(stream)?,
                        config,
                        consumers,
                        consumer_snapshots,
                    )?
                } else {
                    MySqlSource::new_with_consumer_progress(
                        context.component_id.clone(),
                        StreamId::try_new(stream)?,
                        config,
                        consumers,
                    )?
                }
            } else if let Some(snapshot) = snapshot {
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
