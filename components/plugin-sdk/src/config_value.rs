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

//! Configuration value types that support static values or dynamic references.
//!
//! [`ConfigValue<T>`] is the core type used in plugin DTOs for configuration fields that
//! may be provided as static values, environment variable references, or secret references.
//!
//! # Supported Formats
//!
//! Each `ConfigValue<T>` field can be specified in three formats:
//!
//! ## Static Value
//! A plain value of type `T`:
//! ```yaml
//! port: 5432
//! host: "localhost"
//! ```
//!
//! ## POSIX Environment Variable
//! A `${VAR}` or `${VAR:-default}` reference:
//! ```yaml
//! port: "${DB_PORT:-5432}"
//! host: "${DB_HOST}"
//! ```
//!
//! ## Structured Reference
//! An object with a `kind` discriminator:
//! ```yaml
//! password:
//!   kind: Secret
//!   name: db-password
//! port:
//!   kind: EnvironmentVariable
//!   name: DB_PORT
//!   default: "5432"
//! ```
//!
//! # Usage in Plugin DTOs
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::prelude::*;
//!
//! #[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
//! #[serde(rename_all = "camelCase")]
//! pub struct MySourceConfigDto {
//!     /// Database host
//!     #[schema(value_type = ConfigValueString)]
//!     pub host: ConfigValue<String>,
//!
//!     /// Database port
//!     #[schema(value_type = ConfigValueU16)]
//!     pub port: ConfigValue<u16>,
//!
//!     /// Optional connection timeout in milliseconds
//!     #[serde(skip_serializing_if = "Option::is_none")]
//!     #[schema(value_type = Option<ConfigValueU32>)]
//!     pub timeout_ms: Option<ConfigValue<u32>>,
//! }
//! ```

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// A configuration value that can be a static value or a reference to be resolved at runtime.
///
/// This enum is the building block for plugin configuration DTOs. It supports three variants:
///
/// - **Static** — A plain value of type `T`, provided directly in the configuration.
/// - **Secret** — A reference to a named secret, resolved at runtime by the server.
/// - **EnvironmentVariable** — A reference to an environment variable, with an optional default.
///
/// # Serialization
///
/// - `Static` values serialize as the plain `T` value.
/// - `Secret` serializes as `{"kind": "Secret", "name": "..."}`.
/// - `EnvironmentVariable` serializes as `{"kind": "EnvironmentVariable", "name": "...", "default": "..."}`.
///
/// # Deserialization
///
/// Supports three input formats (see module docs for examples):
/// 1. Plain value → `Static(T)`
/// 2. POSIX `${VAR:-default}` string → `EnvironmentVariable { name, default }`
/// 3. Structured `{"kind": "...", ...}` → `Secret` or `EnvironmentVariable`
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue<T>
where
    T: Serialize + DeserializeOwned + Clone,
{
    /// A reference to a secret (resolved to string at runtime, then parsed to `T`).
    Secret { name: String },

    /// A reference to an environment variable.
    EnvironmentVariable {
        name: String,
        default: Option<String>,
    },

    /// A static value of type `T`.
    Static(T),
}

/// Type alias for `ConfigValue<String>` — the most common config value type.
pub type ConfigValueString = ConfigValue<String>;
/// Type alias for `ConfigValue<u16>` — commonly used for port numbers.
pub type ConfigValueU16 = ConfigValue<u16>;
/// Type alias for `ConfigValue<u32>` — commonly used for timeouts and counts.
pub type ConfigValueU32 = ConfigValue<u32>;
/// Type alias for `ConfigValue<u64>` — commonly used for large numeric values.
pub type ConfigValueU64 = ConfigValue<u64>;
/// Type alias for `ConfigValue<usize>` — commonly used for sizes and capacities.
pub type ConfigValueUsize = ConfigValue<usize>;
/// Type alias for `ConfigValue<bool>` — commonly used for feature flags.
pub type ConfigValueBool = ConfigValue<bool>;

