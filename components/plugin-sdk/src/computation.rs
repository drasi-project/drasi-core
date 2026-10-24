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

//! Optional host-side factories over existing plugin descriptors. No plugin
//! vtable, registration ABI, runtime trait or configuration encoding changes.
//! Descriptors supplied here must already satisfy the host's loading/version
//! checks and retain their normal code-unit lifetime. In-process descriptors use
//! task-scoped instance secret resolution. An FFI descriptor keeps its existing
//! host-injected resolver; task-local state does not cross a plugin's executor.

use crate::descriptor::{
    BootstrapPluginDescriptor, IdentityProviderPluginDescriptor, IndexBackendPluginDescriptor,
    ReactionPluginDescriptor, SecretStorePluginDescriptor, SourcePluginDescriptor,
};
use anyhow::Context;
use async_trait::async_trait;
use drasi_lib::computation::v1::{
    LegacyIndexProviderAdapter, LegacyPluginServices, QueryResultsCatalog,
    ReactionPluginConstructor, ReactionPluginHost, ReactionPluginOptions, SourcePluginConstructor,
    SourcePluginHost,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

tokio::task_local! {
    static SECRET_SCOPE: Arc<dyn crate::resolver::ValueResolver>;
}

pub(crate) fn scoped_secret_resolver() -> Option<Arc<dyn crate::resolver::ValueResolver>> {
    SECRET_SCOPE.try_with(Arc::clone).ok()
}

struct InstanceSecrets(Arc<dyn drasi_lib::secret_store::SecretStoreProvider>);
#[async_trait]
impl crate::resolver::ValueResolver for InstanceSecrets {
    async fn resolve_to_string(
        &self,
        value: &crate::ConfigValue<String>,
    ) -> Result<String, crate::resolver::ResolverError> {
        let crate::ConfigValue::Secret { name } = value else {
            return Err(crate::resolver::ResolverError::WrongResolverType);
        };
        self.0.get_secret(name).await.map_err(|_| {
            crate::resolver::ResolverError::SecretResolutionFailed(
                "the computation instance's secret store could not resolve the reference".into(),
            )
        })
    }
}

/// A caller-polled resolver scope, never a mutation of the SDK's global resolver.
/// It applies to in-process descriptor creation and is not inherited by spawned
/// tasks or a separately loaded plugin runtime.
#[derive(Clone)]
pub struct PluginResolution(Arc<dyn crate::resolver::ValueResolver>);
impl PluginResolution {
    pub fn from_services(services: &LegacyPluginServices) -> Self {
        Self(match &services.secrets {
            Some(secrets) => Arc::new(InstanceSecrets(secrets.clone())),
            None => Arc::new(crate::resolver::SecretResolver),
        })
    }
    pub async fn run<F: std::future::Future>(&self, operation: F) -> F::Output {
        SECRET_SCOPE.scope(self.0.clone(), operation).await
    }
}

/// A desired plugin recipe. Keep this original form for export; do not rebuild
/// it from Source::properties()/Reaction::properties(), which may contain secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfiguration {
    pub kind: String,
    pub config_version: String,
    pub config: Value,
}
impl PluginConfiguration {
    pub fn new(kind: impl Into<String>, config_version: impl Into<String>, config: Value) -> Self {
        Self {
            kind: kind.into(),
            config_version: config_version.into(),
            config,
        }
    }
    fn matches(&self, kind: &str, version: &str) -> anyhow::Result<()> {
        if self.kind != kind || self.config_version != version {
            anyhow::bail!(
                "plugin kind/configuration version does not match the supplied descriptor"
            );
        }
        Ok(())
    }
}

