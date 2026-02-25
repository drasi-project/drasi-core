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

//! Descriptor for the profiler reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::ProfilerReactionBuilder;

/// Configuration DTO for the profiler reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = ProfilerReactionConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ProfilerReactionConfigDto {
    /// Window size for profiling statistics.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub window_size: Option<ConfigValue<usize>>,

    /// Report interval in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub report_interval_secs: Option<ConfigValue<u64>>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(ProfilerReactionConfigDto)))]
struct ProfilerReactionSchemas;

/// Descriptor for the profiler reaction plugin.
pub struct ProfilerReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for ProfilerReactionDescriptor {
    fn kind(&self) -> &str {
        "profiler"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "ProfilerReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = ProfilerReactionSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: ProfilerReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = ProfilerReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start);

        if let Some(ref v) = dto.window_size {
            builder = builder.with_window_size(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.report_interval_secs {
            builder = builder.with_report_interval_secs(mapper.resolve_typed(v)?);
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}
