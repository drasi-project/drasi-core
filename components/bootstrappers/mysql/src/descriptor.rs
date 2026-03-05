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

//! Plugin descriptor for the MySQL bootstrap provider.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::{MySqlBootstrapConfig, MySqlBootstrapProvider, TableKeyConfig};

// ── DTO types ────────────────────────────────────────────────────────────────

/// Table key configuration DTO (mirrors [`TableKeyConfig`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::mysql::MySqlTableKeyConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySqlTableKeyConfigDto {
    pub table: String,
    pub key_columns: Vec<String>,
}

fn default_host() -> ConfigValue<String> {
    ConfigValue::Static("localhost".to_string()) // DevSkim: ignore DS162092
}

fn default_port() -> ConfigValue<u16> {
    ConfigValue::Static(3306)
}

fn default_password() -> ConfigValue<String> {
    ConfigValue::Static(String::new())
}

/// Configuration DTO for the MySQL bootstrap provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::mysql::MySqlBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySqlBootstrapConfigDto {
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

    #[serde(default)]
    #[schema(value_type = Vec<bootstrap::mysql::MySqlTableKeyConfig>)]
    pub table_keys: Vec<MySqlTableKeyConfigDto>,
}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(
    MySqlBootstrapConfigDto,
    MySqlTableKeyConfigDto,
)))]
struct MySqlBootstrapSchemas;

/// Plugin descriptor for the MySQL bootstrap provider.
pub struct MySqlBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for MySqlBootstrapDescriptor {
    fn kind(&self) -> &str {
        "mysql"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.mysql.MySqlBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = MySqlBootstrapSchemas::openapi();
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
        let dto: MySqlBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let table_keys = dto
            .table_keys
            .into_iter()
            .map(|tk| TableKeyConfig {
                table: tk.table,
                key_columns: tk.key_columns,
            })
            .collect();

        let config = MySqlBootstrapConfig {
            host: mapper.resolve_string(&dto.host)?,
            port: mapper.resolve_typed(&dto.port)?,
            database: mapper.resolve_string(&dto.database)?,
            user: mapper.resolve_string(&dto.user)?,
            password: mapper.resolve_string(&dto.password)?,
            tables: dto.tables,
            table_keys,
        };

        let provider = MySqlBootstrapProvider::new(config);
        Ok(Box::new(provider))
    }
}
