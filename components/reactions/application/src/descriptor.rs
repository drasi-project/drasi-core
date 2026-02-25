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

//! Descriptor for the application reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::ApplicationReactionBuilder;

/// Configuration DTO for the application reaction plugin.
///
/// The application reaction is primarily used for programmatic in-process
/// subscriptions and has minimal configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = ApplicationReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationReactionConfigDto {}

#[derive(OpenApi)]
#[openapi(components(schemas(ApplicationReactionConfigDto)))]
struct ApplicationReactionSchemas;

/// Descriptor for the application reaction plugin.
pub struct ApplicationReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for ApplicationReactionDescriptor {
    fn kind(&self) -> &str {
        "application"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "ApplicationReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = ApplicationReactionSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        _config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let (reaction, _handle) = ApplicationReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .build();

        Ok(Box::new(reaction))
    }
}
