// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

const IMPLEMENTATION: &str = "drasi/source-time-merge";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recipe {
    definition: SourceTimeMergeDefinition,
    #[serde(default)]
    persistent: bool,
}

impl SourceTimeMergeDefinition {
    /// Describe the standard graph-change merger without acquiring resources.
    ///
    /// `storage` selects a graph-owned [`SourceTimeMergeStorage`] dependency.
    /// Omit it for volatile operation. Bind `out` and, when configured, `late`
    /// to this definition's streams; connect exactly the declared input streams
    /// to `in`. Custom executable schemas still use the object-based constructor.
    pub fn specification(
        &self,
        storage: Option<ResourceId>,
    ) -> anyhow::Result<ComponentSpecification> {
        let persistent = storage.is_some();
        Ok(ComponentSpecification {
            descriptor: self.descriptor(&GraphChangeCodec::schema(), persistent)?,
            role: ComponentRole::Transformer,
            completion: None,
            implementation: ImplementationIdentity::try_new(IMPLEMENTATION, "1")?,
            configuration_version: 1,
            configuration: BTreeMap::from([
                (
                    Arc::from("definition"),
                    ConfigurationValue::Literal(serde_json::to_value(self)?),
                ),
                (
                    Arc::from("persistent"),
                    ConfigurationValue::Literal(persistent.into()),
                ),
            ]),
            dependencies: storage
                .map(|id| BTreeMap::from([(Arc::from("storage"), vec![id])]))
                .unwrap_or_default(),
        })
    }
}

/// Reconstructible graph-change source-time merging, including routed late
/// output and independent persistent storage. Registered by
/// [`FactoryRegistry::standard`].
pub struct SourceTimeMergeFactory {
    descriptor: FactoryDescriptor,
}

impl Default for SourceTimeMergeFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(IMPLEMENTATION, "1")
                    .expect("fixed time merge identity"),
                role: ComponentRole::Transformer,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([
                        (
                            Arc::from("definition"),
                            ConfigurationField {
                                value_type: ConfigurationType::Object,
                                required: true,
                                secret: false,
                            },
                        ),
                        (
                            Arc::from("persistent"),
                            ConfigurationField {
                                value_type: ConfigurationType::Boolean,
                                required: false,
                                secret: false,
                            },
                        ),
                    ]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::from([(
                    Arc::from("storage"),
                    ResourceRequirement {
                        minimum: 0,
                        ..ResourceRequirement::exactly_one::<SourceTimeMergeStorage>(
                            ResourceRole::IndexBackend,
                        )
                    },
                )]),
            },
        }
    }
}

fn recipe(specification: &ComponentSpecification) -> anyhow::Result<Recipe> {
    let values = specification
        .configuration
        .iter()
        .map(|(name, value)| match value {
            ConfigurationValue::Literal(value) => Ok((name.to_string(), value.clone())),
            ConfigurationValue::Reference { .. } => {
                anyhow::bail!("time merge port and storage settings must be literal")
            }
        })
        .collect::<anyhow::Result<serde_json::Map<_, _>>>()?;
    Ok(serde_json::from_value(serde_json::Value::Object(values))?)
}

#[async_trait]
impl ComponentFactory for SourceTimeMergeFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn record_schemas(&self) -> Vec<Arc<Schema>> {
        vec![GraphChangeCodec::schema()]
    }

    fn primary_ports(&self) -> PortPrimaries {
        PortPrimaries::new(Some(port("in")), Some(port("out")))
    }

    fn binds_inputs(&self) -> bool {
        true
    }

    fn specification_from_config(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        let recipe: Recipe = serde_json::from_value(configuration)?;
        anyhow::ensure!(
            recipe.definition.id == id,
            "time merge definition belongs to another component"
        );
        let mut specification = recipe.definition.specification(None)?;
        specification.descriptor = recipe
            .definition
            .descriptor(&GraphChangeCodec::schema(), recipe.persistent)?;
        specification.configuration.insert(
            Arc::from("persistent"),
            ConfigurationValue::Literal(recipe.persistent.into()),
        );
        Ok(specification)
    }

    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        let recipe = recipe(specification)?;
        anyhow::ensure!(
            specification.role == ComponentRole::Transformer
                && specification.completion.is_none()
                && specification.descriptor
                    == recipe
                        .definition
                        .descriptor(&GraphChangeCodec::schema(), recipe.persistent)?,
            "time merge descriptor differs from its definition"
        );
        let storage = specification
            .dependencies
            .get("storage")
            .map_or(0, Vec::len);
        anyhow::ensure!(
            storage == usize::from(recipe.persistent),
            "persistent time merge requires exactly one storage owner; volatile mode takes none"
        );
        Ok(())
    }

    fn validate_scope(
        &self,
        graph: &str,
        specification: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(specification)?;
        anyhow::ensure!(
            recipe(specification)?.definition.graph_id == graph,
            "time merge definition belongs to another graph"
        );
        for id in specification
            .dependencies
            .get("storage")
            .into_iter()
            .flatten()
        {
            if let Some(declaration) = declarations.get(id) {
                anyhow::ensure!(
                    declaration.ownership == ResourceOwnership::Graph,
                    "time merge storage requires graph cleanup ownership"
                );
            }
            if let Some(handle) = resources.get(id) {
                anyhow::ensure!(
                    handle.has_cleanup(),
                    "time merge storage has no cleanup owner"
                );
                handle.get::<SourceTimeMergeStorage>()?.validate_owner(
                    None,
                    graph,
                    specification.descriptor.id(),
                )?;
            }
        }
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let recipe = recipe(&context.specification).map_err(ComponentCreationError::terminal)?;
        let inputs: BTreeSet<_> = context
            .input_bindings()
            .iter()
            .map(|binding| binding.stream.clone())
            .collect();
        if inputs != recipe.definition.sources.iter().cloned().collect()
            || context
                .input_bindings()
                .iter()
                .any(|binding| binding.input_port != port("in"))
        {
            return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                "time merge requires exactly its declared source streams connected to in"
            )));
        }
        let mut merger = SourceTimeMergeTransformer::construct(
            recipe.definition,
            GraphChangeCodec::schema(),
            recipe.persistent,
        )
        .map_err(ComponentCreationError::terminal)?;
        merger
            .set_construction_scope(context.instance_id.clone())
            .map_err(ComponentCreationError::terminal)?;
        if recipe.persistent {
            let storage = context
                .resources::<SourceTimeMergeStorage>("storage")
                .map_err(ComponentCreationError::terminal)?
                .pop()
                .ok_or_else(|| {
                    ComponentCreationError::terminal(anyhow::anyhow!(
                        "time merge storage is unavailable"
                    ))
                })?;
            storage
                .validate_owner(
                    Some(&context.instance_id),
                    &context.graph_id,
                    &context.component_id,
                )
                .map_err(ComponentCreationError::terminal)?;
            let (transaction, lease) = storage
                .open()
                .await
                .map_err(ComponentCreationError::retryable)?;
            merger
                .bindings
                .bind_standalone_owner(storage.provider(), Arc::downgrade(&transaction))
                .map_err(ComponentCreationError::terminal)?;
            merger.transaction = Some(transaction);
            merger._storage_lease = Some(lease);
            merger.publish();
        }
        Ok(ConstructedComponent::transformer(Box::new(merger)))
    }
}
