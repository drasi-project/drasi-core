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

use drasi_kubernetes_common::KubernetesSourceConfig;
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::config::KubernetesBootstrapConfig;
use crate::provider::KubernetesBootstrapProvider;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::kubernetes::KubernetesBootstrapConfig)]
pub struct KubernetesBootstrapConfigDto {}

#[derive(OpenApi)]
#[openapi(components(schemas(KubernetesBootstrapConfigDto)))]
struct KubernetesBootstrapSchemas;

pub struct KubernetesBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for KubernetesBootstrapDescriptor {
    fn kind(&self) -> &str {
        "kubernetes"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.kubernetes.KubernetesBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = KubernetesBootstrapSchemas::openapi();
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
        source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let bootstrap_config: KubernetesBootstrapConfig =
            serde_json::from_value(config_json.clone()).unwrap_or_default();
        let source_config: KubernetesSourceConfig =
            serde_json::from_value(source_config_json.clone())?;
        source_config.validate()?;

        Ok(Box::new(KubernetesBootstrapProvider::new(
            bootstrap_config,
            source_config,
        )))
    }
}
