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

pub fn parse_json_observer_query() -> &'static str {
    "
    MATCH (n:TestNode)
    RETURN
        n.id as id,
        n.target_prop as target_prop,
        n.output_prop as output_prop,
        n.other_prop as other_prop
    "
}

pub fn create_parse_json_middleware(
    name: &str,
    target_property: &str,
    output_property: Option<&str>,
    on_error: Option<&str>,
    max_json_size: Option<usize>,
    max_nesting_depth: Option<usize>,
) -> Arc<SourceMiddlewareConfig> {
    let mut config_map = serde_json::Map::new();
    config_map.insert(
        "target_property".to_string(),
        Value::String(target_property.to_string()),
    );
    if let Some(out_prop) = output_property {
        config_map.insert(
            "output_property".to_string(),
            Value::String(out_prop.to_string()),
        );
    }
    if let Some(err_handling) = on_error {
        config_map.insert(
            "on_error".to_string(),
            Value::String(err_handling.to_string()),
        );
    }
    if let Some(max_size) = max_json_size {
        config_map.insert("max_json_size".to_string(), Value::Number(max_size.into()));
    }
    if let Some(max_depth) = max_nesting_depth {
        config_map.insert(
            "max_nesting_depth".to_string(),
            Value::Number(max_depth.into()),
        );
    }

    Arc::new(SourceMiddlewareConfig::new("parse_json", name, config_map))
}

pub fn source_pipeline(middleware_name: &str) -> Vec<String> {
    vec![middleware_name.to_string()]
}
