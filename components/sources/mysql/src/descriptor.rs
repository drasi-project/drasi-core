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

//! MySQL source plugin descriptor and configuration DTOs.

use crate::{MySqlSourceBuilder, MySqlSourceConfig, SslMode, StartPosition, TableKeyConfig};
use drasi_plugin_sdk::prelude::*;
use std::str::FromStr;
use utoipa::OpenApi;

/// MySQL source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mysql::MySqlSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySqlSourceConfigDto {
    #[serde(default = "default_mysql_host")]
    #[schema(value_type = ConfigValueString)]
    pub host: ConfigValue<String>,

    #[serde(default = "default_mysql_port")]
    #[schema(value_type = ConfigValueU16)]
    pub port: ConfigValue<u16>,

    #[schema(value_type = ConfigValueString)]
    pub database: ConfigValue<String>,

    #[schema(value_type = ConfigValueString)]
    pub user: ConfigValue<String>,

    #[serde(default = "default_mysql_password")]
    #[schema(value_type = ConfigValueString)]
    pub password: ConfigValue<String>,

    #[serde(default)]
    pub tables: Vec<String>,

    #[serde(default)]
    #[schema(value_type = SslModeDto)]
    pub ssl_mode: ConfigValue<SslModeDto>,

    #[serde(default)]
    #[schema(value_type = Vec<source::mysql::TableKeyConfig>)]
    pub table_keys: Vec<TableKeyConfigDto>,

    #[serde(default)]
    #[schema(value_type = StartPositionDto)]
    pub start_position: ConfigValue<StartPositionDto>,

    #[serde(default = "default_server_id")]
    #[schema(value_type = ConfigValueU32)]
    pub server_id: ConfigValue<u32>,

    #[serde(default = "default_heartbeat_interval")]
    #[schema(value_type = ConfigValueU64)]
    pub heartbeat_interval_seconds: ConfigValue<u64>,
}

/// SSL mode DTO (mirrors [`SslMode`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::mysql::SslMode)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SslModeDto {
    #[default]
    Disabled,
    IfAvailable,
    Require,
    RequireVerifyCa,
    RequireVerifyFull,
}

impl FromStr for SslModeDto {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "disabled" => Ok(SslModeDto::Disabled),
            "if_available" | "ifavailable" => Ok(SslModeDto::IfAvailable),
            "require" => Ok(SslModeDto::Require),
            "require_verify_ca" | "requireverifyca" => Ok(SslModeDto::RequireVerifyCa),
            "require_verify_full" | "requireverifyfull" => Ok(SslModeDto::RequireVerifyFull),
            _ => Err(format!("Invalid SSL mode: {s}")),
        }
    }
}

impl From<SslModeDto> for SslMode {
    fn from(dto: SslModeDto) -> Self {
        match dto {
            SslModeDto::Disabled => SslMode::Disabled,
            SslModeDto::IfAvailable => SslMode::IfAvailable,
            SslModeDto::Require => SslMode::Require,
            SslModeDto::RequireVerifyCa => SslMode::RequireVerifyCa,
            SslModeDto::RequireVerifyFull => SslMode::RequireVerifyFull,
        }
    }
}

impl From<&SslMode> for SslModeDto {
    fn from(mode: &SslMode) -> Self {
        match mode {
            SslMode::Disabled => SslModeDto::Disabled,
            SslMode::IfAvailable => SslModeDto::IfAvailable,
            SslMode::Require => SslModeDto::Require,
            SslMode::RequireVerifyCa => SslModeDto::RequireVerifyCa,
            SslMode::RequireVerifyFull => SslModeDto::RequireVerifyFull,
        }
    }
}

/// Start position DTO (mirrors [`StartPosition`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mysql::StartPosition)]
#[serde(tag = "type", rename_all = "snake_case")]
#[derive(Default)]
pub enum StartPositionDto {
    FromStart,
    #[default]
    FromEnd,
    FromPosition {
        file: String,
        position: u32,
    },
    FromGtid(String),
}

impl FromStr for StartPositionDto {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "from_start" | "fromstart" => Ok(StartPositionDto::FromStart),
            "from_end" | "fromend" => Ok(StartPositionDto::FromEnd),
            _ => Err(format!("Invalid start position: {s}")),
        }
    }
}

impl From<StartPositionDto> for StartPosition {
    fn from(dto: StartPositionDto) -> Self {
        match dto {
            StartPositionDto::FromStart => StartPosition::FromStart,
            StartPositionDto::FromEnd => StartPosition::FromEnd,
            StartPositionDto::FromPosition { file, position } => {
                StartPosition::FromPosition { file, position }
            }
            StartPositionDto::FromGtid(gtid) => StartPosition::FromGtid(gtid),
        }
    }
}

