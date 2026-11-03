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

//! Plugin descriptor for the PostgreSQL bootstrap provider.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::config::{SslMode, TableKeyConfig};
use crate::PostgresBootstrapConfig;
use crate::PostgresBootstrapProvider;

// ── DTO types ────────────────────────────────────────────────────────────────

/// SSL mode DTO (mirrors [`SslMode`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = bootstrap::postgres::SslMode)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum SslModeDto {
    Disable,
    #[default]
    Prefer,
    Require,
}

impl utoipa::PartialSchema for SslModeDto {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        <Self as utoipa::ToSchema>::schema().1
    }
}

impl std::str::FromStr for SslModeDto {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_lowercase().as_str() {
            "disable" => Ok(Self::Disable),
            "prefer" => Ok(Self::Prefer),
            "require" => Ok(Self::Require),
            _ => Err(format!("Invalid SSL mode: {value}")),
        }
    }
}

async fn ssl_mode(value: &ConfigValue<SslModeDto>, mapper: &DtoMapper) -> anyhow::Result<SslMode> {
    Ok(match mapper.resolve_typed(value).await? {
        SslModeDto::Disable => SslMode::Disable,
        SslModeDto::Prefer => SslMode::Prefer,
        SslModeDto::Require => SslMode::Require,
    })
}

/// Table key configuration DTO (mirrors [`TableKeyConfig`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::postgres::TableKeyConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableKeyConfigDto {
    pub table: String,
    pub key_columns: Vec<String>,
}

fn default_host() -> ConfigValue<String> {
    ConfigValue::Static("localhost".to_string()) // DevSkim: ignore DS162092
}

fn default_port() -> ConfigValue<u16> {
    ConfigValue::Static(5432)
}

fn default_password() -> ConfigValue<String> {
    ConfigValue::Static(String::new())
}

fn default_slot_name() -> String {
    "drasi_slot".to_string()
}

fn default_publication_name() -> String {
    "drasi_publication".to_string()
}

fn default_ssl_mode() -> ConfigValue<SslModeDto> {
    ConfigValue::Static(SslModeDto::default())
}

/// Configuration DTO for the PostgreSQL bootstrap provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::postgres::PostgresBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PostgresBootstrapConfigDto {
    #[serde(default = "default_host")]
    #[schema(schema_with = ConfigValue::<String>::schema)]
    pub host: ConfigValue<String>,

    #[serde(default = "default_port")]
    #[schema(schema_with = ConfigValue::<u16>::schema)]
    pub port: ConfigValue<u16>,

    #[schema(schema_with = ConfigValue::<String>::schema)]
    pub database: ConfigValue<String>,

    #[schema(schema_with = ConfigValue::<String>::schema)]
    pub user: ConfigValue<String>,

    #[serde(default = "default_password")]
    #[schema(schema_with = ConfigValue::<String>::schema)]
    pub password: ConfigValue<String>,

    #[serde(default)]
    pub tables: Vec<String>,

    #[serde(default = "default_slot_name")]
    pub slot_name: String,

    #[serde(default = "default_publication_name")]
    pub publication_name: String,

    #[serde(default = "default_ssl_mode")]
    #[schema(schema_with = ConfigValue::<SslModeDto>::schema)]
    pub ssl_mode: ConfigValue<SslModeDto>,

    #[serde(default)]
    #[schema(value_type = Vec<bootstrap::postgres::TableKeyConfig>)]
    pub table_keys: Vec<TableKeyConfigDto>,
}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(PostgresBootstrapConfigDto, SslModeDto, TableKeyConfigDto,)))]
struct PostgresBootstrapSchemas;

