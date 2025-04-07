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

use async_trait::async_trait;
use drasi_core::{
    interface::{
        ElementIndex,
        MiddlewareError,
        MiddlewareSetupError,
        SourceMiddleware,
        SourceMiddlewareFactory
    },
    models::{Element, SourceChange, SourceMiddlewareConfig, ElementValue},
};
use serde::Deserialize;
use serde_json::Value;
use base64::Engine as _;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ErrorHandling {
    Skip,
    Fail,
}

impl Default for ErrorHandling {
    fn default() -> Self {
        ErrorHandling::Skip
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EncodingType {
    Base64,
    Base64url,
    Hex,
    Url,
    JsonEscape,
}

pub struct Decoder {
    name: String,
    encoding_type: EncodingType,
    target_property: String,
    strip_quotes: bool,
    parse_json: bool,
    flatten: bool,
    output_prefix: Option<String>,
    on_error: ErrorHandling,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DecoderConfig {
    pub encoding_type: EncodingType,
    pub target_property: String,
    #[serde(default)]
    pub strip_quotes: bool,
    #[serde(default)]
    pub parse_json: bool,
    #[serde(default)]
    pub flatten: bool,
    #[serde(default)]
    pub output_prefix: Option<String>,
    #[serde(default)]
    pub on_error: ErrorHandling,
}

#[async_trait]
impl SourceMiddleware for Decoder {
    async fn process(
        &self,
        source_change: SourceChange,
        _element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        match source_change {
            SourceChange::Insert { mut element } => {
                match self.decode_property(&mut element) {
                    Ok(_) => Ok(vec![SourceChange::Insert { element }]),
                    Err(e) => Err(e),
                }
            }
            SourceChange::Update { mut element } => {
                match self.decode_property(&mut element) {
                    Ok(_) => Ok(vec![SourceChange::Update { element }]),
                    Err(e) => Err(e),
                }
            }
            SourceChange::Delete { metadata } => Ok(vec![SourceChange::Delete { metadata }]),
            SourceChange::Future { .. } => Ok(vec![source_change]),
        }
    }
}

impl Decoder {
    fn decode_property(&self, element: &mut Element) -> Result<(), MiddlewareError> {
        let property = &self.target_property;
        
        // Check if the property exists
        if element.get_properties().get(property).is_none() {
            let msg = format!("[{}] Property {} not found in element. Decoder will skip this change.", self.name, property);
            log::warn!("{}", msg);
            if self.on_error == ErrorHandling::Fail {
                return Err(MiddlewareError::SourceChangeError(msg));
            }
            return Ok(());
        }
        
        // Get the property value
        let value = element.get_property(property);
        
        // Only process string values
        let encoded = match value {
            ElementValue::String(s) => s,
            _ => {
                let msg = format!("[{}] Property {} is not a string value. Decoder will skip this change.", self.name, property);
                log::warn!("{}", msg);
                if self.on_error == ErrorHandling::Fail {
                    return Err(MiddlewareError::SourceChangeError(msg));
                }
                return Ok(());
            }
        };
        
        // Step 1: Strip quotes if requested
        let encoded = if self.strip_quotes { encoded.trim_matches('"') } else { &encoded }.to_string();
        
        // Step 2: Decode the string based on encoding type
        let decoded_result = match self.encoding_type {
            EncodingType::Base64 => self.decode_base64(&encoded),
            EncodingType::Base64url => self.decode_base64url(&encoded),
            EncodingType::Hex => self.decode_hex(&encoded),
            EncodingType::Url => self.decode_url(&encoded),
            EncodingType::JsonEscape => self.decode_json_escape(&encoded),
        };

        match decoded_result {
            Ok(decoded_string) => {
                // Step 3: Parse JSON if requested
                if self.parse_json {
                    self.handle_json_processing(element, property, &decoded_string)?;
                } else {
                    self.update_property(element, property, ElementValue::String(decoded_string.into()));
                }
                Ok(())
            }
            Err(e) => {
                let encoding_name = match self.encoding_type {
                    EncodingType::Base64 => "base64",
                    EncodingType::Base64url => "base64url",
                    EncodingType::Hex => "hex",
                    EncodingType::Url => "url",
                    EncodingType::JsonEscape => "json_escape",
                };
                let msg = format!("[{}] Failed to decode property {} using {} encoding: {}. Decoder will skip this change.", 
                    self.name, property, encoding_name, e);
                log::warn!("{}", msg);
                if self.on_error == ErrorHandling::Fail {
                    return Err(MiddlewareError::SourceChangeError(msg));
                }
                Ok(())
            }
        }
    }

    fn decode_base64(&self, encoded: &str) -> Result<String, String> {
        match base64::engine::general_purpose::STANDARD.decode(encoded.as_bytes()) {
            Ok(decoded_bytes) => {
                match String::from_utf8(decoded_bytes) {
                    Ok(decoded_string) => Ok(decoded_string),
                    Err(_) => Err("Failed to decode to UTF-8 string".to_string())
                }
            }
            Err(_) => Err("Invalid base64 encoding".to_string())
        }
    }

    fn decode_base64url(&self, encoded: &str) -> Result<String, String> {
        match base64::engine::general_purpose::URL_SAFE.decode(encoded.as_bytes()) {
            Ok(decoded_bytes) => {
                match String::from_utf8(decoded_bytes) {
                    Ok(decoded_string) => Ok(decoded_string),
                    Err(_) => Err("Failed to decode to UTF-8 string".to_string())
                }
            }
            Err(_) => Err("Invalid base64url encoding".to_string())
        }
    }

    fn decode_hex(&self, encoded: &str) -> Result<String, String> {
        match hex::decode(encoded) {
            Ok(decoded_bytes) => {
                match String::from_utf8(decoded_bytes) {
                    Ok(decoded_string) => Ok(decoded_string),
                    Err(_) => Err("Failed to decode to UTF-8 string".to_string())
                }
            }
            Err(_) => Err("Invalid hex encoding".to_string())
        }
    }

    fn decode_url(&self, encoded: &str) -> Result<String, String> {
        match urlencoding::decode(encoded) {
            Ok(decoded_string) => Ok(decoded_string.into_owned()),
            Err(_) => Err("Invalid URL encoding".to_string())
        }
    }

    fn decode_json_escape(&self, encoded: &str) -> Result<String, String> {
        // For JSON-escaped strings, we need to parse as a JSON string
        let json_value = format!("\"{}\"", encoded);
        match serde_json::from_str::<String>(&json_value) {
            Ok(decoded_string) => Ok(decoded_string),
            Err(e) => Err(format!("Invalid JSON escape sequence: {}", e))
        }
    }
    
    fn handle_json_processing(&self, element: &mut Element, property: &str, json_str: &str) -> Result<(), MiddlewareError> {
        match serde_json::from_str::<serde_json::Value>(json_str) {
            Ok(json_value) => {
                if self.flatten {
                    self.flatten_json(element, &json_value);
                } else {
                    self.update_property(element, property, ElementValue::String(json_str.into()));
                }
                Ok(())
            }
            Err(e) => {
                let msg = format!("[{}] Failed to parse JSON for property {}. Parsing will be skipped. Error: {}", self.name, property, e);
                log::warn!("{}", msg);
                if self.on_error == ErrorHandling::Fail {
                    return Err(MiddlewareError::SourceChangeError(msg));
                }
                self.update_property(element, property, ElementValue::String(json_str.into()));
                Ok(())
            }
        }
    }
    
    fn flatten_json(&self, element: &mut Element, json: &serde_json::Value) {
        if let serde_json::Value::Object(map) = json {
            for (key, value) in map {
                let property_name = if let Some(prefix) = &self.output_prefix {
                    format!("{}{}", prefix, key)
                } else {
                    key.clone()
                };
                
                // Convert the JSON value to an ElementValue
                let element_value = match value {
                    serde_json::Value::String(s) => ElementValue::String(s.clone().into()),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            ElementValue::Integer(i)
                        } else if let Some(f) = n.as_f64() {
                            ElementValue::Float(ordered_float::OrderedFloat(f))
                        } else {
                            continue;
                        }
                    },
                    serde_json::Value::Bool(b) => ElementValue::String(b.to_string().into()),
                    serde_json::Value::Null => ElementValue::Null,
                    _ => continue, // Skip nested objects and arrays for flattening
                };
                
                self.update_property(element, &property_name, element_value);
            }
        }
    }
    
    fn update_property(&self, element: &mut Element, property: &str, value: ElementValue) {
        match element {
            Element::Node { properties, .. } => {
                properties.insert(property, value);
            }
            Element::Relation { properties, .. } => {
                properties.insert(property, value);
            }
        }
    }
}

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
        let decoder_config: DecoderConfig = match serde_json::from_value(Value::Object(config.config.clone())) {
            Ok(cfg) => cfg,
            Err(e) => {
                let msg = format!("Invalid decoder configuration: {}", e);
                return Err(MiddlewareSetupError::InvalidConfiguration(msg));
            }
        };

