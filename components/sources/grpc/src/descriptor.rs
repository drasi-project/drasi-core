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

//! gRPC source plugin descriptor and configuration DTOs.

use crate::{GrpcSourceBuilder, GrpcSourceConfig};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// gRPC source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::grpc::GrpcSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcSourceConfigDto {
    #[serde(default = "default_grpc_host")]
    pub host: ConfigValue<String>,
    #[serde(default = "default_grpc_port")]
    pub port: ConfigValue<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<ConfigValue<String>>,
    #[serde(default = "default_grpc_timeout_ms")]
    pub timeout_ms: ConfigValue<u64>,
}

fn default_grpc_host() -> ConfigValue<String> {
    ConfigValue::Static("0.0.0.0".to_string())
}

fn default_grpc_port() -> ConfigValue<u16> {
    ConfigValue::Static(50051)
}

fn default_grpc_timeout_ms() -> ConfigValue<u64> {
    ConfigValue::Static(5000)
}

#[derive(OpenApi)]
#[openapi(components(schemas(GrpcSourceConfigDto)))]
struct GrpcSourceSchemas;

/// Descriptor for the gRPC source plugin.
pub struct GrpcSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for GrpcSourceDescriptor {
    fn kind(&self) -> &str {
        "grpc"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.grpc.GrpcSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = GrpcSourceSchemas::openapi();
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
        let dto: GrpcSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let config = GrpcSourceConfig {
            host: mapper.resolve_string(&dto.host)?,
            port: mapper.resolve_typed(&dto.port)?,
            endpoint: mapper.resolve_optional(&dto.endpoint)?,
            timeout_ms: mapper.resolve_typed(&dto.timeout_ms)?,
        };

        let source = GrpcSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
