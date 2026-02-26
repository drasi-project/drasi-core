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

//! MS SQL source plugin descriptor and configuration DTOs.

use crate::{AuthMode, EncryptionMode, MsSqlSourceBuilder, StartPosition, TableKeyConfig};
use drasi_plugin_sdk::prelude::*;
use std::str::FromStr;
use utoipa::OpenApi;

/// MS SQL source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = MsSqlSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MsSqlSourceConfigDto {
    #[serde(default = "default_mssql_host")]
    pub host: ConfigValue<String>,
    #[serde(default = "default_mssql_port")]
    pub port: ConfigValue<u16>,
    pub database: ConfigValue<String>,
    pub user: ConfigValue<String>,
    #[serde(default = "default_mssql_password")]
    pub password: ConfigValue<String>,
    #[serde(default)]
    #[schema(value_type = AuthModeDto)]
    pub auth_mode: ConfigValue<AuthModeDto>,
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: ConfigValue<u64>,
    #[serde(default)]
    #[schema(value_type = EncryptionModeDto)]
    pub encryption: ConfigValue<EncryptionModeDto>,
    #[serde(default)]
    pub trust_server_certificate: ConfigValue<bool>,
    #[serde(default)]
    #[schema(value_type = Vec<TableKeyConfigDto>)]
    pub table_keys: Vec<TableKeyConfigDto>,
    #[serde(default)]
    #[schema(value_type = StartPositionDto)]
    pub start_position: ConfigValue<StartPositionDto>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = AuthMode)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum AuthModeDto {
    #[default]
    SqlServer,
    Windows,
    AzureAd,
}

impl FromStr for AuthModeDto {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "sqlserver" | "sql_server" => Ok(AuthModeDto::SqlServer),
            "windows" => Ok(AuthModeDto::Windows),
            "azuread" | "azure_ad" => Ok(AuthModeDto::AzureAd),
            _ => Err(format!("Invalid auth mode: {s}")),
        }
    }
}

impl From<AuthModeDto> for AuthMode {
    fn from(dto: AuthModeDto) -> Self {
        match dto {
            AuthModeDto::SqlServer => AuthMode::SqlServer,
            AuthModeDto::Windows => AuthMode::Windows,
            AuthModeDto::AzureAd => AuthMode::AzureAd,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = EncryptionMode)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum EncryptionModeDto {
    Off,
    On,
    #[default]
    NotSupported,
}

impl FromStr for EncryptionModeDto {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "off" => Ok(EncryptionModeDto::Off),
            "on" => Ok(EncryptionModeDto::On),
            "notsupported" | "not_supported" => Ok(EncryptionModeDto::NotSupported),
            _ => Err(format!("Invalid encryption mode: {s}")),
        }
    }
}

impl From<EncryptionModeDto> for EncryptionMode {
    fn from(dto: EncryptionModeDto) -> Self {
        match dto {
            EncryptionModeDto::Off => EncryptionMode::Off,
            EncryptionModeDto::On => EncryptionMode::On,
            EncryptionModeDto::NotSupported => EncryptionMode::NotSupported,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = StartPosition)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum StartPositionDto {
    Beginning,
    #[default]
    Current,
}

impl FromStr for StartPositionDto {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "beginning" => Ok(StartPositionDto::Beginning),
            "current" => Ok(StartPositionDto::Current),
            _ => Err(format!("Invalid start position: {s}")),
        }
    }
}

impl From<StartPositionDto> for StartPosition {
    fn from(dto: StartPositionDto) -> Self {
        match dto {
            StartPositionDto::Beginning => StartPosition::Beginning,
            StartPositionDto::Current => StartPosition::Current,
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

fn default_mssql_host() -> ConfigValue<String> {
    ConfigValue::Static("localhost".to_string())
}

fn default_mssql_port() -> ConfigValue<u16> {
    ConfigValue::Static(1433)
}

fn default_mssql_password() -> ConfigValue<String> {
    ConfigValue::Static(String::new())
}

fn default_poll_interval_ms() -> ConfigValue<u64> {
    ConfigValue::Static(1000)
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    MsSqlSourceConfigDto,
    AuthModeDto,
    EncryptionModeDto,
    StartPositionDto,
    TableKeyConfigDto,
)))]
struct MsSqlSourceSchemas;

/// Descriptor for the MS SQL source plugin.
pub struct MsSqlSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for MsSqlSourceDescriptor {
    fn kind(&self) -> &str {
        "mssql"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "MsSqlSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = MsSqlSourceSchemas::openapi();
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
        _auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: MsSqlSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let host: String = mapper.resolve_string(&dto.host)?;
        let port: u16 = mapper.resolve_typed(&dto.port)?;
        let database: String = mapper.resolve_string(&dto.database)?;
        let user: String = mapper.resolve_string(&dto.user)?;
        let password: String = mapper.resolve_string(&dto.password)?;
        let auth_mode: AuthMode = mapper.resolve_typed::<AuthModeDto>(&dto.auth_mode)?.into();
        let poll_interval_ms: u64 = mapper.resolve_typed(&dto.poll_interval_ms)?;
        let encryption: EncryptionMode = mapper
            .resolve_typed::<EncryptionModeDto>(&dto.encryption)?
            .into();
        let trust_server_certificate: bool = mapper.resolve_typed(&dto.trust_server_certificate)?;
        let start_position: StartPosition = mapper
            .resolve_typed::<StartPositionDto>(&dto.start_position)?
            .into();

        let mut builder = MsSqlSourceBuilder::new(id)
            .with_host(host)
            .with_port(port)
            .with_database(database)
            .with_user(user)
            .with_password(password)
            .with_auth_mode(auth_mode)
            .with_tables(dto.tables.clone())
            .with_poll_interval_ms(poll_interval_ms)
            .with_encryption(encryption)
            .with_trust_server_certificate(trust_server_certificate)
            .with_start_position(start_position);

        for tk in &dto.table_keys {
            builder = builder.with_table_key(tk.table.clone(), tk.key_columns.clone());
        }

        let source = builder.build()?;
        Ok(Box::new(source))
    }
}