impl From<&StartPosition> for StartPositionDto {
    fn from(pos: &StartPosition) -> Self {
        match pos {
            StartPosition::FromStart => StartPositionDto::FromStart,
            StartPosition::FromEnd => StartPositionDto::FromEnd,
            StartPosition::FromPosition { file, position } => StartPositionDto::FromPosition {
                file: file.clone(),
                position: *position,
            },
            StartPosition::FromGtid(gtid) => StartPositionDto::FromGtid(gtid.clone()),
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

impl From<&MySqlSourceConfig> for MySqlSourceConfigDto {
    fn from(config: &MySqlSourceConfig) -> Self {
        Self {
            host: ConfigValue::Static(config.host.clone()),
            port: ConfigValue::Static(config.port),
            database: ConfigValue::Static(config.database.clone()),
            user: ConfigValue::Static(config.user.clone()),
            password: ConfigValue::Static(config.password.clone()),
            tables: config.tables.clone(),
            ssl_mode: ConfigValue::Static(SslModeDto::from(&config.ssl_mode)),
            table_keys: config
                .table_keys
                .iter()
                .map(TableKeyConfigDto::from)
                .collect(),
            start_position: ConfigValue::Static(StartPositionDto::from(&config.start_position)),
            server_id: ConfigValue::Static(config.server_id),
            heartbeat_interval_seconds: ConfigValue::Static(config.heartbeat_interval_seconds),
        }
    }
}

/// Table key configuration DTO (mirrors [`TableKeyConfig`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mysql::TableKeyConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableKeyConfigDto {
    pub table: String,
    pub key_columns: Vec<String>,
}

fn default_mysql_host() -> ConfigValue<String> {
    ConfigValue::Static("localhost".to_string())
}

fn default_mysql_port() -> ConfigValue<u16> {
    ConfigValue::Static(3306)
}

fn default_mysql_password() -> ConfigValue<String> {
    ConfigValue::Static(String::new())
}

fn default_server_id() -> ConfigValue<u32> {
    // 0 is a sentinel meaning "auto-generate from source instance ID"
    ConfigValue::Static(0)
}

fn default_heartbeat_interval() -> ConfigValue<u64> {
    ConfigValue::Static(30)
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    MySqlSourceConfigDto,
    SslModeDto,
    StartPositionDto,
    TableKeyConfigDto,
)))]
struct MySqlSourceSchemas;

/// Descriptor for the MySQL source plugin.
pub struct MySqlSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for MySqlSourceDescriptor {
    fn kind(&self) -> &str {
        "mysql"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.mysql.MySqlSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = MySqlSourceSchemas::openapi();
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
        let dto: MySqlSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let host: String = mapper.resolve_string(&dto.host).await?;
        let port: u16 = mapper.resolve_typed(&dto.port).await?;
        let database: String = mapper.resolve_string(&dto.database).await?;
        let user: String = mapper.resolve_string(&dto.user).await?;
        let password: String = mapper.resolve_string(&dto.password).await?;
        let ssl_mode: SslMode = mapper.resolve_typed::<SslModeDto>(&dto.ssl_mode).await?.into();
        let mut server_id: u32 = mapper.resolve_typed(&dto.server_id).await?;
        if server_id == 0 {
            // Auto-generate a deterministic server_id from the source instance ID.
            // Use lower 31 bits of hash to stay in valid range (1..2^31).
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            id.hash(&mut hasher);
            let hash = hasher.finish();
            server_id = ((hash & 0x7FFF_FFFE) as u32) + 1; // range [1, 2^31-1]
        }
        let heartbeat_interval_seconds: u64 =
            mapper.resolve_typed(&dto.heartbeat_interval_seconds).await?;
        let start_position: StartPosition = mapper
            .resolve_typed::<StartPositionDto>(&dto.start_position)
            .await?
            .into();

        let mut builder = MySqlSourceBuilder::new(id)
            .with_host(host)
            .with_port(port)
            .with_database(database)
            .with_user(user)
            .with_password(password)
            .with_tables(dto.tables.clone())
            .with_ssl_mode(ssl_mode)
            .with_start_position(start_position)
            .with_server_id(server_id)
            .with_heartbeat_interval_seconds(heartbeat_interval_seconds);

        for tk in &dto.table_keys {
            builder = builder.add_table_key(TableKeyConfig {
                table: tk.table.clone(),
                key_columns: tk.key_columns.clone(),
            });
        }

        let mut source = builder.build()?;
        source.base.set_raw_config(config_json.clone());

        Ok(Box::new(source))
    }
}
