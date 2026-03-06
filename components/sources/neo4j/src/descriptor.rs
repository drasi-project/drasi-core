// Copyright 2026 The Drasi Authors.
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

//! Neo4j source plugin descriptor and configuration DTOs.

use crate::{CdcMode, Neo4jSourceConfig, StartCursor};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// Neo4j CDC source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::neo4j::Neo4jSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Neo4jSourceConfigDto {
    #[serde(default = "default_uri")]
    #[schema(value_type = ConfigValueString)]
    pub uri: ConfigValue<String>,

    #[serde(default = "default_user")]
    #[schema(value_type = ConfigValueString)]
    pub user: ConfigValue<String>,

    #[serde(default = "default_password")]
    #[schema(value_type = ConfigValueString)]
    pub password: ConfigValue<String>,

    #[serde(default = "default_database")]
    #[schema(value_type = ConfigValueString)]
    pub database: ConfigValue<String>,

    #[serde(default)]
    pub labels: Vec<String>,

    #[serde(default)]
    pub rel_types: Vec<String>,

    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,

    #[serde(default)]
    #[schema(value_type = source::neo4j::CdcMode)]
    pub cdc_mode: CdcModeDto,

    #[serde(default)]
    #[schema(value_type = source::neo4j::StartCursor)]
    pub start_cursor: StartCursorDto,
}

/// Neo4j CDC enrichment mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::neo4j::CdcMode)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum CdcModeDto {
    Diff,
    #[default]
    Full,
}

impl From<CdcModeDto> for CdcMode {
    fn from(dto: CdcModeDto) -> Self {
        match dto {
            CdcModeDto::Diff => CdcMode::Diff,
            CdcModeDto::Full => CdcMode::Full,
        }
    }
}

/// Startup behavior for CDC cursor initialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::neo4j::StartCursor)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
#[derive(Default)]
pub enum StartCursorDto {
    Beginning,
    #[default]
    Now,
    Timestamp(i64),
}

impl From<StartCursorDto> for StartCursor {
    fn from(dto: StartCursorDto) -> Self {
        match dto {
            StartCursorDto::Beginning => StartCursor::Beginning,
            StartCursorDto::Now => StartCursor::Now,
            StartCursorDto::Timestamp(ts) => StartCursor::Timestamp(ts),
        }
    }
}

fn default_uri() -> ConfigValue<String> {
    ConfigValue::Static("bolt://localhost:7687".to_string())
}

fn default_user() -> ConfigValue<String> {
    ConfigValue::Static("neo4j".to_string())
}

fn default_password() -> ConfigValue<String> {
    ConfigValue::Static(String::new())
}

fn default_database() -> ConfigValue<String> {
    ConfigValue::Static("neo4j".to_string())
}

fn default_poll_interval_ms() -> u64 {
    500
}

#[derive(OpenApi)]
#[openapi(components(schemas(Neo4jSourceConfigDto, CdcModeDto, StartCursorDto,)))]
struct Neo4jSourceSchemas;

/// Descriptor for the Neo4j source plugin.
pub struct Neo4jSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for Neo4jSourceDescriptor {
    fn kind(&self) -> &str {
        "neo4j"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.neo4j.Neo4jSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = Neo4jSourceSchemas::openapi();
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
        let dto: Neo4jSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let config = Neo4jSourceConfig {
            uri: mapper.resolve_string(&dto.uri)?,
            user: mapper.resolve_string(&dto.user)?,
            password: mapper.resolve_string(&dto.password)?,
            database: mapper.resolve_string(&dto.database)?,
            labels: dto.labels,
            rel_types: dto.rel_types,
            poll_interval_ms: dto.poll_interval_ms,
            cdc_mode: dto.cdc_mode.into(),
            start_cursor: dto.start_cursor.into(),
        };

        let source = crate::Neo4jSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
