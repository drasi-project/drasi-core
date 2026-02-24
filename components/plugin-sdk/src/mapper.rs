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
//!         let host = mapper.resolve_string(&dto.host)?;
//!         let port = mapper.resolve_typed(&dto.port)?;
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
use crate::resolver::{EnvironmentVariableResolver, ResolverError, SecretResolver, ValueResolver};
use std::collections::HashMap;
use std::str::FromStr;
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
    resolvers: HashMap<&'static str, Box<dyn ValueResolver>>,
}

impl DtoMapper {
    /// Create a new mapper with the default resolvers (environment variable + secret).
    pub fn new() -> Self {
        let mut resolvers: HashMap<&'static str, Box<dyn ValueResolver>> = HashMap::new();
        resolvers.insert("EnvironmentVariable", Box::new(EnvironmentVariableResolver));
        resolvers.insert("Secret", Box::new(SecretResolver));

        Self { resolvers }
    }

    /// Register a custom [`ValueResolver`] for a given reference kind.
    ///
    /// This replaces any previously registered resolver for the same kind.
    pub fn with_resolver(mut self, kind: &'static str, resolver: Box<dyn ValueResolver>) -> Self {
        self.resolvers.insert(kind, resolver);
        self
    }

    /// Resolve a `ConfigValue<String>` to its actual string value.
    pub fn resolve_string(&self, value: &ConfigValue<String>) -> Result<String, ResolverError> {
        match value {
            ConfigValue::Static(s) => Ok(s.clone()),

            ConfigValue::Secret { .. } => {
                let resolver = self
                    .resolvers
                    .get("Secret")
                    .ok_or_else(|| ResolverError::NoResolverFound("Secret".to_string()))?;
                resolver.resolve_to_string(value)
            }

            ConfigValue::EnvironmentVariable { .. } => {
                let resolver = self.resolvers.get("EnvironmentVariable").ok_or_else(|| {
                    ResolverError::NoResolverFound("EnvironmentVariable".to_string())
                })?;
                resolver.resolve_to_string(value)
            }
        }
    }

