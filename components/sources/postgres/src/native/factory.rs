// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;
use drasi_lib::computation::v1::{
    ComponentCreationError, ComponentFactory, ComponentRole, ComponentSpecification,
    ConfigurationField, ConfigurationSchema, ConfigurationType, ConfigurationValue,
    ConstructedComponent, ConstructionContext, FactoryDescriptor, ImplementationIdentity,
    QuerySourceProgressResource, ResourceHandle, ResourceId, ResourceRequirement, ResourceRole,
    ResourceSpecification,
};

pub struct PostgresTransactionSourceFactory {
    descriptor: FactoryDescriptor,
}

impl Default for PostgresTransactionSourceFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("drasi/postgres-transactions", "1")
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
                            Arc::from("coordinated_snapshot"),
                            ConfigurationField {
                                value_type: ConfigurationType::Boolean,
                                required: false,
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
                        ResourceRequirement::exactly_one::<QuerySourceProgressResource>(
                            ResourceRole::Checkpoint,
                        ),
                    ),
                    (
                        Arc::from("snapshot"),
                        ResourceRequirement {
                            minimum: 0,
                            ..ResourceRequirement::exactly_one::<PostgresSnapshot>(
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
impl ComponentFactory for PostgresTransactionSourceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
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
                    "PostgreSQL progress resource belongs to another graph"
                );
            }
        }
        Ok(())
    }

    fn validate(&self, spec: &ComponentSpecification) -> Result<()> {
        anyhow::ensure!(
            spec.descriptor == PostgresTransactionSource::describe(spec.descriptor.id().clone())?,
            "invalid native PostgreSQL ports/schema"
        );
        if let Some(ConfigurationValue::Literal(settings)) = spec.configuration.get("settings") {
            serde_json::from_value::<PostgresTransactionConfig>(settings.clone())?.validate()?;
        }
        if let Some(ConfigurationValue::Literal(stream)) = spec.configuration.get("stream") {
            StreamId::try_new(stream.as_str().context("invalid PostgreSQL stream")?)?;
        }
        let coordinated = match spec.configuration.get("coordinated_snapshot") {
            Some(ConfigurationValue::Reference { .. }) => None,
            Some(ConfigurationValue::Literal(value)) => Some(
                value
                    .as_bool()
                    .context("invalid coordinated snapshot flag")?,
            ),
            None => Some(false),
        };
        if let Some(coordinated) = coordinated {
            anyhow::ensure!(
                coordinated
                    == spec
                        .dependencies
                        .get("snapshot")
                        .is_some_and(|ids| !ids.is_empty()),
                "coordinated PostgreSQL mode requires its paired snapshot resource"
            );
        }
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError> {
        let create = || -> Result<_> {
            let progress = context
                .resources::<QuerySourceProgressResource>("source_progress")?
                .pop()
                .context("missing PostgreSQL committed progress resource")?;
            anyhow::ensure!(
                progress.0.graph_id() == context.graph_id.as_ref(),
                "PostgreSQL progress resource belongs to another graph"
            );
            let stream = context
                .configuration()
                .get("stream")
                .and_then(serde_json::Value::as_str)
                .context("missing PostgreSQL stream")?;
            let settings = context
                .configuration()
                .get("settings")
                .context("missing PostgreSQL settings")?;
            let mut source = PostgresTransactionSource::new(
                context.component_id.clone(),
                StreamId::try_new(stream)?,
                serde_json::from_value(settings.clone())?,
                progress.0.clone(),
            )?;
            let snapshot = if context.specification.dependencies.contains_key("snapshot") {
                context.resources::<PostgresSnapshot>("snapshot")?.pop()
            } else {
                None
            };
            let coordinated = context
                .configuration()
                .get("coordinated_snapshot")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            anyhow::ensure!(
                coordinated == snapshot.is_some(),
                "PostgreSQL snapshot configuration/resource mismatch"
            );
            if let Some(snapshot) = snapshot {
                snapshot.validate_source(&source)?;
                source.snapshot = Some(snapshot);
            }
            Ok(ConstructedComponent::source(Box::new(source)))
        };
        create().map_err(ComponentCreationError::terminal)
    }
}
