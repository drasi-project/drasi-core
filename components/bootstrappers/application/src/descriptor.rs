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

//! Plugin descriptor for the Application bootstrap provider.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::ApplicationBootstrapProvider;

// ── DTO types ────────────────────────────────────────────────────────────────

/// Empty configuration DTO for the Application bootstrap provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::application::ApplicationBootstrapConfig)]
pub struct ApplicationBootstrapConfigDto {}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(ApplicationBootstrapConfigDto)))]
struct ApplicationBootstrapSchemas;

/// Plugin descriptor for the Application bootstrap provider.
pub struct ApplicationBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for ApplicationBootstrapDescriptor {
    fn kind(&self) -> &str {
        "application"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.application.ApplicationBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = ApplicationBootstrapSchemas::openapi();
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
        _config_json: &serde_json::Value,
        _source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        Ok(Box::new(ApplicationBootstrapProvider::new()))
    }
}
