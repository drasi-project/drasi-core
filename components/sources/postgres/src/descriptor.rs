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

//! PostgreSQL source plugin descriptor and configuration DTOs.

use crate::{PostgresSourceConfig, SslMode, TableKeyConfig};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;
use std::str::FromStr;

/// PostgreSQL source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = PostgresSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PostgresSourceConfigDto {
    #[serde(default = "default_postgres_host")]
    pub host: ConfigValue<String>,
    #[serde(default = "default_postgres_port")]
    pub port: ConfigValue<u16>,
    pub database: ConfigValue<String>,
    pub user: ConfigValue<String>,
    #[serde(default = "default_password")]
    pub password: ConfigValue<String>,
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default = "default_slot_name")]
    pub slot_name: String,
    #[serde(default = "default_publication_name")]
    pub publication_name: String,
    #[serde(default = "default_ssl_mode")]
    #[schema(value_type = ConfigValue<SslMode>)]
    pub ssl_mode: ConfigValue<SslModeDto>,
    #[serde(default)]
    #[schema(value_type = Vec<TableKeyConfig>)]
    pub table_keys: Vec<TableKeyConfigDto>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = SslMode)]
#[serde(rename_all = "lowercase")]
pub enum SslModeDto {
    Disable,
    Prefer,
    Require,
}

impl Default for SslModeDto {
    fn default() -> Self {
        Self::Prefer
    }
}

impl FromStr for SslModeDto {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "disable" => Ok(SslModeDto::Disable),
            "prefer" => Ok(SslModeDto::Prefer),
            "require" => Ok(SslModeDto::Require),
            _ => Err(format!("Invalid SSL mode: {s}")),
        }
    }
}

impl From<SslModeDto> for SslMode {
    fn from(dto: SslModeDto) -> Self {
        match dto {
            SslModeDto::Disable => SslMode::Disable,
            SslModeDto::Prefer => SslMode::Prefer,
            SslModeDto::Require => SslMode::Require,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = TableKeyConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableKeyConfigDto {
    pub table: String,
    pub key_columns: Vec<String>,
}

fn default_postgres_host() -> ConfigValue<String> {
    ConfigValue::Static("localhost".to_string())
}

fn default_postgres_port() -> ConfigValue<u16> {
    ConfigValue::Static(5432)
}

fn default_slot_name() -> String {
    "drasi_slot".to_string()
}

fn default_publication_name() -> String {
    "drasi_publication".to_string()
}

fn default_password() -> ConfigValue<String> {
    ConfigValue::Static(String::new())
}

fn default_ssl_mode() -> ConfigValue<SslModeDto> {
    ConfigValue::Static(SslModeDto::default())
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    PostgresSourceConfigDto,
    SslModeDto,
    TableKeyConfigDto,
)))]
struct PostgresSourceSchemas;

/// Descriptor for the PostgreSQL source plugin.
pub struct PostgresSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for PostgresSourceDescriptor {
    fn kind(&self) -> &str {
        "postgres"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "PostgresSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = PostgresSourceSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: PostgresSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let config = PostgresSourceConfig {
            host: mapper.resolve_string(&dto.host)?,
            port: mapper.resolve_typed(&dto.port)?,
            database: mapper.resolve_string(&dto.database)?,
            user: mapper.resolve_string(&dto.user)?,
            password: mapper.resolve_string(&dto.password)?,
            tables: dto.tables.clone(),
            slot_name: dto.slot_name.clone(),
            publication_name: dto.publication_name.clone(),
            ssl_mode: mapper.resolve_typed::<SslModeDto>(&dto.ssl_mode)?.into(),
            table_keys: dto
                .table_keys
                .iter()
                .map(|tk| TableKeyConfig {
                    table: tk.table.clone(),
                    key_columns: tk.key_columns.clone(),
                })
                .collect(),
        };

        let source = crate::PostgresSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
