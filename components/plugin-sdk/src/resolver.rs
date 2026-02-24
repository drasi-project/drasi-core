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

//! Value resolvers for [`ConfigValue`] reference types.
//!
//! Resolvers convert [`ConfigValue::EnvironmentVariable`] and [`ConfigValue::Secret`]
//! references into their actual values at runtime. The server provides built-in resolvers
//! for environment variables and secrets.
//!
//! # Built-in Resolvers
//!
//! - [`EnvironmentVariableResolver`] — Reads from `std::env::var()`, falls back to default.
//! - [`SecretResolver`] — Stub that returns `NotImplemented` (will be backed by a secret store).
//!
//! # Custom Resolvers
//!
//! Implement [`ValueResolver`] to add custom resolution logic (e.g., HashiCorp Vault, AWS SSM):
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::resolver::{ValueResolver, ResolverError};
//! use drasi_plugin_sdk::ConfigValue;
//!
//! struct VaultResolver { /* client */ }
//!
//! impl ValueResolver for VaultResolver {
//!     fn resolve_to_string(&self, value: &ConfigValue<String>) -> Result<String, ResolverError> {
//!         match value {
//!             ConfigValue::Secret { name } => {
//!                 // Look up secret in Vault
//!                 Ok("resolved-value".to_string())
//!             }
//!             _ => Err(ResolverError::WrongResolverType),
//!         }
//!     }
//! }
//! ```

use crate::config_value::ConfigValue;
use thiserror::Error;

/// Errors that can occur during value resolution.
#[derive(Debug, Error)]
pub enum ResolverError {
    /// The referenced environment variable was not found and no default was provided.
    #[error("Environment variable '{0}' not found and no default provided")]
    EnvVarNotFound(String),

    /// The requested resolution method is not yet implemented.
    #[error("Not implemented: {0}")]
    NotImplemented(String),

    /// No resolver was registered for the given reference type.
    #[error("No resolver found for reference type: {0}")]
    NoResolverFound(String),

    /// A resolver was called with a `ConfigValue` variant it doesn't handle.
    #[error("Wrong resolver type used for this reference")]
    WrongResolverType,

    /// The resolved string value could not be parsed to the target type.
    #[error("Failed to parse value: {0}")]
    ParseError(String),
}

/// Trait for resolving a specific type of [`ConfigValue`] variant to its actual string value.
///
/// Each resolver handles one variant (e.g., `EnvironmentVariable` or `Secret`).
/// The [`DtoMapper`](crate::mapper::DtoMapper) dispatches to the appropriate resolver
/// based on the variant.
pub trait ValueResolver: Send + Sync {
    /// Resolve a [`ConfigValue`] variant to its actual string value.
    ///
    /// Returns `Err(ResolverError::WrongResolverType)` if called with a variant
    /// this resolver doesn't handle.
    fn resolve_to_string(&self, value: &ConfigValue<String>) -> Result<String, ResolverError>;
}

/// Resolves [`ConfigValue::EnvironmentVariable`] references by reading `std::env::var()`.
///
/// Falls back to the `default` value if the environment variable is not set.
/// Returns [`ResolverError::EnvVarNotFound`] if neither the variable nor a default exists.
pub struct EnvironmentVariableResolver;

impl ValueResolver for EnvironmentVariableResolver {
    fn resolve_to_string(&self, value: &ConfigValue<String>) -> Result<String, ResolverError> {
        match value {
            ConfigValue::EnvironmentVariable { name, default } => {
                std::env::var(name).or_else(|_| {
                    default
                        .clone()
                        .ok_or_else(|| ResolverError::EnvVarNotFound(name.clone()))
                })
            }
            _ => Err(ResolverError::WrongResolverType),
        }
    }
}

/// Resolves [`ConfigValue::Secret`] references.
///
/// Currently returns [`ResolverError::NotImplemented`] — will be backed by
/// a pluggable secret store in a future release.
pub struct SecretResolver;

impl ValueResolver for SecretResolver {
    fn resolve_to_string(&self, value: &ConfigValue<String>) -> Result<String, ResolverError> {
        match value {
            ConfigValue::Secret { name } => Err(ResolverError::NotImplemented(format!(
                "Secret resolution not yet implemented for '{name}'"
            ))),
            _ => Err(ResolverError::WrongResolverType),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_resolver_with_set_var() {
        std::env::set_var("TEST_SDK_VAR_1", "test_value");

        let resolver = EnvironmentVariableResolver;
        let value = ConfigValue::EnvironmentVariable {
            name: "TEST_SDK_VAR_1".to_string(),
            default: None,
        };

        let result = resolver.resolve_to_string(&value).expect("resolve");
        assert_eq!(result, "test_value");

        std::env::remove_var("TEST_SDK_VAR_1");
    }

    #[test]
    fn test_env_resolver_with_default() {
        let resolver = EnvironmentVariableResolver;
        let value = ConfigValue::EnvironmentVariable {
            name: "NONEXISTENT_SDK_VAR_12345".to_string(),
            default: Some("default_value".to_string()),
        };

        let result = resolver.resolve_to_string(&value).expect("resolve");
        assert_eq!(result, "default_value");
    }

    #[test]
    fn test_env_resolver_missing_var_no_default() {
        let resolver = EnvironmentVariableResolver;
        let value = ConfigValue::EnvironmentVariable {
            name: "NONEXISTENT_SDK_VAR_67890".to_string(),
            default: None,
        };

        let result = resolver.resolve_to_string(&value);
        assert!(result.is_err());
        assert!(matches!(
            result.expect_err("should fail"),
            ResolverError::EnvVarNotFound(_)
        ));
    }

    #[test]
    fn test_env_resolver_wrong_variant() {
        let resolver = EnvironmentVariableResolver;
        let value = ConfigValue::Secret {
            name: "x".to_string(),
        };
        assert!(matches!(
            resolver.resolve_to_string(&value).expect_err("should fail"),
            ResolverError::WrongResolverType
        ));
    }

    #[test]
    fn test_secret_resolver_not_implemented() {
        let resolver = SecretResolver;
        let value = ConfigValue::Secret {
            name: "my-secret".to_string(),
        };

        let result = resolver.resolve_to_string(&value);
        assert!(result.is_err());
        assert!(matches!(
            result.expect_err("should fail"),
            ResolverError::NotImplemented(_)
        ));
    }

    #[test]
    fn test_secret_resolver_wrong_variant() {
        let resolver = SecretResolver;
        let value = ConfigValue::Static("x".to_string());
        assert!(matches!(
            resolver.resolve_to_string(&value).expect_err("should fail"),
            ResolverError::WrongResolverType
        ));
    }
}
