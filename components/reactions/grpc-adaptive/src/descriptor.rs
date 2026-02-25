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

//! Descriptor for the gRPC adaptive reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::GrpcAdaptiveReactionBuilder;

/// Configuration DTO for the gRPC adaptive reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = GrpcAdaptiveReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct GrpcAdaptiveReactionConfigDto {
    /// gRPC server endpoint URL.
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,

    /// Request timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_ms: Option<ConfigValue<u64>>,

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

    /// Minimum adaptive batch size.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub adaptive_min_batch_size: Option<ConfigValue<usize>>,

    /// Maximum adaptive batch size.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub adaptive_max_batch_size: Option<ConfigValue<usize>>,

    /// Adaptive window size in 100ms units.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub adaptive_window_size: Option<ConfigValue<usize>>,

    /// Adaptive batch timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub adaptive_batch_timeout_ms: Option<ConfigValue<u64>>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(GrpcAdaptiveReactionConfigDto)))]
struct GrpcAdaptiveReactionSchemas;

/// Descriptor for the gRPC adaptive reaction plugin.
pub struct GrpcAdaptiveReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for GrpcAdaptiveReactionDescriptor {
    fn kind(&self) -> &str {
        "grpc-adaptive"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "GrpcAdaptiveReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = GrpcAdaptiveReactionSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().expect("OpenAPI components missing").schemas).expect("Failed to serialize config schema")
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: GrpcAdaptiveReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = GrpcAdaptiveReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_endpoint(mapper.resolve_string(&dto.endpoint)?);

        if let Some(ref v) = dto.timeout_ms {
            builder = builder.with_timeout_ms(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.max_retries {
            builder = builder.with_max_retries(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.adaptive_min_batch_size {
            builder = builder.with_min_batch_size(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.adaptive_max_batch_size {
            builder = builder.with_max_batch_size(mapper.resolve_typed(v)?);
        }

        for (key, value) in &dto.metadata {
            builder = builder.with_metadata(key, value);
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}
