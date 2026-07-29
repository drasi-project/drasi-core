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

//! Plugin descriptor for the state machine source.
//!
//! A single plugin crate exports one **source** descriptor (kind `state-machine`).
//! The source config carries the `states` (with their `enter` conditions); the
//! set of queries it subscribes to is derived from those conditions. Downstream
//! queries subscribe to the source by its declared id.

use drasi_lib::Source;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::config::{EnterCondition, Op, StateDef, StateMachineSourceConfig};
use crate::StateMachineSource;

#[derive(OpenApi)]
#[openapi(components(schemas(StateMachineSourceConfig, StateDef, EnterCondition, Op)))]
struct StateMachineSourceSchemas;

/// Descriptor for the state machine source plugin.
pub struct StateMachineSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for StateMachineSourceDescriptor {
    fn kind(&self) -> &str {
        "state-machine"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.state_machine.StateMachineSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = StateMachineSourceSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    fn display_name(&self) -> &str {
        "State Machine"
    }

    fn display_description(&self) -> &str {
        "Maps continuous-query results to entity state transitions and exposes live entity state as a source"
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Source>> {
        let config: StateMachineSourceConfig = serde_json::from_value(config_json.clone())?;
        let source = StateMachineSource::create(id.to_string(), config, None, auto_start)?;
        Ok(Box::new(source))
    }
}
