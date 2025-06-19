// Copyright 2024 The Drasi Authors.
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

use std::sync::Arc;

use drasi_core::models::SourceMiddlewareConfig;
use serde_json::Value;

pub fn promote_observer_query() -> &'static str {
    "
    MATCH (n:TestNode)
    RETURN
        n.id as id,
        n.original_prop as original_prop,
        n.nested as nested,
        n.promoted_value as promoted_value,
        n.another_promoted as another_promoted
    "
}

pub fn create_promote_middleware(
    name: &str,
    mappings: Value,
    on_conflict: Option<&str>,
    on_error: Option<&str>,
) -> Arc<SourceMiddlewareConfig> {
    let mut config_map = serde_json::Map::new();
    config_map.insert("mappings".to_string(), mappings);

    if let Some(conflict_strategy) = on_conflict {
        config_map.insert(
            "on_conflict".to_string(),
            Value::String(conflict_strategy.to_string()),
        );
    }
    if let Some(error_handling) = on_error {
        config_map.insert(
            "on_error".to_string(),
            Value::String(error_handling.to_string()),
        );
    }

    Arc::new(SourceMiddlewareConfig::new("promote", name, config_map))
}

/// Defines the pipeline for the source.
pub fn source_pipeline(middleware_name: &str) -> Vec<String> {
    vec![middleware_name.to_string()]
}
