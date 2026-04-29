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

//! SQLite bootstrap plugin descriptor and configuration DTOs.

use crate::{SqliteBootstrapProvider, TableKeyConfig};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

// ── DTO types ────────────────────────────────────────────────────────────────

/// SQLite bootstrap configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::sqlite::SqliteBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SqliteBootstrapConfigDto {
    /// SQLite file path. Omit or set to null for an in-memory database (bootstrap will be skipped).
    #[serde(default)]
    #[schema(value_type = Option<ConfigValueString>)]
    pub path: Option<ConfigValue<String>>,

    /// Optional table allow-list. Omit or set to null for all user tables.
    #[serde(default)]
    pub tables: Option<Vec<String>>,

    /// Optional explicit key config for element ID generation.
    #[serde(default)]
    #[schema(value_type = Vec<bootstrap::sqlite::TableKeyConfig>)]
    pub table_keys: Vec<TableKeyConfigDto>,
}

/// Table key configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::sqlite::TableKeyConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableKeyConfigDto {
    pub table: String,
    pub key_columns: Vec<String>,
}

// ── OpenAPI schema ───────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(SqliteBootstrapConfigDto, TableKeyConfigDto,)))]
struct SqliteBootstrapSchemas;

// ── Descriptor ───────────────────────────────────────────────────────────────

/// Plugin descriptor for the SQLite bootstrap provider.
pub struct SqliteBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for SqliteBootstrapDescriptor {
    fn kind(&self) -> &str {
        "sqlite"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.sqlite.SqliteBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = SqliteBootstrapSchemas::openapi();
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
    ) -> anyhow::Result<Box<dyn drasi_lib::bootstrap::BootstrapProvider>> {
        let dto: SqliteBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = SqliteBootstrapProvider::builder();

        if let Some(path_cv) = &dto.path {
            builder = builder.with_path(mapper.resolve_string(path_cv)?);
        }

        if let Some(tables) = dto.tables {
            builder = builder.with_tables(tables);
        }

        let table_keys = dto
            .table_keys
            .into_iter()
            .map(|tk| TableKeyConfig {
                table: tk.table,
                key_columns: tk.key_columns,
            })
            .collect::<Vec<_>>();

        if !table_keys.is_empty() {
            builder = builder.with_table_keys(table_keys);
        }

        Ok(Box::new(builder.build()))
    }
}
