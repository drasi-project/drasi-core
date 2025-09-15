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
use serde_json::{Map as JsonMap, Value as JsonValue};

use super::{ELEMENT_LABEL, PARSED_DATA_PROP, RAW_DATA_PROP};

pub fn observer_query() -> String {
    format!(
        "
        MATCH (n:{ELEMENT_LABEL})
        RETURN
            n.`{RAW_DATA_PROP}` as rawData,        // Original (overwritten by decoder)
            n.`{PARSED_DATA_PROP}` as parsedData,    // Parsed structure from parse_json
            // --- Promoted fields from various tests ---
            n.userId as userId,
            n.orderTotal as orderTotal,
            n.orderStatus as orderStatus,
            n.promotedValue as promotedValue,
            n.promotedTag as promotedTag
        "
    )
}

/// Decoder middleware configuration with encoding_type=base64, strip_quotes=true, target=RAW_DATA_PROP.
pub fn create_decoder_config(name: &str, on_error: Option<&str>) -> Arc<SourceMiddlewareConfig> {
    let mut config_map = JsonMap::new();
    config_map.insert(
        "encoding_type".to_string(),
        JsonValue::String("base64".to_string()),
    );
    config_map.insert(
        "target_property".to_string(),
        JsonValue::String(RAW_DATA_PROP.to_string()),
    );

    config_map.insert("strip_quotes".to_string(), JsonValue::Bool(true));
    if let Some(err_strategy) = on_error {
        config_map.insert(
            "on_error".to_string(),
            JsonValue::String(err_strategy.to_string()),
        );
    }

    Arc::new(SourceMiddlewareConfig::new("decoder", name, config_map))
}

/// Config for parse_json middleware target=RAW_DATA_PROP (output of decoder), output=PARSED_DATA_PROP.
pub fn create_parser_config(name: &str, on_error: Option<&str>) -> Arc<SourceMiddlewareConfig> {
    let mut config_map = JsonMap::new();
    config_map.insert(
        "target_property".to_string(),
        JsonValue::String(RAW_DATA_PROP.to_string()), // Operates on the output of the decoder
    );
    config_map.insert(
        "output_property".to_string(),
        JsonValue::String(PARSED_DATA_PROP.to_string()), // Store parsed data separately
    );
    if let Some(err_strategy) = on_error {
        config_map.insert(
            "on_error".to_string(),
            JsonValue::String(err_strategy.to_string()),
        );
    }

    Arc::new(SourceMiddlewareConfig::new("parse_json", name, config_map))
}

/// Config for promote middleware. It promotes fields from PARSED_DATA_PROP to the root node.
pub fn create_promoter_config(
    name: &str,
    mappings: JsonValue,
    on_conflict: Option<&str>,
    on_error: Option<&str>,
) -> Arc<SourceMiddlewareConfig> {
    let mut config_map = JsonMap::new();
    config_map.insert("mappings".to_string(), mappings);

    if let Some(conflict_strategyegy) = on_conflict {
        config_map.insert(
            "on_conflict".to_string(),
            JsonValue::String(conflict_strategyegy.to_string()),
        );
    }
    if let Some(error_handling) = on_error {
        config_map.insert(
            "on_error".to_string(),
            JsonValue::String(error_handling.to_string()),
        );
    }

    Arc::new(SourceMiddlewareConfig::new("promote", name, config_map))
}

/// Defines the middleware pipeline order.
pub fn create_pipeline(decoder_name: &str, parser_name: &str, promoter_name: &str) -> Vec<String> {
    vec![
        decoder_name.to_string(),
        parser_name.to_string(),
        promoter_name.to_string(),
    ]
}
