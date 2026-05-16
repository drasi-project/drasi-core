// Copyright 2025 The Drasi Authors.
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

//! DTO-to-domain model mapping service with value resolution.
//!
//! The [`DtoMapper`] is the main mapping service that plugins use to convert their
//! DTO configuration structs into domain model values. It resolves [`ConfigValue`]
//! references (environment variables, secrets) into their actual values.
//!
//! # Usage in Plugin Descriptors
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::prelude::*;
//!
//! struct MySourceDescriptor;
//!
//! #[async_trait]
//! impl SourcePluginDescriptor for MySourceDescriptor {
//!     // ... other methods ...
//!
//!     async fn create_source(
//!         &self,
//!         id: &str,
//!         config_json: &serde_json::Value,
//!         auto_start: bool,
//!     ) -> anyhow::Result<Box<dyn drasi_lib::Source>> {
//!         // Deserialize the JSON into the plugin's DTO
//!         let dto: MySourceConfigDto = serde_json::from_value(config_json.clone())?;
//!
//!         // Create a mapper to resolve config values
//!         let mapper = DtoMapper::new();
//!
//!         // Resolve individual fields
//!         let host = mapper.resolve_string(&dto.host).await?;
//!         let port = mapper.resolve_typed(&dto.port).await?;
//!
//!         // Build the source using resolved values
//!         Ok(Box::new(MySource::new(id, host, port, auto_start)))
//!     }
//! }
//! ```
//!
//! # The ConfigMapper Pattern
//!
//! For complex mappings, implement the [`ConfigMapper`] trait to encapsulate the
//! conversion logic:
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::prelude::*;
//!
//! struct MyConfigMapper;
//!
//! impl ConfigMapper<MySourceConfigDto, MySourceConfig> for MyConfigMapper {
//!     fn map(&self, dto: &MySourceConfigDto, resolver: &DtoMapper) -> Result<MySourceConfig, MappingError> {
//!         Ok(MySourceConfig {
//!             host: resolver.resolve_string(&dto.host)?,
//!             port: resolver.resolve_typed(&dto.port)?,
//!             timeout: resolver.resolve_optional(&dto.timeout_ms)?,
//!         })
//!     }
//! }
//! ```

use crate::config_value::ConfigValue;
use crate::resolver::{
    get_secret_resolver, EnvironmentVariableResolver, ResolverError, SecretResolver, ValueResolver,
};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;

/// Errors that can occur during DTO-to-domain mapping.
#[derive(Debug, Error)]
pub enum MappingError {
    /// A [`ConfigValue`] reference could not be resolved.
    #[error("Failed to resolve config value: {0}")]
    ResolutionError(#[from] ResolverError),

    /// No mapper was found for the given config type.
    #[error("No mapper found for config type: {0}")]
    NoMapperFound(String),

    /// The mapper received a DTO type it doesn't handle.
    #[error("Mapper type mismatch")]
    MapperTypeMismatch,

    /// Source creation failed.
    #[error("Failed to create source: {0}")]
    SourceCreationError(String),

    /// Reaction creation failed.
    #[error("Failed to create reaction: {0}")]
    ReactionCreationError(String),

    /// A configuration value was invalid.
    #[error("Invalid value: {0}")]
    InvalidValue(String),
}

/// Trait for converting a specific DTO config type to its domain model.
///
/// Implement this trait when you have a complex mapping between a DTO and its
/// corresponding domain type. The `resolver` parameter provides access to
/// [`DtoMapper`] for resolving [`ConfigValue`] references.
///
/// # Type Parameters
///
/// - `TDto` — The DTO (Data Transfer Object) type from the API layer.
/// - `TDomain` — The domain model type used internally by the plugin.
pub trait ConfigMapper<TDto, TDomain>: Send + Sync {
    /// Convert a DTO to its domain model, resolving any config value references.
    fn map(&self, dto: &TDto, resolver: &DtoMapper) -> Result<TDomain, MappingError>;
}

/// Main mapping service that resolves [`ConfigValue`] references in plugin DTOs.
///
/// Provides methods to resolve `ConfigValue<T>` fields into their actual values
/// by dispatching to the appropriate [`ValueResolver`] based on the variant.
///
/// # Default Resolvers
///
/// - `"EnvironmentVariable"` → [`EnvironmentVariableResolver`]
/// - `"Secret"` → [`SecretResolver`] (currently returns `NotImplemented`)
pub struct DtoMapper {
    resolvers: HashMap<&'static str, Arc<dyn ValueResolver>>,
}

impl DtoMapper {
    /// Create a new mapper with the default resolvers (environment variable + secret).
    ///
    /// If a global secret resolver has been registered via
    /// [`register_secret_resolver`](crate::resolver::register_secret_resolver),
    /// it will be used automatically. Otherwise, the default [`SecretResolver`]
    /// stub is used (which returns `NotImplemented`).
    pub fn new() -> Self {
        let mut resolvers: HashMap<&'static str, Arc<dyn ValueResolver>> = HashMap::new();
        resolvers.insert("EnvironmentVariable", Arc::new(EnvironmentVariableResolver));

        let secret_resolver = get_secret_resolver().unwrap_or_else(|| Arc::new(SecretResolver));
        resolvers.insert("Secret", secret_resolver);

        Self { resolvers }
    }

