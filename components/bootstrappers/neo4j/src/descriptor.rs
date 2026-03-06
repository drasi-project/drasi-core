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

//! Plugin descriptor for the Neo4j bootstrap provider.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::{Neo4jBootstrapConfig, Neo4jBootstrapProvider};

// ── DTO types ────────────────────────────────────────────────────────────────

/// Neo4j bootstrap provider configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::neo4j::Neo4jBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Neo4jBootstrapConfigDto {
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

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(Neo4jBootstrapConfigDto,)))]
struct Neo4jBootstrapSchemas;

/// Plugin descriptor for the Neo4j bootstrap provider.
pub struct Neo4jBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for Neo4jBootstrapDescriptor {
    fn kind(&self) -> &str {
        "neo4j"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.neo4j.Neo4jBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = Neo4jBootstrapSchemas::openapi();
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
        let dto: Neo4jBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let config = Neo4jBootstrapConfig {
            uri: mapper.resolve_string(&dto.uri)?,
            user: mapper.resolve_string(&dto.user)?,
            password: mapper.resolve_string(&dto.password)?,
            database: mapper.resolve_string(&dto.database)?,
            labels: dto.labels,
            rel_types: dto.rel_types,
        };

        config.validate()?;
        Ok(Box::new(Neo4jBootstrapProvider::new(config)))
    }
}
