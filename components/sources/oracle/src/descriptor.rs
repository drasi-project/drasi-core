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

//! Oracle source plugin descriptor.

use crate::{OracleSourceBuilder, OracleSourceConfig, SslMode, StartPosition, TableKeyConfig};
use drasi_plugin_sdk::prelude::*;
use std::str::FromStr;
use utoipa::OpenApi;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::oracle::OracleSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OracleSourceConfigDto {
    #[serde(default = "default_host")]
    pub host: ConfigValue<String>,
    #[serde(default = "default_port")]
    pub port: ConfigValue<u16>,
    #[serde(default = "default_service")]
    pub service: ConfigValue<String>,
    pub user: ConfigValue<String>,
    #[serde(default = "default_password")]
    pub password: ConfigValue<String>,
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: ConfigValue<u64>,
    #[serde(default)]
    #[schema(value_type = StartPositionDto)]
    pub start_position: ConfigValue<StartPositionDto>,
    #[serde(default)]
    #[schema(value_type = SslModeDto)]
    pub ssl_mode: ConfigValue<SslModeDto>,
    #[serde(default)]
    #[schema(value_type = Vec<TableKeyConfigDto>)]
    pub table_keys: Vec<TableKeyConfigDto>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::oracle::StartPosition)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum StartPositionDto {
    Beginning,
    #[default]
    Current,
}

impl From<StartPositionDto> for StartPosition {
    fn from(value: StartPositionDto) -> Self {
        match value {
            StartPositionDto::Beginning => StartPosition::Beginning,
            StartPositionDto::Current => StartPosition::Current,
        }
    }
}

impl FromStr for StartPositionDto {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_lowercase().as_str() {
            "beginning" => Ok(Self::Beginning),
            "current" => Ok(Self::Current),
            _ => Err(format!("Invalid Oracle start position: {value}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::oracle::SslMode)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum SslModeDto {
    #[default]
    Disable,
    Require,
}

impl From<SslModeDto> for SslMode {
    fn from(value: SslModeDto) -> Self {
        match value {
            SslModeDto::Disable => SslMode::Disable,
            SslModeDto::Require => SslMode::Require,
        }
    }
}

impl FromStr for SslModeDto {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_lowercase().as_str() {
            "disable" => Ok(Self::Disable),
            "require" => Ok(Self::Require),
            _ => Err(format!("Invalid Oracle SSL mode: {value}")),
        }
    }
}

impl From<&OracleSourceConfig> for OracleSourceConfigDto {
    fn from(config: &OracleSourceConfig) -> Self {
        Self {
            host: ConfigValue::Static(config.host.clone()),
            port: ConfigValue::Static(config.port),
            service: ConfigValue::Static(config.database.clone()),
            user: ConfigValue::Static(config.user.clone()),
            password: ConfigValue::Static(config.password.clone()),
            tables: config.tables.clone(),
            poll_interval_ms: ConfigValue::Static(config.poll_interval_ms),
            start_position: ConfigValue::Static(StartPositionDto::from(&config.start_position)),
            ssl_mode: ConfigValue::Static(SslModeDto::from(&config.ssl_mode)),
            table_keys: config
                .table_keys
                .iter()
                .map(TableKeyConfigDto::from)
                .collect(),
        }
    }
}

impl From<&StartPosition> for StartPositionDto {
    fn from(value: &StartPosition) -> Self {
        match value {
            StartPosition::Beginning => Self::Beginning,
            StartPosition::Current => Self::Current,
        }
    }
}

impl From<&SslMode> for SslModeDto {
    fn from(value: &SslMode) -> Self {
        match value {
            SslMode::Disable => Self::Disable,
            SslMode::Require => Self::Require,
        }
    }
}

impl From<&TableKeyConfig> for TableKeyConfigDto {
    fn from(tk: &TableKeyConfig) -> Self {
        Self {
            table: tk.table.clone(),
            key_columns: tk.key_columns.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::oracle::TableKeyConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableKeyConfigDto {
    pub table: String,
    pub key_columns: Vec<String>,
}

fn default_host() -> ConfigValue<String> {
    ConfigValue::Static("localhost".to_string())
}

fn default_port() -> ConfigValue<u16> {
    ConfigValue::Static(1521)
}

fn default_service() -> ConfigValue<String> {
    ConfigValue::Static("FREEPDB1".to_string())
}

fn default_password() -> ConfigValue<String> {
    ConfigValue::Static(String::new())
}

fn default_poll_interval_ms() -> ConfigValue<u64> {
    ConfigValue::Static(1000)
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    OracleSourceConfigDto,
    StartPositionDto,
    SslModeDto,
    TableKeyConfigDto,
)))]
struct OracleSourceSchemas;

pub struct OracleSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for OracleSourceDescriptor {
    fn kind(&self) -> &str {
        "oracle"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.oracle.OracleSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = OracleSourceSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize Oracle config schema")
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: OracleSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = OracleSourceBuilder::new(id)
            .with_host(mapper.resolve_string(&dto.host).await?)
            .with_port(mapper.resolve_typed(&dto.port).await?)
            .with_service(mapper.resolve_string(&dto.service).await?)
            .with_user(mapper.resolve_string(&dto.user).await?)
            .with_password(mapper.resolve_string(&dto.password).await?)
            .with_tables(dto.tables)
            .with_poll_interval_ms(mapper.resolve_typed(&dto.poll_interval_ms).await?)
            .with_start_position(
                mapper
                    .resolve_typed::<StartPositionDto>(&dto.start_position)
                    .await?
                    .into(),
            )
            .with_ssl_mode(
                mapper
                    .resolve_typed::<SslModeDto>(&dto.ssl_mode)
                    .await?
                    .into(),
            )
            .with_auto_start(auto_start);

        for table_key in dto.table_keys {
            builder = builder.with_table_key(table_key.table, table_key.key_columns);
        }

        let mut source = builder.build()?;
        source.base.set_raw_config(config_json.clone());

        Ok(Box::new(source))
    }
}