/// OpenAPI schema wrapper for [`ConfigValueString`].
///
/// Use this as `#[schema(value_type = ConfigValueString)]` in utoipa-annotated DTOs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = ConfigValueString)]
pub struct ConfigValueStringSchema(pub ConfigValueString);

/// OpenAPI schema wrapper for [`ConfigValueU16`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = ConfigValueU16)]
pub struct ConfigValueU16Schema(pub ConfigValueU16);

/// OpenAPI schema wrapper for [`ConfigValueU32`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = ConfigValueU32)]
pub struct ConfigValueU32Schema(pub ConfigValueU32);

/// OpenAPI schema wrapper for [`ConfigValueU64`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = ConfigValueU64)]
pub struct ConfigValueU64Schema(pub ConfigValueU64);

/// OpenAPI schema wrapper for [`ConfigValueUsize`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = ConfigValueUsize)]
pub struct ConfigValueUsizeSchema(pub ConfigValueUsize);

/// OpenAPI schema wrapper for [`ConfigValueBool`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = ConfigValueBool)]
pub struct ConfigValueBoolSchema(pub ConfigValueBool);

// Custom serialization to support the discriminated union format
impl<T> Serialize for ConfigValue<T>
where
    T: Serialize + DeserializeOwned + Clone,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        match self {
            ConfigValue::Secret { name } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("kind", "Secret")?;
                map.serialize_entry("name", name)?;
                map.end()
            }
            ConfigValue::EnvironmentVariable { name, default } => {
                let size = if default.is_some() { 3 } else { 2 };
                let mut map = serializer.serialize_map(Some(size))?;
                map.serialize_entry("kind", "EnvironmentVariable")?;
                map.serialize_entry("name", name)?;
                if let Some(d) = default {
                    map.serialize_entry("default", d)?;
                }
                map.end()
            }
            ConfigValue::Static(value) => value.serialize(serializer),
        }
    }
}

// Custom deserialization to support POSIX format, structured format, and static values
impl<'de, T> Deserialize<'de> for ConfigValue<T>
where
    T: Serialize + DeserializeOwned + Clone + 'static,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        use serde_json::Value;

        let value = Value::deserialize(deserializer)?;

        // Try to deserialize as structured format with "kind" field
        if let Value::Object(ref map) = value {
            if let Some(Value::String(kind)) = map.get("kind") {
                match kind.as_str() {
                    "Secret" => {
                        let name = map
                            .get("name")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| D::Error::missing_field("name"))?
                            .to_string();

                        return Ok(ConfigValue::Secret { name });
                    }
                    "EnvironmentVariable" => {
                        let name = map
                            .get("name")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| D::Error::missing_field("name"))?
                            .to_string();

                        let default = map
                            .get("default")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        return Ok(ConfigValue::EnvironmentVariable { name, default });
                    }
                    _ => {
                        return Err(D::Error::custom(format!("Unknown kind: {kind}")));
                    }
                }
            }
        }

        // Try to parse POSIX format for any type (the string will be parsed to T later)
        if let Value::String(s) = &value {
            if let Some(env_ref) = parse_posix_env_var(s) {
                return Ok(env_ref);
            }
        }

        // Otherwise, deserialize as static value
        let static_value: T = serde_json::from_value(value)
            .map_err(|e| D::Error::custom(format!("Failed to deserialize as static value: {e}")))?;

        Ok(ConfigValue::Static(static_value))
    }
}

/// Parse POSIX-style environment variable reference like `${VAR:-default}` or `${VAR}`.
fn parse_posix_env_var<T>(s: &str) -> Option<ConfigValue<T>>
where
    T: Clone + Serialize + DeserializeOwned,
{
    if !s.starts_with("${") || !s.ends_with('}') {
        return None;
    }

    let inner = &s[2..s.len() - 1];

    if let Some(colon_pos) = inner.find(":-") {
        let name = inner[..colon_pos].to_string();
        let default = Some(inner[colon_pos + 2..].to_string());
        Some(ConfigValue::EnvironmentVariable { name, default })
    } else {
        let name = inner.to_string();
        Some(ConfigValue::EnvironmentVariable {
            name,
            default: None,
        })
    }
}