    /// Resolve a `ConfigValue<T>` to its typed value.
    ///
    /// For `Static` values, returns the value directly. For `EnvironmentVariable` and
    /// `Secret` references, resolves to a string first, then parses to `T` via [`FromStr`].
    pub fn resolve_typed<T>(&self, value: &ConfigValue<T>) -> Result<T, ResolverError>
    where
        T: FromStr + Clone + serde::Serialize + serde::de::DeserializeOwned,
        T::Err: std::fmt::Display,
    {
        match value {
            ConfigValue::Static(v) => Ok(v.clone()),

            ConfigValue::Secret { name } => {
                let string_val = self.resolve_secret_to_string(name)?;
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
    pub fn resolve_optional<T>(
        &self,
        value: &Option<ConfigValue<T>>,
    ) -> Result<Option<T>, ResolverError>
    where
        T: FromStr + Clone + serde::Serialize + serde::de::DeserializeOwned,
        T::Err: std::fmt::Display,
    {
        value.as_ref().map(|v| self.resolve_typed(v)).transpose()
    }

    /// Resolve an optional `ConfigValue<String>` to `Option<String>`.
    pub fn resolve_optional_string(
        &self,
        value: &Option<ConfigValue<String>>,
    ) -> Result<Option<String>, ResolverError> {
        value.as_ref().map(|v| self.resolve_string(v)).transpose()
    }

    /// Resolve a slice of `ConfigValue<String>` to `Vec<String>`.
    pub fn resolve_string_vec(
        &self,
        values: &[ConfigValue<String>],
    ) -> Result<Vec<String>, ResolverError> {
        values.iter().map(|v| self.resolve_string(v)).collect()
    }

    /// Helper: resolve secret name to string (delegates to SecretResolver).
    fn resolve_secret_to_string(&self, name: &str) -> Result<String, ResolverError> {
        Err(ResolverError::NotImplemented(format!(
            "Secret resolution not yet implemented for '{name}'"
        )))
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

    #[test]
    fn test_resolve_string_static() {
        let mapper = DtoMapper::new();
        let value = ConfigValue::Static("hello".to_string());

        let result = mapper.resolve_string(&value).expect("resolve");
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_resolve_string_env_var() {
        std::env::set_var("TEST_SDK_MAPPER_VAR", "mapped_value");

        let mapper = DtoMapper::new();
        let value = ConfigValue::EnvironmentVariable {
            name: "TEST_SDK_MAPPER_VAR".to_string(),
            default: None,
        };

        let result = mapper.resolve_string(&value).expect("resolve");
        assert_eq!(result, "mapped_value");

        std::env::remove_var("TEST_SDK_MAPPER_VAR");
    }

    #[test]
    fn test_resolve_typed_u16() {
        let mapper = DtoMapper::new();
        let value = ConfigValue::Static(5432u16);

        let result = mapper.resolve_typed(&value).expect("resolve");
        assert_eq!(result, 5432u16);
    }

    #[test]
    fn test_resolve_typed_u16_from_env() {
        std::env::set_var("TEST_SDK_PORT", "8080");

        let mapper = DtoMapper::new();
        let value: ConfigValue<u16> = ConfigValue::EnvironmentVariable {
            name: "TEST_SDK_PORT".to_string(),
            default: None,
        };

        let result = mapper.resolve_typed(&value).expect("resolve");
        assert_eq!(result, 8080u16);

        std::env::remove_var("TEST_SDK_PORT");
    }

    #[test]
    fn test_resolve_typed_parse_error() {
        std::env::set_var("TEST_SDK_INVALID_PORT", "not_a_number");

        let mapper = DtoMapper::new();
        let value: ConfigValue<u16> = ConfigValue::EnvironmentVariable {
            name: "TEST_SDK_INVALID_PORT".to_string(),
            default: None,
        };

        let result = mapper.resolve_typed(&value);
        assert!(result.is_err());
        assert!(matches!(
            result.expect_err("should fail"),
            ResolverError::ParseError(_)
        ));

        std::env::remove_var("TEST_SDK_INVALID_PORT");
    }

    #[test]
    fn test_resolve_optional_some() {
        let mapper = DtoMapper::new();
        let value = Some(ConfigValue::Static("test".to_string()));

        let result = mapper.resolve_optional(&value).expect("resolve");
        assert_eq!(result, Some("test".to_string()));
    }

    #[test]
    fn test_resolve_optional_none() {
        let mapper = DtoMapper::new();
        let value: Option<ConfigValue<String>> = None;

        let result = mapper.resolve_optional(&value).expect("resolve");
        assert_eq!(result, None);
    }

    #[test]
    fn test_resolve_string_vec() {
        let mapper = DtoMapper::new();
        let values = vec![
            ConfigValue::Static("a".to_string()),
            ConfigValue::Static("b".to_string()),
        ];

        let result = mapper.resolve_string_vec(&values).expect("resolve");
        assert_eq!(result, vec!["a", "b"]);
    }

    #[test]
    fn test_config_mapper_trait() {
        struct TestMapper;

        #[derive(Debug)]
        struct TestDto {
            host: ConfigValue<String>,
        }

        struct TestDomain {
            host: String,
        }

        impl ConfigMapper<TestDto, TestDomain> for TestMapper {
            fn map(
                &self,
                dto: &TestDto,
                resolver: &DtoMapper,
            ) -> Result<TestDomain, MappingError> {
                Ok(TestDomain {
                    host: resolver.resolve_string(&dto.host)?,
                })
            }
        }

        let mapper = DtoMapper::new();
        let dto = TestDto {
            host: ConfigValue::Static("localhost".to_string()),
        };

        let domain = mapper.map_with(&dto, &TestMapper).expect("map");
        assert_eq!(domain.host, "localhost");
    }

    #[test]
    fn test_custom_resolver() {
        struct AlwaysResolver;
        impl ValueResolver for AlwaysResolver {
            fn resolve_to_string(
                &self,
                _value: &ConfigValue<String>,
            ) -> Result<String, ResolverError> {
                Ok("custom-resolved".to_string())
            }
        }

        let mapper = DtoMapper::new().with_resolver("Secret", Box::new(AlwaysResolver));
        let value = ConfigValue::Secret {
            name: "test".to_string(),
        };

        let result = mapper.resolve_string(&value).expect("resolve");
        assert_eq!(result, "custom-resolved");
    }
}
