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

use crate::{AuthMode, KubernetesSourceBuilder, KubernetesSourceConfig, ResourceSpec, StartFrom};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::kubernetes::ResourceSpec)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceSpecDto {
    pub api_version: String,
    pub kind: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, utoipa::ToSchema)]
#[schema(as = source::kubernetes::AuthMode)]
#[serde(rename_all = "lowercase")]
pub enum AuthModeDto {
    #[default]
    Kubeconfig,
    InCluster,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default, utoipa::ToSchema)]
#[schema(as = source::kubernetes::StartFrom)]
#[serde(tag = "type", content = "timestamp", rename_all = "snake_case")]
pub enum StartFromDto {
    #[default]
    Now,
    Beginning,
    Timestamp(i64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::kubernetes::KubernetesSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KubernetesSourceConfigDto {
    pub resources: Vec<ResourceSpecDto>,
    #[serde(default)]
    pub namespaces: Vec<String>,
    #[serde(default)]
    pub label_selector: Option<String>,
    #[serde(default)]
    pub field_selector: Option<String>,
    #[serde(default)]
    pub auth_mode: AuthModeDto,
    #[serde(default)]
    pub kubeconfig_path: Option<String>,
    #[serde(default)]
    pub kubeconfig_content: Option<String>,
    #[serde(default)]
    pub include_owner_relations: bool,
    #[serde(default)]
    pub start_from: StartFromDto,
    #[serde(default)]
    pub exclude_annotations: Vec<String>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    KubernetesSourceConfigDto,
    ResourceSpecDto,
    AuthModeDto,
    StartFromDto,
)))]
struct KubernetesSourceSchemas;

pub struct KubernetesSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for KubernetesSourceDescriptor {
    fn kind(&self) -> &str {
        "kubernetes"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.kubernetes.KubernetesSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = KubernetesSourceSchemas::openapi();
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
        let dto: KubernetesSourceConfigDto = serde_json::from_value(config_json.clone())?;

        let resources = dto
            .resources
            .iter()
            .map(|r| ResourceSpec {
                api_version: r.api_version.clone(),
                kind: r.kind.clone(),
            })
            .collect::<Vec<_>>();

        let auth_mode = match dto.auth_mode {
            AuthModeDto::Kubeconfig => AuthMode::Kubeconfig,
            AuthModeDto::InCluster => AuthMode::InCluster,
        };

        let start_from = match dto.start_from {
            StartFromDto::Now => StartFrom::Now,
            StartFromDto::Beginning => StartFrom::Beginning,
            StartFromDto::Timestamp(ts) => StartFrom::Timestamp(ts),
        };

        let config = KubernetesSourceConfig {
            resources,
            namespaces: dto.namespaces,
            label_selector: dto.label_selector,
            field_selector: dto.field_selector,
            auth_mode,
            kubeconfig_path: dto.kubeconfig_path,
            kubeconfig_content: dto.kubeconfig_content,
            include_owner_relations: dto.include_owner_relations,
            start_from,
            exclude_annotations: if dto.exclude_annotations.is_empty() {
                crate::default_annotation_excludes()
            } else {
                dto.exclude_annotations
            },
        };
        config.validate()?;

        let source = KubernetesSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
