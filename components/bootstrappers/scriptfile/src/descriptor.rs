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

//! Plugin descriptor for the ScriptFile bootstrap provider.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::ScriptFileBootstrapConfig;
use crate::ScriptFileBootstrapProvider;

// ── DTO types ────────────────────────────────────────────────────────────────

/// Configuration DTO for the ScriptFile bootstrap provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = ScriptFileBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScriptFileBootstrapConfigDto {
    #[serde(default)]
    pub file_paths: Vec<String>,
}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(ScriptFileBootstrapConfigDto)))]
struct ScriptFileBootstrapSchemas;

/// Plugin descriptor for the ScriptFile bootstrap provider.
pub struct ScriptFileBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for ScriptFileBootstrapDescriptor {
    fn kind(&self) -> &str {
        "scriptfile"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "ScriptFileBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = ScriptFileBootstrapSchemas::openapi();
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
        let dto: ScriptFileBootstrapConfigDto = serde_json::from_value(config_json.clone())?;

        let config = ScriptFileBootstrapConfig {
            file_paths: dto.file_paths,
        };

        Ok(Box::new(ScriptFileBootstrapProvider::new(config)))
    }
}
