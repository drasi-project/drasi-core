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
    #[schema(value_type = ConfigValueString)]
    pub host: ConfigValue<String>,

    #[serde(default = "default_port")]
    #[schema(value_type = ConfigValueU16)]
    pub port: ConfigValue<u16>,

    #[schema(value_type = ConfigValueString)]
    pub database: ConfigValue<String>,

    #[schema(value_type = ConfigValueString)]
    pub user: ConfigValue<String>,

    #[serde(default = "default_password")]
    #[schema(value_type = ConfigValueString)]
    pub password: ConfigValue<String>,

    #[serde(default)]
    pub tables: Vec<String>,

    #[serde(default = "default_slot_name")]
    pub slot_name: String,

    #[serde(default = "default_publication_name")]
    pub publication_name: String,

    #[serde(default = "default_ssl_mode")]
    #[schema(value_type = ConfigValue<bootstrap::postgres::SslMode>)]
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

        let ssl_mode = match dto.ssl_mode {
            ConfigValue::Static(v) => match v {
                SslModeDto::Disable => SslMode::Disable,
                SslModeDto::Prefer => SslMode::Prefer,
                SslModeDto::Require => SslMode::Require,
            },
            _ => SslMode::default(),
        };

        let table_keys = dto
            .table_keys
            .into_iter()
            .map(|tk| TableKeyConfig {
                table: tk.table,
                key_columns: tk.key_columns,
            })
            .collect();

        let config = PostgresBootstrapConfig {
            host: mapper.resolve_string(&dto.host)?,
            port: mapper.resolve_typed(&dto.port)?,
            database: mapper.resolve_string(&dto.database)?,
            user: mapper.resolve_string(&dto.user)?,
            password: mapper.resolve_string(&dto.password)?,
            tables: dto.tables,
            slot_name: dto.slot_name,
            publication_name: dto.publication_name,
            ssl_mode,
            table_keys,
        };

        config.validate()?;
        Ok(Box::new(PostgresBootstrapProvider::new(config)))
    }
}