impl<T> Default for ConfigValue<T>
where
    T: Serialize + DeserializeOwned + Clone + Default,
{
    fn default() -> Self {
        ConfigValue::Static(T::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_static_string() {
        let json = r#""hello""#;
        let value: ConfigValue<String> = serde_json::from_str(json).expect("deserialize");
        assert_eq!(value, ConfigValue::Static("hello".to_string()));
    }

    #[test]
    fn test_deserialize_static_number() {
        let json = r#"5432"#;
        let value: ConfigValue<u16> = serde_json::from_str(json).expect("deserialize");
        assert_eq!(value, ConfigValue::Static(5432));
    }

    #[test]
    fn test_deserialize_posix_with_default() {
        let json = r#""${DB_PORT:-5432}""#;
        let value: ConfigValue<String> = serde_json::from_str(json).expect("deserialize");
        match value {
            ConfigValue::EnvironmentVariable { name, default } => {
                assert_eq!(name, "DB_PORT");
                assert_eq!(default, Some("5432".to_string()));
            }
            _ => panic!("Expected EnvironmentVariable"),
        }
    }

    #[test]
    fn test_deserialize_posix_without_default() {
        let json = r#""${DB_HOST}""#;
        let value: ConfigValue<String> = serde_json::from_str(json).expect("deserialize");
        match value {
            ConfigValue::EnvironmentVariable { name, default } => {
                assert_eq!(name, "DB_HOST");
                assert_eq!(default, None);
            }
            _ => panic!("Expected EnvironmentVariable"),
        }
    }

    #[test]
    fn test_deserialize_structured_secret() {
        let json = r#"{"kind": "Secret", "name": "db-password"}"#;
        let value: ConfigValue<String> = serde_json::from_str(json).expect("deserialize");
        match value {
            ConfigValue::Secret { name } => assert_eq!(name, "db-password"),
            _ => panic!("Expected Secret"),
        }
    }

    #[test]
    fn test_deserialize_structured_env() {
        let json = r#"{"kind": "EnvironmentVariable", "name": "DB_HOST", "default": "localhost"}"#;
        let value: ConfigValue<String> = serde_json::from_str(json).expect("deserialize");
        match value {
            ConfigValue::EnvironmentVariable { name, default } => {
                assert_eq!(name, "DB_HOST");
                assert_eq!(default, Some("localhost".to_string()));
            }
            _ => panic!("Expected EnvironmentVariable"),
        }
    }

    #[test]
    fn test_serialize_static() {
        let value = ConfigValue::Static("hello".to_string());
        let json = serde_json::to_string(&value).expect("serialize");
        assert_eq!(json, r#""hello""#);
    }

    #[test]
    fn test_serialize_env_var() {
        let value: ConfigValue<String> = ConfigValue::EnvironmentVariable {
            name: "DB_HOST".to_string(),
            default: Some("localhost".to_string()),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");
        assert_eq!(parsed["kind"], "EnvironmentVariable");
        assert_eq!(parsed["name"], "DB_HOST");
        assert_eq!(parsed["default"], "localhost");
    }

    #[test]
    fn test_serialize_secret() {
        let value: ConfigValue<String> = ConfigValue::Secret {
            name: "my-secret".to_string(),
        };
        let json = serde_json::to_string(&value).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");
        assert_eq!(parsed["kind"], "Secret");
        assert_eq!(parsed["name"], "my-secret");
    }

    #[test]
    fn test_roundtrip_static() {
        let original = ConfigValue::Static(42u16);
        let json = serde_json::to_string(&original).expect("serialize");
        let deserialized: ConfigValue<u16> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_default() {
        let value: ConfigValue<String> = ConfigValue::default();
        assert_eq!(value, ConfigValue::Static(String::default()));
    }
}
