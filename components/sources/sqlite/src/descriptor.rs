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

//! SQLite source plugin descriptor and configuration DTOs.

use crate::config::{RestApiConfig, SqliteSourceConfig, TableKeyConfig};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

// ── DTO types ────────────────────────────────────────────────────────────────

/// SQLite source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::sqlite::SqliteSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SqliteSourceConfigDto {
    /// SQLite file path. Omit or set to null for an in-memory database.
    #[serde(default)]
    #[schema(value_type = Option<ConfigValueString>)]
    pub path: Option<ConfigValue<String>>,

    /// Optional explicit table allow-list. Omit or set to null for all user tables.
    #[serde(default)]
    pub tables: Option<Vec<String>>,

    /// Optional explicit key config for element ID generation.
    #[serde(default)]
    #[schema(value_type = Vec<source::sqlite::TableKeyConfig>)]
    pub table_keys: Vec<TableKeyConfigDto>,

    /// Optional REST API configuration. When provided, CRUD endpoints are exposed.
    #[serde(default)]
    #[schema(value_type = Option<source::sqlite::RestApiConfig>)]
    pub rest_api: Option<RestApiConfigDto>,
}

/// REST API configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::sqlite::RestApiConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RestApiConfigDto {
    /// Address to bind (default: "0.0.0.0").
    #[serde(default = "default_rest_host")]
    #[schema(value_type = ConfigValueString)]
    pub host: ConfigValue<String>,

    /// Port to bind (default: 8080).
    #[serde(default = "default_rest_port")]
    #[schema(value_type = ConfigValueU16)]
    pub port: ConfigValue<u16>,
}

/// Table key configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::sqlite::TableKeyConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableKeyConfigDto {
    pub table: String,
    pub key_columns: Vec<String>,
}

fn default_rest_host() -> ConfigValue<String> {
    ConfigValue::Static("0.0.0.0".to_string())
}

fn default_rest_port() -> ConfigValue<u16> {
    ConfigValue::Static(8080)
}

// ── OpenAPI schema ───────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(SqliteSourceConfigDto, RestApiConfigDto, TableKeyConfigDto,)))]
struct SqliteSourceSchemas;

// ── Descriptor ───────────────────────────────────────────────────────────────

/// Plugin descriptor for the SQLite source plugin.
pub struct SqliteSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for SqliteSourceDescriptor {
    fn kind(&self) -> &str {
        "sqlite"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.sqlite.SqliteSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = SqliteSourceSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: SqliteSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let path = match &dto.path {
            Some(cv) => Some(mapper.resolve_string(cv)?),
            None => None,
        };

        let rest_api = match &dto.rest_api {
            Some(rest_dto) => Some(RestApiConfig {
                host: mapper.resolve_string(&rest_dto.host)?,
                port: mapper.resolve_typed(&rest_dto.port)?,
            }),
            None => None,
        };

        let table_keys = dto
            .table_keys
            .iter()
            .map(|tk| TableKeyConfig {
                table: tk.table.clone(),
                key_columns: tk.key_columns.clone(),
            })
            .collect();

        let config = SqliteSourceConfig {
            path,
            tables: dto.tables,
            table_keys,
            rest_api,
            ..Default::default()
        };

        let mut builder = crate::SqliteSourceBuilder::new(id);
        if let Some(p) = &config.path {
            builder = builder.with_path(p);
        }
        if let Some(tables) = config.tables {
            builder = builder.with_tables(tables);
        }
        if !config.table_keys.is_empty() {
            builder = builder.with_table_keys(config.table_keys);
        }
        if let Some(rest) = config.rest_api {
            builder = builder.with_rest_api(rest);
        }
        builder = builder.auto_start(auto_start);

        Ok(Box::new(builder.build()?))
    }
}
