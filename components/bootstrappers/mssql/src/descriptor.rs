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

//! Plugin descriptor for the MS SQL bootstrap provider.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::{AuthMode, EncryptionMode, MsSqlBootstrapProvider, MsSqlSourceConfig, TableKeyConfig};

// ── DTO types ────────────────────────────────────────────────────────────────

/// Authentication mode DTO (mirrors [`AuthMode`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = AuthMode)]
#[serde(rename_all = "lowercase")]
pub enum AuthModeDto {
    SqlServer,
    Windows,
    AzureAd,
}

impl Default for AuthModeDto {
    fn default() -> Self {
        Self::SqlServer
    }
}

/// Encryption mode DTO (mirrors [`EncryptionMode`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = EncryptionMode)]
#[serde(rename_all = "lowercase")]
pub enum EncryptionModeDto {
    Off,
    On,
    NotSupported,
}

impl Default for EncryptionModeDto {
    fn default() -> Self {
        Self::NotSupported
    }
}

/// Table key configuration DTO (mirrors [`TableKeyConfig`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = MsSqlTableKeyConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MsSqlTableKeyConfigDto {
    pub table: String,
    pub key_columns: Vec<String>,
}

fn default_host() -> ConfigValue<String> {
    ConfigValue::Static("localhost".to_string()) // DevSkim: ignore DS162092
}

fn default_port() -> ConfigValue<u16> {
    ConfigValue::Static(1433)
}

fn default_password() -> ConfigValue<String> {
    ConfigValue::Static(String::new())
}

fn default_auth_mode() -> ConfigValue<AuthModeDto> {
    ConfigValue::Static(AuthModeDto::default())
}

fn default_encryption() -> ConfigValue<EncryptionModeDto> {
    ConfigValue::Static(EncryptionModeDto::default())
}

fn default_trust_server_certificate() -> ConfigValue<bool> {
    ConfigValue::Static(false)
}

/// Configuration DTO for the MS SQL bootstrap provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = MsSqlBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MsSqlBootstrapConfigDto {
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

    #[serde(default = "default_auth_mode")]
    #[schema(value_type = ConfigValue<AuthMode>)]
    pub auth_mode: ConfigValue<AuthModeDto>,

    #[serde(default)]
    pub tables: Vec<String>,

    #[serde(default = "default_encryption")]
    #[schema(value_type = ConfigValue<EncryptionMode>)]
    pub encryption: ConfigValue<EncryptionModeDto>,

    #[serde(default = "default_trust_server_certificate")]
    #[schema(value_type = ConfigValueBool)]
    pub trust_server_certificate: ConfigValue<bool>,

    #[serde(default)]
    #[schema(value_type = Vec<MsSqlTableKeyConfig>)]
    pub table_keys: Vec<MsSqlTableKeyConfigDto>,
}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(
    MsSqlBootstrapConfigDto,
    AuthModeDto,
    EncryptionModeDto,
    MsSqlTableKeyConfigDto,
)))]
struct MsSqlBootstrapSchemas;

/// Plugin descriptor for the MS SQL bootstrap provider.
pub struct MsSqlBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for MsSqlBootstrapDescriptor {
    fn kind(&self) -> &str {
        "mssql"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "MsSqlBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = MsSqlBootstrapSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    }

    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        _source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let dto: MsSqlBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let auth_mode = match dto.auth_mode {
            ConfigValue::Static(v) => match v {
                AuthModeDto::SqlServer => AuthMode::SqlServer,
                AuthModeDto::Windows => AuthMode::Windows,
                AuthModeDto::AzureAd => AuthMode::AzureAd,
            },
            _ => AuthMode::default(),
        };

        let encryption = match dto.encryption {
            ConfigValue::Static(v) => match v {
                EncryptionModeDto::Off => EncryptionMode::Off,
                EncryptionModeDto::On => EncryptionMode::On,
                EncryptionModeDto::NotSupported => EncryptionMode::NotSupported,
            },
            _ => EncryptionMode::default(),
        };

        let table_keys = dto
            .table_keys
            .into_iter()
            .map(|tk| TableKeyConfig {
                table: tk.table,
                key_columns: tk.key_columns,
            })
            .collect();

        let config = MsSqlSourceConfig {
            host: mapper.resolve_string(&dto.host)?,
            port: mapper.resolve_typed(&dto.port)?,
            database: mapper.resolve_string(&dto.database)?,
            user: mapper.resolve_string(&dto.user)?,
            password: mapper.resolve_string(&dto.password)?,
            auth_mode,
            tables: dto.tables,
            poll_interval_ms: 1000,
            encryption,
            trust_server_certificate: mapper.resolve_typed(&dto.trust_server_certificate)?,
            table_keys,
            start_position: Default::default(),
        };

        let provider = MsSqlBootstrapProvider::new("mssql-bootstrap", config);
        Ok(Box::new(provider))
    }
}
