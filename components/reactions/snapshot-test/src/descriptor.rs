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

use async_trait::async_trait;
use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::descriptor::ReactionPluginDescriptor;
use utoipa::OpenApi;

use crate::{SnapshotTestConfig, SnapshotTestReaction};

#[derive(OpenApi)]
#[openapi(components(schemas(SnapshotTestConfig)))]
struct SnapshotTestSchemas;

pub struct SnapshotTestReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for SnapshotTestReactionDescriptor {
    fn kind(&self) -> &str {
        "snapshot-test"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.snapshot_test.SnapshotTestConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = SnapshotTestSchemas::openapi();
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
        _config_json: &serde_json::Value,
        _auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let (reaction, _report) = SnapshotTestReaction::new(id, query_ids);
        Ok(Box::new(reaction))
    }
}
