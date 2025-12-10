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

use crate::common::ErrorHandling;
use async_trait::async_trait;
use base64::Engine;
use drasi_core::{
    interface::{
        ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    models::{Element, ElementValue, SourceChange, SourceMiddlewareConfig},
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

#[cfg(test)]
mod tests;

/// Specifies the encoding type of the target property's string value.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EncodingType {
    Base64,
    Base64url,
    Hex,
    Url,
    JsonEscape,
}

/// Mapping of encoding types to their string representations for error messages
const ENCODING_TYPE_NAMES: &[(&str, EncodingType)] = &[
    ("base64", EncodingType::Base64),
    ("base64url", EncodingType::Base64url),
    ("hex", EncodingType::Hex),
    ("url", EncodingType::Url),
    ("json_escape", EncodingType::JsonEscape),
];

/// Configuration for the Decoder middleware.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecoderConfig {
    /// The encoding type to decode from.
    pub encoding_type: EncodingType,
    /// The property containing the encoded string.
    pub target_property: String,
    /// Optional property name to store the decoded string.
    /// If None, `target_property` is overwritten.
    pub output_property: Option<String>,
    /// If true, surrounding double quotes (`"`) are removed from the target string before decoding.
    #[serde(default)]
    pub strip_quotes: bool,
    /// How to handle errors during decoding or if the target property is missing/invalid.
    #[serde(default)]
    pub on_error: ErrorHandling,
    /// Maximum allowed size for encoded strings to prevent excessive memory usage.
    #[serde(default = "default_max_size")]
    pub max_size_bytes: usize,
}

fn default_max_size() -> usize {
    1024 * 1024 // 1MB default
}

/// Middleware that decodes a string property using a specified encoding.
pub struct Decoder {
    name: String,
    config: DecoderConfig,
}

#[async_trait]
impl SourceMiddleware for Decoder {
    async fn process(
        &self,
        source_change: SourceChange,
        _element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        match source_change {
            SourceChange::Insert { mut element } => match self.decode_property(&mut element) {
                Ok(_) => Ok(vec![SourceChange::Insert { element }]),
                Err(e) => Err(e),
            },
            SourceChange::Update { mut element } => match self.decode_property(&mut element) {
                Ok(_) => Ok(vec![SourceChange::Update { element }]),
                Err(e) => Err(e),
            },
            // Pass through other change types
            SourceChange::Delete { .. } | SourceChange::Future { .. } => Ok(vec![source_change]),
        }
    }
}