    /// Register a custom [`ValueResolver`] for a given reference kind.
    ///
    /// This replaces any previously registered resolver for the same kind.
    pub fn with_resolver(mut self, kind: &'static str, resolver: Arc<dyn ValueResolver>) -> Self {
        self.resolvers.insert(kind, resolver);
        self
    }

    /// Resolve a `ConfigValue<String>` to its actual string value.
    pub async fn resolve_string(
        &self,
        value: &ConfigValue<String>,
    ) -> Result<String, ResolverError> {
        match value {
            ConfigValue::Static(s) => Ok(s.clone()),

            ConfigValue::Secret { .. } => {
                let resolver = self
                    .resolvers
                    .get("Secret")
                    .ok_or_else(|| ResolverError::NoResolverFound("Secret".to_string()))?;
                resolver.resolve_to_string(value).await
            }

            ConfigValue::EnvironmentVariable { .. } => {
                let resolver = self.resolvers.get("EnvironmentVariable").ok_or_else(|| {
                    ResolverError::NoResolverFound("EnvironmentVariable".to_string())
                })?;
                resolver.resolve_to_string(value).await
            }
        }
    }

    /// Resolve a `ConfigValue<T>` to its typed value.
    ///
    /// For `Static` values, returns the value directly. For `EnvironmentVariable` and
    /// `Secret` references, resolves to a string first, then parses to `T` via [`FromStr`].
    pub async fn resolve_typed<T>(&self, value: &ConfigValue<T>) -> Result<T, ResolverError>
    where
        T: FromStr + Clone + serde::Serialize + serde::de::DeserializeOwned,
        T::Err: std::fmt::Display,
    {
        match value {
            ConfigValue::Static(v) => Ok(v.clone()),

            ConfigValue::Secret { name } => {
                let resolver = self
                    .resolvers
                    .get("Secret")
                    .ok_or_else(|| ResolverError::NoResolverFound("Secret".to_string()))?;
                let string_cv = ConfigValue::Secret { name: name.clone() };
                let string_val = resolver.resolve_to_string(&string_cv).await?;
                string_val.parse::<T>().map_err(|e| {
                    ResolverError::ParseError(format!("Failed to parse secret '{name}': {e}"))
                })
            }

            ConfigValue::EnvironmentVariable { name, default } => {
                let string_val = std::env::var(name).or_else(|_| {
                    default
                        .clone()
                        .ok_or_else(|| ResolverError::EnvVarNotFound(name.clone()))
                })?;

                string_val.parse::<T>().map_err(|e| {
                    ResolverError::ParseError(format!("Failed to parse env var '{name}': {e}"))
                })
            }
        }
    }

    /// Resolve an optional `ConfigValue<T>`. Returns `Ok(None)` if the value is `None`.
    pub async fn resolve_optional<T>(
        &self,
        value: &Option<ConfigValue<T>>,
    ) -> Result<Option<T>, ResolverError>
    where
        T: FromStr + Clone + serde::Serialize + serde::de::DeserializeOwned,
        T::Err: std::fmt::Display,
    {
        match value {
            Some(v) => self.resolve_typed(v).await.map(Some),
            None => Ok(None),
        }
    }

    /// Resolve an optional `ConfigValue<String>` to `Option<String>`.
    pub async fn resolve_optional_string(
        &self,
        value: &Option<ConfigValue<String>>,
    ) -> Result<Option<String>, ResolverError> {
        match value {
            Some(v) => self.resolve_string(v).await.map(Some),
            None => Ok(None),
        }
    }

    /// Resolve a slice of `ConfigValue<String>` to `Vec<String>`.
    pub async fn resolve_string_vec(
        &self,
        values: &[ConfigValue<String>],
    ) -> Result<Vec<String>, ResolverError> {
        let mut result = Vec::with_capacity(values.len());
        for v in values {
            result.push(self.resolve_string(v).await?);
        }
        Ok(result)
    }

    /// Map a DTO using a [`ConfigMapper`] implementation.
    pub fn map_with<TDto, TDomain>(
        &self,
        dto: &TDto,
        mapper: &impl ConfigMapper<TDto, TDomain>,
    ) -> Result<TDomain, MappingError> {
        mapper.map(dto, self)
    }
}

impl Default for DtoMapper {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_resolve_string_static() {
        let mapper = DtoMapper::new();
        let value = ConfigValue::Static("hello".to_string());

