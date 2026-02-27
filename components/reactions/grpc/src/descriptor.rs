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

//! Descriptor for the gRPC reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::GrpcReactionBuilder;

/// Configuration DTO for the gRPC reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::grpc::GrpcReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct GrpcReactionConfigDto {
    /// gRPC server endpoint URL.
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,

    /// Request timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_ms: Option<ConfigValue<u64>>,

    /// Batch size for bundling events.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub batch_size: Option<ConfigValue<usize>>,

    /// Batch flush timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub batch_flush_timeout_ms: Option<ConfigValue<u64>>,

    /// Maximum retries for failed requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU32>)]
    pub max_retries: Option<ConfigValue<u32>>,

    /// Connection retry attempts.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU32>)]
    pub connection_retry_attempts: Option<ConfigValue<u32>>,

    /// Initial connection timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub initial_connection_timeout_ms: Option<ConfigValue<u64>>,

    /// Metadata headers to include in requests.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(GrpcReactionConfigDto)))]
struct GrpcReactionSchemas;

/// Descriptor for the gRPC reaction plugin.
pub struct GrpcReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for GrpcReactionDescriptor {
    fn kind(&self) -> &str {
        "grpc"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.grpc.GrpcReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = GrpcReactionSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: GrpcReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = GrpcReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_endpoint(mapper.resolve_string(&dto.endpoint)?);

        if let Some(ref v) = dto.timeout_ms {
            builder = builder.with_timeout_ms(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.batch_size {
            builder = builder.with_batch_size(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.batch_flush_timeout_ms {
            builder = builder.with_batch_flush_timeout_ms(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.max_retries {
            builder = builder.with_max_retries(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.connection_retry_attempts {
            builder = builder.with_connection_retry_attempts(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.initial_connection_timeout_ms {
            builder = builder.with_initial_connection_timeout_ms(mapper.resolve_typed(v)?);
        }

        for (key, value) in &dto.metadata {
            builder = builder.with_metadata(key, value);
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}
