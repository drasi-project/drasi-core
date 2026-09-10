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

use std::{
    any::{Any, TypeId},
    collections::BTreeMap,
    sync::Arc,
};

use async_trait::async_trait;

use super::{Component, ComponentRole, GraphError, GraphResult};
use crate::computation::v1::{
    data::validate_identifier, ComponentDescriptor, ComponentGeneration, ComponentId, EnvelopeSink,
    EnvelopeSource, FailureDisposition, ResourceId, SinkCompletion, Transformer,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct PluginIdentity {
    pub id: Arc<str>,
    pub version: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ImplementationIdentity {
    pub name: Arc<str>,
    pub version: Arc<str>,
    pub plugin: Option<PluginIdentity>,
}

impl ImplementationIdentity {
    pub fn try_new(name: impl Into<Arc<str>>, version: impl Into<Arc<str>>) -> GraphResult<Self> {
        let value = Self {
            name: name.into(),
            version: version.into(),
            plugin: None,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> GraphResult<()> {
        validate_identifier("implementation", &self.name)?;
        validate_identifier("implementation version", &self.version)?;
        if let Some(plugin) = &self.plugin {
            validate_identifier("plugin", &plugin.id)?;
            validate_identifier("plugin version", &plugin.version)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ConfigurationValue {
    Literal(serde_json::Value),
    Reference {
        resource: ResourceId,
        key: Arc<str>,
        secret: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationType {
    Boolean,
    Integer,
    String,
    Object,
    Array,
    Json,
}

impl ConfigurationType {
    fn accepts(self, value: &serde_json::Value) -> bool {
        match self {
            Self::Boolean => value.is_boolean(),
            Self::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
            Self::String => value.is_string(),
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
            Self::Json => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigurationField {
    pub value_type: ConfigurationType,
    pub required: bool,
    pub secret: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ConfigurationSchema {
    pub fields: BTreeMap<Arc<str>, ConfigurationField>,
    pub allow_additional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResourceRole {
    Bootstrap,
    Middleware,
    Inspection,
    Identity,
    IndexBackend,
    SecretStore,
    StateStore,
    Wal,
    Pipe,
    Checkpoint,
    Outbox,
    LiveResults,
    FutureQueue,
    LegacySource,
    SourceSubscription,
    QueryCatalog,
    LegacyReaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ResourceOwnership {
    Graph,
    Borrowed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceSpecification {
    pub id: ResourceId,
    pub role: ResourceRole,
    pub ownership: ResourceOwnership,
    /// An unresolved external binding, never a live handle or credentials.
    pub binding: Arc<str>,
}

#[async_trait]
pub trait ResourceCleanup: Send + Sync {
    async fn shutdown(&self) -> anyhow::Result<()>;
}

/// An actual constructed resource, held separately from desired specifications.
/// Catalogue requirements can validate its concrete Rust type during preflight.
#[derive(Clone)]
pub struct ResourceHandle {
    role: ResourceRole,
    value: Arc<dyn Any + Send + Sync>,
    cleanup: Option<Arc<dyn ResourceCleanup>>,
}

impl ResourceHandle {
    pub fn new<T: Any + Send + Sync>(role: ResourceRole, value: Arc<T>) -> Self {
        Self {
            role,
            value,
            cleanup: None,
        }
    }

    pub fn with_cleanup(mut self, cleanup: Arc<dyn ResourceCleanup>) -> Self {
        self.cleanup = Some(cleanup);
        self
    }

    pub fn role(&self) -> ResourceRole {
        self.role
    }

    pub fn has_cleanup(&self) -> bool {
        self.cleanup.is_some()
    }

    pub(super) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.value, &other.value)
    }

    pub fn get<T: Any + Send + Sync>(&self) -> GraphResult<Arc<T>> {
        self.value
            .clone()
            .downcast()
            .map_err(|_| GraphError::Topology {
                reason: "constructed resource has an incompatible concrete interface".into(),
            })
    }

    pub(super) async fn shutdown(&self) -> anyhow::Result<()> {
        if let Some(cleanup) = &self.cleanup {
            cleanup.shutdown().await?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ResourceRequirement {
    pub role: ResourceRole,
    pub minimum: usize,
    pub maximum: Option<usize>,
    pub concrete_type: Option<TypeId>,
}

impl ResourceRequirement {
    pub fn exactly_one<T: Any>(role: ResourceRole) -> Self {
        Self {
            role,
            minimum: 1,
            maximum: Some(1),
            concrete_type: Some(TypeId::of::<T>()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FactoryDescriptor {
    pub implementation: ImplementationIdentity,
    pub role: ComponentRole,
    pub configuration_version: u32,
    pub configuration: ConfigurationSchema,
    pub dependencies: BTreeMap<Arc<str>, ResourceRequirement>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ComponentSpecification {
    pub descriptor: ComponentDescriptor,
    pub role: ComponentRole,
    pub completion: Option<SinkCompletion>,
    pub implementation: ImplementationIdentity,
    pub configuration_version: u32,
    pub configuration: BTreeMap<Arc<str>, ConfigurationValue>,
    pub dependencies: BTreeMap<Arc<str>, Vec<ResourceId>>,
}

#[derive(Debug, thiserror::Error)]
#[error("{source}")]
pub struct ComponentCreationError {
    pub disposition: FailureDisposition,
    #[source]
    pub source: anyhow::Error,
}

impl ComponentCreationError {
    pub fn retryable(source: impl Into<anyhow::Error>) -> Self {
        Self {
            disposition: FailureDisposition::Retryable,
            source: source.into(),
        }
    }

    pub fn terminal(source: impl Into<anyhow::Error>) -> Self {
        Self {
            disposition: FailureDisposition::Terminal,
            source: source.into(),
        }
    }
}

pub struct ConstructedComponent(pub(super) Component);

impl ConstructedComponent {
    pub fn source(source: Box<dyn EnvelopeSource>) -> Self {
        Self(Component::Source(source))
    }
    pub fn transformer(transformer: Box<dyn Transformer>) -> Self {
        Self(Component::Transformer(transformer))
    }
    pub fn query(query: Box<dyn Transformer>) -> Self {
        Self(Component::Query(query))
    }
    pub fn sink(sink: Box<dyn EnvelopeSink>) -> Self {
        Self(Component::Sink(sink))
    }
}

/// Construction is distinct from activation. Implementations must be cancellation
/// safe and must not start data processing from `create`.
#[async_trait]
pub trait ComponentFactory: Send + Sync {
    fn descriptor(&self) -> &FactoryDescriptor;
    fn supports_reconfiguration(&self) -> bool {
        false
    }
    /// Additional pure definition validation, after generic schema/dependency checks.
    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()>;
    fn validate_resources(
        &self,
        specification: &ComponentSpecification,
        _declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        _resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(specification)
    }
    fn validate_scope(
        &self,
        _graph_id: &str,
        specification: &ComponentSpecification,
        declarations: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate_resources(specification, declarations, resources)
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> Result<ConstructedComponent, ComponentCreationError>;
}

#[async_trait]
pub trait ConfigurationResolver: Send + Sync {
    /// Pure reference validation; do not fetch external values here.
    fn validate_reference(&self, key: &str) -> anyhow::Result<()>;
    async fn resolve(&self, key: &str) -> anyhow::Result<serde_json::Value>;
}

pub struct ConfigurationResolverResource(pub Arc<dyn ConfigurationResolver>);

/// Runtime-only context: resolved values and actual resource instances cannot be
/// obtained by serializing the desired specification.
pub struct ConstructionContext {
    pub instance_id: Arc<str>,
    pub graph_id: Arc<str>,
    pub component_id: ComponentId,
    pub generation: ComponentGeneration,
    pub specification: Arc<ComponentSpecification>,
    configuration: BTreeMap<Arc<str>, serde_json::Value>,
    resources: BTreeMap<ResourceId, ResourceHandle>,
}

impl ConstructionContext {
    pub fn configuration(&self) -> &BTreeMap<Arc<str>, serde_json::Value> {
        &self.configuration
    }

    pub fn resources<T: Any + Send + Sync>(&self, dependency: &str) -> GraphResult<Vec<Arc<T>>> {
        let ids = self
            .specification
            .dependencies
            .get(dependency)
            .ok_or_else(|| GraphError::Topology {
                reason: format!("undeclared construction dependency {dependency}"),
            })?;
        ids.iter()
            .map(|id| {
                self.resources
                    .get(id)
                    .ok_or_else(|| GraphError::Topology {
                        reason: format!("resource {id} is not realized"),
                    })?
                    .get()
            })
            .collect()
    }

    pub(super) async fn resolve(
        instance_id: Arc<str>,
        graph_id: Arc<str>,
        generation: ComponentGeneration,
        specification: Arc<ComponentSpecification>,
        resources: BTreeMap<ResourceId, ResourceHandle>,
        schema: &ConfigurationSchema,
    ) -> Result<Self, ComponentCreationError> {
        let mut configuration = BTreeMap::new();
        for (key, value) in &specification.configuration {
            let value = match value {
                ConfigurationValue::Literal(value) => value.clone(),
                ConfigurationValue::Reference { resource, key, .. } => {
                    let resolver = resources
                        .get(resource)
                        .ok_or_else(|| {
                            ComponentCreationError::retryable(anyhow::anyhow!(
                                "resource {resource} is not realized"
                            ))
                        })?
                        .get::<ConfigurationResolverResource>()
                        .map_err(ComponentCreationError::terminal)?;
                    resolver
                        .0
                        .resolve(key)
                        .await
                        .map_err(ComponentCreationError::retryable)?
                }
            };
            if schema
                .fields
                .get(key)
                .is_some_and(|field| !field.value_type.accepts(&value))
            {
                return Err(ComponentCreationError::terminal(anyhow::anyhow!(
                    "resolved configuration field {key} has an incompatible type"
                )));
            }
            configuration.insert(key.clone(), value);
        }
        Ok(Self {
            instance_id,
            graph_id,
            component_id: specification.descriptor.id().clone(),
            generation,
            specification,
            configuration,
            resources,
        })
    }
}

pub(super) fn validate_specification(
    graph_id: &str,
    spec: &ComponentSpecification,
    factory: &dyn ComponentFactory,
    declarations: &BTreeMap<ResourceId, ResourceSpecification>,
    resources: &BTreeMap<ResourceId, ResourceHandle>,
) -> GraphResult<()> {
    spec.implementation.validate()?;
    if (spec.role == ComponentRole::Sink) != spec.completion.is_some() {
        return Err(super::topology(
            "only sink specifications declare a required completion boundary",
        ));
    }
    let descriptor = factory.descriptor();
    if descriptor.implementation != spec.implementation
        || descriptor.role != spec.role
        || descriptor.configuration_version != spec.configuration_version
        || spec.configuration_version == 0
    {
        return Err(super::topology(
            "factory implementation, role, plugin, or configuration version mismatch",
        ));
    }
    for (key, field) in &descriptor.configuration.fields {
        if field.required && !spec.configuration.contains_key(key) {
            return Err(super::topology(format!(
                "missing configuration field {key}"
            )));
        }
    }
    for (key, value) in &spec.configuration {
        validate_identifier("configuration field", key)?;
        let field = descriptor.configuration.fields.get(key);
        if field.is_none() && !descriptor.configuration.allow_additional {
            return Err(super::topology(format!(
                "unknown configuration field {key}"
            )));
        }
        match value {
            ConfigurationValue::Literal(value) => {
                if field.is_some_and(|field| field.secret || !field.value_type.accepts(value)) {
                    return Err(super::topology(format!("configuration field {key} requires a compatible value or unresolved secret reference")));
                }
            }
            ConfigurationValue::Reference {
                resource,
                key: reference,
                secret,
            } => {
                validate_identifier("configuration reference", reference)?;
                let declaration = declarations.get(resource).ok_or_else(|| {
                    super::topology(format!("unknown configuration resource {resource}"))
                })?;
                if *secret && declaration.role != ResourceRole::SecretStore {
                    return Err(super::topology(
                        "secret references require a secret-store resource",
                    ));
                }
                if field.is_some_and(|field| field.secret) && !secret {
                    return Err(super::topology("secret fields require secret references"));
                }
                if let Some(handle) = resources.get(resource) {
                    handle
                        .get::<ConfigurationResolverResource>()?
                        .0
                        .validate_reference(reference)
                        .map_err(|error| {
                            super::topology(format!("invalid configuration reference: {error:#}"))
                        })?;
                }
            }
        }
    }
    for (slot, requirement) in &descriptor.dependencies {
        validate_identifier("dependency slot", slot)?;
        let dependencies = spec
            .dependencies
            .get(slot)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if dependencies.len() < requirement.minimum
            || requirement.maximum.is_some_and(|maximum| {
                maximum < requirement.minimum || dependencies.len() > maximum
            })
        {
            return Err(super::topology(format!(
                "dependency cardinality mismatch for {slot}"
            )));
        }
        let mut unique = std::collections::BTreeSet::new();
        for id in dependencies {
            let declaration = declarations
                .get(id)
                .ok_or_else(|| super::topology(format!("unknown resource {id}")))?;
            if !unique.insert(id) || declaration.role != requirement.role {
                return Err(super::topology(format!(
                    "duplicate or incompatible dependency {slot}"
                )));
            }
            if let Some(handle) = resources.get(id) {
                if requirement
                    .concrete_type
                    .is_some_and(|expected| expected != handle.value.as_ref().type_id())
                {
                    return Err(super::topology(format!(
                        "resource {id} has an incompatible concrete interface"
                    )));
                }
            }
        }
    }
    if spec
        .dependencies
        .keys()
        .any(|slot| !descriptor.dependencies.contains_key(slot))
    {
        return Err(super::topology("undeclared factory dependency slot"));
    }
    factory
        .validate_scope(graph_id, spec, declarations, resources)
        .map_err(|error| super::topology(format!("invalid component definition: {error:#}")))
}