        let result = mapper.resolve_string(&value).await.expect("resolve");
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn test_resolve_string_env_var() {
        std::env::set_var("TEST_SDK_MAPPER_VAR", "mapped_value");

        let mapper = DtoMapper::new();
        let value = ConfigValue::EnvironmentVariable {
            name: "TEST_SDK_MAPPER_VAR".to_string(),
            default: None,
        };

        let result = mapper.resolve_string(&value).await.expect("resolve");
        assert_eq!(result, "mapped_value");

        std::env::remove_var("TEST_SDK_MAPPER_VAR");
    }

    #[tokio::test]
    async fn test_resolve_typed_u16() {
        let mapper = DtoMapper::new();
        let value = ConfigValue::Static(5432u16);

        let result = mapper.resolve_typed(&value).await.expect("resolve");
        assert_eq!(result, 5432u16);
    }

    #[tokio::test]
    async fn test_resolve_typed_u16_from_env() {
        std::env::set_var("TEST_SDK_PORT", "8080");

        let mapper = DtoMapper::new();
        let value: ConfigValue<u16> = ConfigValue::EnvironmentVariable {
            name: "TEST_SDK_PORT".to_string(),
            default: None,
        };

        let result = mapper.resolve_typed(&value).await.expect("resolve");
        assert_eq!(result, 8080u16);

        std::env::remove_var("TEST_SDK_PORT");
    }

    #[tokio::test]
    async fn test_resolve_typed_parse_error() {
        std::env::set_var("TEST_SDK_INVALID_PORT", "not_a_number");

        let mapper = DtoMapper::new();
        let value: ConfigValue<u16> = ConfigValue::EnvironmentVariable {
            name: "TEST_SDK_INVALID_PORT".to_string(),
            default: None,
        };

        let result = mapper.resolve_typed(&value).await;
        assert!(result.is_err());
        assert!(matches!(
            result.expect_err("should fail"),
            ResolverError::ParseError(_)
        ));

        std::env::remove_var("TEST_SDK_INVALID_PORT");
    }

    #[tokio::test]
    async fn test_resolve_optional_some() {
        let mapper = DtoMapper::new();
        let value = Some(ConfigValue::Static("test".to_string()));

        let result = mapper.resolve_optional(&value).await.expect("resolve");
        assert_eq!(result, Some("test".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_optional_none() {
        let mapper = DtoMapper::new();
        let value: Option<ConfigValue<String>> = None;

        let result = mapper.resolve_optional(&value).await.expect("resolve");
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_resolve_string_vec() {
        let mapper = DtoMapper::new();
        let values = vec![
            ConfigValue::Static("a".to_string()),
            ConfigValue::Static("b".to_string()),
        ];

        let result = mapper.resolve_string_vec(&values).await.expect("resolve");
        assert_eq!(result, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn test_config_mapper_trait() {
        struct TestMapper;

        #[derive(Debug)]
        struct TestDto {
            host: ConfigValue<String>,
        }

        struct TestDomain {
            host: String,
        }

        impl ConfigMapper<TestDto, TestDomain> for TestMapper {
            fn map(&self, dto: &TestDto, resolver: &DtoMapper) -> Result<TestDomain, MappingError> {
                // ConfigMapper::map is still sync; it cannot call async resolve_* directly.
                // For this test, we only test with Static values which don't actually need async.
                match &dto.host {
                    ConfigValue::Static(s) => Ok(TestDomain { host: s.clone() }),
                    _ => Err(MappingError::InvalidValue(
                        "only static in test".to_string(),
                    )),
                }
            }
        }

        let mapper = DtoMapper::new();
        let dto = TestDto {
            host: ConfigValue::Static("localhost".to_string()),
        };

        let domain = mapper.map_with(&dto, &TestMapper).expect("map");
        assert_eq!(domain.host, "localhost");
    }

    #[tokio::test]
    async fn test_custom_resolver() {
        struct AlwaysResolver;
        #[async_trait::async_trait]
        impl ValueResolver for AlwaysResolver {
            async fn resolve_to_string(
                &self,
                _value: &ConfigValue<String>,
            ) -> Result<String, ResolverError> {
                Ok("custom-resolved".to_string())
            }
        }

        let mapper = DtoMapper::new().with_resolver("Secret", Arc::new(AlwaysResolver));
        let value = ConfigValue::Secret {
            name: "test".to_string(),
        };

        let result = mapper.resolve_string(&value).await.expect("resolve");
        assert_eq!(result, "custom-resolved");
    }
}