        if decoder_config.target_property.is_empty() {
            return Err(MiddlewareSetupError::InvalidConfiguration(
                "Missing 'target_property' field in decoder configuration".to_string(),
            ));
        }

        // Add validation to ensure flatten requires parse_json
        if decoder_config.flatten && !decoder_config.parse_json {
            return Err(MiddlewareSetupError::InvalidConfiguration(
                "The 'flatten' option requires 'parse_json' to be enabled".to_string(),
            ));
        }

        let name = config.name.to_string();
        let encoding_type = decoder_config.encoding_type;
        let target_property = decoder_config.target_property.clone();
        let strip_quotes = decoder_config.strip_quotes;
        let parse_json = decoder_config.parse_json;
        let flatten = decoder_config.flatten;
        let output_prefix = decoder_config.output_prefix.clone();
        let on_error = decoder_config.on_error;
        
        log::info!(
            "[{}] Creating Decoder middleware with encoding_type: {:?}, target_property: {}, strip_quotes: {}, parse_json: {}, flatten: {}, output_prefix: {:?}, on_error: {:?}",
            name,
            encoding_type,
            target_property,
            strip_quotes,
            parse_json,
            flatten,
            output_prefix,
            on_error
        );

        Ok(Arc::new(Decoder { 
            name, 
            encoding_type,
            target_property, 
            strip_quotes, 
            parse_json, 
            flatten, 
            output_prefix,
            on_error
        }))
    }
}