struct DescriptorSchema(jsonschema::JSONSchema);
impl DescriptorSchema {
    fn new(name: &str, schemas: &str) -> anyhow::Result<Self> {
        let mut schemas: Value =
            serde_json::from_str(schemas).context("invalid descriptor schema JSON")?;
        let map = schemas
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("descriptor must supply an OpenAPI schema map"))?;
        let root = if map.contains_key(name) {
            name.to_owned()
        } else {
            // Older descriptors name a qualified Config while utoipa exports
            // its unqualified ConfigDto. Accept only that unambiguous alias.
            let short = name.rsplit('.').next().unwrap_or(name);
            let dto = format!("{short}Dto");
            let matches: Vec<_> = map
                .keys()
                .filter(|key| key.as_str() == short || **key == dto)
                .collect();
            let [actual] = matches.as_slice() else {
                anyhow::bail!("descriptor root schema is absent or ambiguous");
            };
            log::debug!("Using legacy descriptor schema alias {name} -> {actual}");
            (*actual).clone()
        };
        // These are SDK wire types, not plugin-specific inferred schemas.
        for (name, primitive) in [
            // Some existing DTO schemas erase ConfigValue<T>'s T. Validate its
            // wire reference shape here; the descriptor validates T on creation.
            (
                "ConfigValue",
                json!({"not":{"type":"object","required":["kind"]}}),
            ),
            ("ConfigValueString", json!({"type":"string"})),
            (
                "ConfigValueU16",
                json!({"type":"integer","minimum":0,"maximum":65535}),
            ),
            (
                "ConfigValueU32",
                json!({"type":"integer","minimum":0,"maximum":4294967295u64}),
            ),
            ("ConfigValueU64", json!({"type":"integer","minimum":0})),
            ("ConfigValueUsize", json!({"type":"integer","minimum":0})),
            ("ConfigValueBool", json!({"type":"boolean"})),
        ] {
            map.insert(name.into(), json!({"anyOf":[primitive,
                {"type":"object","required":["kind","name"],"properties":{"kind":{"enum":["Secret"]},"name":{"type":"string","minLength":1}},"additionalProperties":false},
                {"type":"object","required":["kind","name"],"properties":{"kind":{"enum":["EnvironmentVariable"]},"name":{"type":"string","minLength":1},"default":{"type":"string"}},"additionalProperties":false},
                {"type":"string","pattern":"^\\$\\{[^}]+\\}$"}
            ]}));
        }
        let escaped = root.replace('~', "~0").replace('/', "~1");
        let mut schema = json!({"$ref":format!("#/components/schemas/{escaped}"),"components":{"schemas":schemas}});
        normalize_openapi(&mut schema)?;
        let compiled = jsonschema::JSONSchema::options()
            .with_draft(jsonschema::Draft::Draft7)
            .compile(&schema)
            .map_err(|error| {
                anyhow::anyhow!(
                    "invalid descriptor schema at {}: {:?}",
                    error.schema_path,
                    error.kind
                )
            })?;
        Ok(Self(compiled))
    }
    fn validate(&self, value: &Value) -> anyhow::Result<()> {
        if let Err(mut errors) = self.0.validate(value) {
            if let Some(error) = errors.next() {
                // Do not include resolved configuration values in diagnostics.
                anyhow::bail!(
                    "plugin configuration at {} violates schema {}",
                    error.instance_path,
                    error.schema_path
                );
            }
        }
        Ok(())
    }
}

