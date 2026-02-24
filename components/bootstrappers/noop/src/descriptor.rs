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

//! Plugin descriptor for the NoOp bootstrap provider.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::NoOpBootstrapProvider;

// ── DTO types ────────────────────────────────────────────────────────────────

/// Empty configuration DTO for the NoOp bootstrap provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = NoOpBootstrapConfig)]
pub struct NoOpBootstrapConfigDto {}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(NoOpBootstrapConfigDto)))]
struct NoOpBootstrapSchemas;

/// Plugin descriptor for the NoOp bootstrap provider.
pub struct NoOpBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for NoOpBootstrapDescriptor {
    fn kind(&self) -> &str {
        "noop"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "NoOpBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = NoOpBootstrapSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    }

    async fn create_bootstrap_provider(
        &self,
        _config_json: &serde_json::Value,
        _source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        Ok(Box::new(NoOpBootstrapProvider::new()))
    }
}