impl Decoder {
    /// Helper method to get the type name of an ElementValue
    fn get_element_value_type_name(value: &ElementValue) -> &'static str {
        match value {
            ElementValue::Null => "Null",
            ElementValue::Bool(_) => "Bool",
            ElementValue::Float(_) => "Float",
            ElementValue::Integer(_) => "Integer",
            ElementValue::String(_) => "String",
            ElementValue::List(_) => "List",
            ElementValue::Object(_) => "Object",
        }
    }

    /// Helper method to get the encoding type name for error messages
    fn get_encoding_type_name(&self) -> &'static str {
        for (name, encoding_type) in ENCODING_TYPE_NAMES {
            if *encoding_type == self.config.encoding_type {
                return name;
            }
        }
        "unknown"
    }

    /// Decodes the target property within the given element.
    fn decode_property(&self, element: &mut Element) -> Result<(), MiddlewareError> {
        let target_prop_name = &self.config.target_property;
        let output_prop_name = self
            .config
            .output_property
            .as_deref()
            .unwrap_or(target_prop_name);

        // Get the property value without cloning
        match element.get_properties().get(target_prop_name) {
            Some(ElementValue::String(s)) => {
                let encoded_str = s.to_string();

                // Check size limit
                if encoded_str.len() > self.config.max_size_bytes {
                    let msg = format!(
                        "[{}] Encoded string in property '{}' exceeds size limit ({} > {})",
                        self.name,
                        target_prop_name,
                        encoded_str.len(),
                        self.config.max_size_bytes
                    );
                    log::warn!("{msg}");
                    return if self.config.on_error == ErrorHandling::Fail {
                        Err(MiddlewareError::SourceChangeError(msg))
                    } else {
                        Ok(())
                    };
                }

                // Step 1: Strip quotes if requested
                let processed_str = if self.config.strip_quotes {
                    encoded_str.trim_matches('"')
                } else {
                    &encoded_str
                }
                .to_string();

                // Step 2: Decode the string based on encoding type
                let decoded_result = match self.config.encoding_type {
                    EncodingType::Base64 => self.decode_base64(&processed_str),
                    EncodingType::Base64url => self.decode_base64url(&processed_str),
                    EncodingType::Hex => self.decode_hex(&processed_str),
                    EncodingType::Url => self.decode_url(&processed_str),
                    EncodingType::JsonEscape => self.decode_json_escape(&processed_str),
                };

                match decoded_result {
                    Ok(decoded_string) => {
                        // Get mutable access to properties to check for collision and insert
                        match element {
                            Element::Node { properties, .. }
                            | Element::Relation { properties, .. } => {
                                // Check for potential overwrite collision
                                if output_prop_name != target_prop_name
                                    && properties.get(output_prop_name).is_some()
                                {
                                    log::warn!(
                                        "[{}] Output property '{}' specified in config already exists and will be overwritten.",
                                        self.name,
                                        output_prop_name
                                    );
                                }

                                // Update the element with the decoded string
                                properties.insert(
                                    output_prop_name,
                                    ElementValue::String(decoded_string.into()),
                                );
                            }
                        }
                        Ok(())
                    }
                    Err(e) => {
                        let encoding_name = self.get_encoding_type_name();
                        let msg = format!(
                            "[{}] Failed to decode property '{}' using {} encoding: {}",
                            self.name, target_prop_name, encoding_name, e
                        );
                        log::warn!("{msg}");
                        if self.config.on_error == ErrorHandling::Fail {
                            Err(MiddlewareError::SourceChangeError(msg))
                        } else {
                            Ok(())
                        }
                    }
                }
            }
            Some(value) => {
                // Handle non-string property types
                let type_name = Self::get_element_value_type_name(value);
                let msg = format!(
                    "[{}] Target property '{}' is not a string value (Type: {}).",
                    self.name, target_prop_name, type_name
                );
                log::warn!("{msg}");
                if self.config.on_error == ErrorHandling::Fail {
                    Err(MiddlewareError::SourceChangeError(msg))
                } else {
                    Ok(())
                }
            }
            None => {
                // Handle missing property
                let msg = format!(
                    "[{}] Target property '{}' not found in element.",
                    self.name, target_prop_name
                );
                log::warn!("{msg}");
                if self.config.on_error == ErrorHandling::Fail {
                    Err(MiddlewareError::SourceChangeError(msg))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Decodes a base64 encoded string.
    pub fn decode_base64(&self, encoded: &str) -> Result<String, String> {
        base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map_err(|e| format!("Invalid base64 encoding: {e}"))
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|e| format!("Decoded bytes are not valid UTF-8: {e}"))
            })
    }

    /// Decodes a base64url encoded string.
    pub fn decode_base64url(&self, encoded: &str) -> Result<String, String> {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded.as_bytes())
            .map_err(|e| format!("Invalid base64url encoding: {e}"))
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|e| format!("Decoded bytes are not valid UTF-8: {e}"))
            })
    }

    /// Decodes a hex encoded string.
    pub fn decode_hex(&self, encoded: &str) -> Result<String, String> {
        hex::decode(encoded)
            .map_err(|e| format!("Invalid hex encoding: {e}"))
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|e| format!("Decoded bytes are not valid UTF-8: {e}"))
            })
    }

    /// Decodes a URL encoded string.
    pub fn decode_url(&self, encoded: &str) -> Result<String, String> {
        urlencoding::decode(encoded)
            .map(|cow| cow.into_owned())
            .map_err(|e| format!("Invalid URL encoding: {e}"))
    }

    /// Decodes a JSON escaped string.
    pub fn decode_json_escape(&self, encoded: &str) -> Result<String, String> {
        let json_value_str = format!("\"{encoded}\"");
        serde_json::from_str::<String>(&json_value_str)
            .map_err(|e| format!("Invalid JSON escape sequence: {e}"))
    }
}

/// Factory for creating Decoder middleware instances.
pub struct DecoderFactory {}

impl DecoderFactory {
    pub fn new() -> Self {
        DecoderFactory {}
    }
}

impl Default for DecoderFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceMiddlewareFactory for DecoderFactory {
    fn name(&self) -> String {
        "decoder".to_string()
    }

    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        let decoder_config: DecoderConfig =
            match serde_json::from_value(Value::Object(config.config.clone())) {
                Ok(cfg) => cfg,
                Err(e) => {
                    return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                        "[{}] Invalid decoder configuration: {}",
                        config.name, e
                    )));
                }
            };

        // Basic validation
        if decoder_config.target_property.is_empty() {
            return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                "[{}] Missing or empty 'target_property' field in decoder configuration",
                config.name
            )));
        }

        if let Some(output_prop) = &decoder_config.output_property {
            if output_prop.is_empty() {
                return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                    "[{}] 'output_property' cannot be empty if provided",
                    config.name
                )));
            }
        }

        // Validate max_size_bytes is reasonable
        if decoder_config.max_size_bytes == 0 {
            return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                "[{}] 'max_size_bytes' must be greater than zero",
                config.name
            )));
        }

        log::info!(
            "[{}] Creating Decoder middleware with config: {:?}",
            config.name,
            decoder_config
        );

        Ok(Arc::new(Decoder {
            name: config.name.to_string(),
            config: decoder_config,
        }))
    }
}
