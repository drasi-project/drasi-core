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
use serde_json::{json, Value};

pub fn decoder_query() -> &'static str {
    "
    MATCH (n:TargetNode)
    RETURN
        n.id as id,
        n.encoded_data as encoded_data,
        n.decoded_data as decoded_data,
        n.other_prop as other_prop
    "
}

/// Creates a decoder middleware configuration.
pub fn create_decoder_middleware(
    name: &str,
    encoding_type: &str,
    target_property: &str,
    output_property: Option<&str>,
    strip_quotes: bool,
    on_error: &str,
    max_size_bytes: Option<usize>,
) -> Arc<SourceMiddlewareConfig> {
    let mut config_map = serde_json::Map::new();
    config_map.insert(
        "encoding_type".to_string(),
        Value::String(encoding_type.to_string()),
    );
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
    config_map.insert("strip_quotes".to_string(), Value::Bool(strip_quotes));
    config_map.insert("on_error".to_string(), Value::String(on_error.to_string()));
    if let Some(max_size) = max_size_bytes {
        config_map.insert("max_size_bytes".to_string(), json!(max_size));
    }

    Arc::new(SourceMiddlewareConfig::new("decoder", name, config_map))
}

pub fn source_pipeline(middleware_name: &str) -> Vec<String> {
    vec![middleware_name.to_string()]
}