/// Plugin descriptor for the PostgreSQL bootstrap provider.
pub struct PostgresBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for PostgresBootstrapDescriptor {
    fn kind(&self) -> &str {
        "postgres"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.postgres.PostgresBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = PostgresBootstrapSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        _source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let dto: PostgresBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let table_keys = dto
            .table_keys
            .into_iter()
            .map(|tk| TableKeyConfig {
                table: tk.table,
                key_columns: tk.key_columns,
            })
            .collect();

        let config = PostgresBootstrapConfig {
            host: mapper.resolve_string(&dto.host).await?,
            port: mapper.resolve_typed(&dto.port).await?,
            database: mapper.resolve_string(&dto.database).await?,
            user: mapper.resolve_string(&dto.user).await?,
            password: mapper.resolve_string(&dto.password).await?,
            tables: dto.tables,
            slot_name: dto.slot_name,
            publication_name: dto.publication_name,
            ssl_mode: ssl_mode(&dto.ssl_mode, &mapper).await?,
            table_keys,
        };

        config.validate()?;
        Ok(Box::new(PostgresBootstrapProvider::new(config)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_plugin_sdk::resolver::{ResolverError, ValueResolver};
    use std::sync::Arc;

    struct Resolver(&'static str);

    #[async_trait]
    impl ValueResolver for Resolver {
        async fn resolve_to_string(
            &self,
            _value: &ConfigValue<String>,
        ) -> Result<String, ResolverError> {
            Ok(self.0.into())
        }
    }

    #[tokio::test]
    async fn ssl_mode_references_preserve_the_resolved_policy_and_reject_invalid_values() {
        for (text, expected) in [
            ("require", SslMode::Require),
            ("disable", SslMode::Disable),
            ("prefer", SslMode::Prefer),
        ] {
            let mapper = DtoMapper::new().with_resolver("Secret", Arc::new(Resolver(text)));
            let reference = ConfigValue::Secret {
                name: "tls-policy".into(),
            };
            assert_eq!(ssl_mode(&reference, &mapper).await.unwrap(), expected);
        }
        let mapper = DtoMapper::new().with_resolver("Secret", Arc::new(Resolver("invalid")));
        assert!(ssl_mode(
            &ConfigValue::Secret {
                name: "tls-policy".into()
            },
            &mapper
        )
        .await
        .is_err());
        assert!(
            ssl_mode(
                &ConfigValue::Secret {
                    name: "tls-policy".into()
                },
                &DtoMapper::new(),
            )
            .await
            .is_err(),
            "resolver failures must not downgrade TLS to Prefer"
        );
    }

    #[tokio::test]
    async fn ssl_mode_environment_references_and_static_values_are_not_defaulted() {
        let mapper = DtoMapper::new();
        for (text, expected) in [
            ("require", SslMode::Require),
            ("disable", SslMode::Disable),
            ("prefer", SslMode::Prefer),
        ] {
            let value = ConfigValue::EnvironmentVariable {
                name: "DRASI_TEST_UNUSED_POSTGRES_BOOTSTRAP_SSL_POLICY".into(),
                default: Some(text.into()),
            };
            assert_eq!(ssl_mode(&value, &mapper).await.unwrap(), expected);
            let value = serde_json::from_value(serde_json::json!(text)).unwrap();
            assert_eq!(ssl_mode(&value, &mapper).await.unwrap(), expected);
        }
        let invalid = ConfigValue::EnvironmentVariable {
            name: "DRASI_TEST_UNUSED_POSTGRES_BOOTSTRAP_SSL_POLICY".into(),
            default: Some("invalid".into()),
        };
        assert!(ssl_mode(&invalid, &mapper).await.is_err());
    }

    #[test]
    fn descriptor_embeds_canonical_typed_config_schemas() {
        let schemas: serde_json::Value =
            serde_json::from_str(&PostgresBootstrapDescriptor.config_schema_json()).unwrap();
        let fields = &schemas["bootstrap.postgres.PostgresBootstrapConfig"]["properties"];
        for name in ["host", "database", "user", "password"] {
            assert_eq!(
                fields[name]["anyOf"],
                serde_json::to_value(ConfigValue::<String>::schema()).unwrap()["anyOf"]
            );
        }
        assert_eq!(
            fields["port"]["anyOf"],
            serde_json::to_value(ConfigValue::<u16>::schema()).unwrap()["anyOf"]
        );
        assert_eq!(
            fields["sslMode"]["anyOf"][0]["enum"],
            serde_json::json!(["disable", "prefer", "require"])
        );
        assert!(!schemas
            .to_string()
            .contains("#/components/schemas/ConfigValue"));
    }
}