fn normalize_openapi(schema: &mut Value) -> anyhow::Result<()> {
    match schema {
        Value::Object(object) => {
            if object
                .get("$ref")
                .and_then(Value::as_str)
                .is_some_and(|reference| !reference.starts_with("#/"))
            {
                anyhow::bail!("remote descriptor schema references are not supported");
            }
            for (exclusive, inclusive) in [
                ("exclusiveMinimum", "minimum"),
                ("exclusiveMaximum", "maximum"),
            ] {
                if let Some(flag) = object.get(exclusive).and_then(Value::as_bool) {
                    object.remove(exclusive);
                    if flag {
                        if let Some(bound) = object.remove(inclusive) {
                            object.insert(exclusive.into(), bound);
                        }
                    }
                }
            }
            if object.remove("nullable") == Some(Value::Bool(true)) {
                if let Some(Value::String(kind)) = object.get("type").cloned() {
                    object.insert("type".into(), json!([kind, "null"]));
                } else if let Some(reference) = object.remove("$ref") {
                    object.insert("anyOf".into(), json!([{"$ref":reference},{"type":"null"}]));
                }
            }
            for value in object.values_mut() {
                normalize_openapi(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_openapi(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub struct SourcePluginFactory {
    descriptor: Arc<dyn SourcePluginDescriptor>,
    id: String,
    recipe: PluginConfiguration,
    schema: DescriptorSchema,
    auto_start: bool,
}
impl SourcePluginFactory {
    pub fn new(
        descriptor: Arc<dyn SourcePluginDescriptor>,
        id: impl Into<String>,
        recipe: PluginConfiguration,
        auto_start: bool,
    ) -> anyhow::Result<Arc<Self>> {
        recipe.matches(descriptor.kind(), descriptor.config_version())?;
        let schema = DescriptorSchema::new(
            descriptor.config_schema_name(),
            &descriptor.config_schema_json(),
        )?;
        schema.validate(&recipe.config)?;
        let id = id.into();
        drasi_lib::computation::v1::ComponentId::try_new(id.as_str())?;
        Ok(Arc::new(Self {
            descriptor,
            id,
            recipe,
            schema,
            auto_start,
        }))
    }
    pub fn configuration(&self) -> &PluginConfiguration {
        &self.recipe
    }
    pub async fn host(
        self: &Arc<Self>,
        services: LegacyPluginServices,
    ) -> anyhow::Result<Arc<SourcePluginHost>> {
        SourcePluginHost::recreatable(
            Arc::new(ScopedSourceFactory {
                factory: self.clone(),
                resolution: PluginResolution::from_services(&services),
            }),
            services,
        )
        .await
    }
}
struct ScopedSourceFactory {
    factory: Arc<SourcePluginFactory>,
    resolution: PluginResolution,
}
#[async_trait]
impl SourcePluginConstructor for ScopedSourceFactory {
    async fn create(&self) -> anyhow::Result<Box<dyn drasi_lib::Source>> {
        self.resolution.run(self.factory.create()).await
    }
}
#[async_trait]
impl SourcePluginConstructor for SourcePluginFactory {
    async fn create(&self) -> anyhow::Result<Box<dyn drasi_lib::Source>> {
        self.schema.validate(&self.recipe.config)?;
        self.descriptor
            .create_source(&self.id, &self.recipe.config, self.auto_start)
            .await
    }
}

pub struct ReactionPluginFactory {
    descriptor: Arc<dyn ReactionPluginDescriptor>,
    id: String,
    queries: Vec<String>,
    recipe: PluginConfiguration,
    schema: DescriptorSchema,
    auto_start: bool,
}
impl ReactionPluginFactory {
    pub fn new(
        descriptor: Arc<dyn ReactionPluginDescriptor>,
        id: impl Into<String>,
        queries: Vec<String>,
        recipe: PluginConfiguration,
        auto_start: bool,
    ) -> anyhow::Result<Arc<Self>> {
        recipe.matches(descriptor.kind(), descriptor.config_version())?;
        let schema = DescriptorSchema::new(
            descriptor.config_schema_name(),
            &descriptor.config_schema_json(),
        )?;
        schema.validate(&recipe.config)?;
        let id = id.into();
        drasi_lib::computation::v1::ComponentId::try_new(id.as_str())?;
        for query in &queries {
            drasi_lib::computation::v1::ComponentId::try_new(query.as_str())?;
        }
        Ok(Arc::new(Self {
            descriptor,
            id,
            queries,
            recipe,
            schema,
            auto_start,
        }))
    }
    pub fn configuration(&self) -> &PluginConfiguration {
        &self.recipe
    }
    pub async fn host(
        self: &Arc<Self>,
        services: LegacyPluginServices,
        catalog: QueryResultsCatalog,
        options: ReactionPluginOptions,
    ) -> anyhow::Result<Arc<ReactionPluginHost>> {
        ReactionPluginHost::recreatable(
            Arc::new(ScopedReactionFactory {
                factory: self.clone(),
                resolution: PluginResolution::from_services(&services),
            }),
            services,
            catalog,
            options,
        )
        .await
    }
}
struct ScopedReactionFactory {
    factory: Arc<ReactionPluginFactory>,
    resolution: PluginResolution,
}
#[async_trait]
impl ReactionPluginConstructor for ScopedReactionFactory {
    async fn create(&self) -> anyhow::Result<Box<dyn drasi_lib::Reaction>> {
        self.resolution.run(self.factory.create()).await
    }
}
#[async_trait]
impl ReactionPluginConstructor for ReactionPluginFactory {
    async fn create(&self) -> anyhow::Result<Box<dyn drasi_lib::Reaction>> {
        self.schema.validate(&self.recipe.config)?;
        self.descriptor
            .create_reaction(
                &self.id,
                self.queries.clone(),
                &self.recipe.config,
                self.auto_start,
            )
            .await
    }
}

pub struct BootstrapPluginFactory {
    descriptor: Arc<dyn BootstrapPluginDescriptor>,
    recipe: PluginConfiguration,
    schema: DescriptorSchema,
    source_config: Value,
}
impl BootstrapPluginFactory {
    pub fn new(
        descriptor: Arc<dyn BootstrapPluginDescriptor>,
        recipe: PluginConfiguration,
        source_config: Value,
    ) -> anyhow::Result<Self> {
        recipe.matches(descriptor.kind(), descriptor.config_version())?;
        let schema = DescriptorSchema::new(
            descriptor.config_schema_name(),
            &descriptor.config_schema_json(),
        )?;
        schema.validate(&recipe.config)?;
        Ok(Self {
            descriptor,
            recipe,
            schema,
            source_config,
        })
    }
    pub fn configuration(&self) -> &PluginConfiguration {
        &self.recipe
    }
    pub async fn create(&self) -> anyhow::Result<Box<dyn drasi_lib::bootstrap::BootstrapProvider>> {
        self.schema.validate(&self.recipe.config)?;
        self.descriptor
            .create_bootstrap_provider(&self.recipe.config, &self.source_config)
            .await
    }
    pub async fn create_with_services(
        &self,
        services: &LegacyPluginServices,
    ) -> anyhow::Result<Box<dyn drasi_lib::bootstrap::BootstrapProvider>> {
        PluginResolution::from_services(services)
            .run(self.create())
            .await
    }
}

pub async fn create_scoped_index_provider(
    descriptor: Arc<dyn IndexBackendPluginDescriptor>,
    recipe: &PluginConfiguration,
    services: &LegacyPluginServices,
) -> anyhow::Result<Arc<LegacyIndexProviderAdapter>> {
    recipe.matches(descriptor.kind(), descriptor.config_version())?;
    DescriptorSchema::new(
        descriptor.config_schema_name(),
        &descriptor.config_schema_json(),
    )?
    .validate(&recipe.config)?;
    let provider = PluginResolution::from_services(services)
        .run(descriptor.create_index_backend(&recipe.config))
        .await?;
    Ok(LegacyIndexProviderAdapter::scoped(
        provider,
        services.scope.clone(),
    )?)
}

pub async fn create_index_provider(
    descriptor: Arc<dyn IndexBackendPluginDescriptor>,
    recipe: &PluginConfiguration,
) -> anyhow::Result<Arc<LegacyIndexProviderAdapter>> {
    recipe.matches(descriptor.kind(), descriptor.config_version())?;
    DescriptorSchema::new(
        descriptor.config_schema_name(),
        &descriptor.config_schema_json(),
    )?
    .validate(&recipe.config)?;
    Ok(LegacyIndexProviderAdapter::new(
        descriptor.create_index_backend(&recipe.config).await?,
    ))
}
pub async fn create_identity_provider(
    descriptor: Arc<dyn IdentityProviderPluginDescriptor>,
    recipe: &PluginConfiguration,
) -> anyhow::Result<Arc<dyn drasi_lib::identity::IdentityProvider>> {
    recipe.matches(descriptor.kind(), descriptor.config_version())?;
    DescriptorSchema::new(
        descriptor.config_schema_name(),
        &descriptor.config_schema_json(),
    )?
    .validate(&recipe.config)?;
    Ok(descriptor
        .create_identity_provider(&recipe.config)
        .await?
        .into())
}
pub async fn create_secret_store(
    descriptor: Arc<dyn SecretStorePluginDescriptor>,
    recipe: &PluginConfiguration,
) -> anyhow::Result<Arc<dyn drasi_lib::secret_store::SecretStoreProvider>> {
    recipe.matches(descriptor.kind(), descriptor.config_version())?;
    DescriptorSchema::new(
        descriptor.config_schema_name(),
        &descriptor.config_schema_json(),
    )?
    .validate(&recipe.config)?;
    Ok(descriptor.create_secret_store(&recipe.config).await?.into())
}
