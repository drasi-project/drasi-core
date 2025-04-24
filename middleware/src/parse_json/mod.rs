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

/// Error type to represent the specific type of parsing error
#[derive(Debug, PartialEq, Eq)]
pub enum ParseJsonErrorType {
    /// The target property was not found in the element
    MissingProperty,
    /// The target property value is not a string
    InvalidType,
    /// The JSON string exceeds the configured maximum size
    SizeExceeded,
    /// The JSON string contains invalid characters or sequences
    InvalidInput,
    /// The JSON string exceeds the maximum allowed nesting depth
    DeepNesting,
    /// Error during JSON parsing
    ParseError,
    /// Error converting parsed JSON to ElementValue
    ConversionError,
}

/// Configuration for the ParseJson middleware.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseJsonConfig {
    /// The property containing the JSON string to parse.
    pub target_property: String,
    /// Optional property name to store the parsed JSON value.
    /// If None, `target_property` is overwritten.
    pub output_property: Option<String>,
    /// How to handle errors during parsing or if the target property is missing/invalid.
    #[serde(default)]
    pub on_error: ErrorHandling,
    /// Maximum size in bytes for the JSON string (default: 1MB)
    #[serde(default = "default_max_json_size")]
    pub max_json_size: usize,
    /// Maximum nesting depth for JSON objects/arrays (default: 20)
    #[serde(default = "default_max_nesting_depth")]
    pub max_nesting_depth: usize,
}

fn default_max_json_size() -> usize {
    1024 * 1024 // 1MB default limit
}

fn default_max_nesting_depth() -> usize {
    20 // Default max nesting depth
}

/// Middleware that parses a string property containing JSON into an ElementValue.
pub struct ParseJson {
    name: String,
    config: ParseJsonConfig,
}

#[async_trait]
impl SourceMiddleware for ParseJson {
    async fn process(
        &self,
        source_change: SourceChange,
        _element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        match source_change {
            SourceChange::Insert { mut element } => match self.parse_property(&mut element) {
                Ok(_) => Ok(vec![SourceChange::Insert { element }]),
                Err(e) => Err(e),
            },
            SourceChange::Update { mut element } => match self.parse_property(&mut element) {
                Ok(_) => Ok(vec![SourceChange::Update { element }]),
                Err(e) => Err(e),
            },
            // Pass through other change types
            SourceChange::Delete { .. } | SourceChange::Future { .. } => Ok(vec![source_change]),
        }
    }
}

impl ParseJson {
    /// Helper method to get the type name of an ElementValue
    fn get_element_value_type_name(value: &ElementValue) -> &'static str {
        match value {
            ElementValue::Integer(_) => "Integer",
            ElementValue::Float(_) => "Float",
            ElementValue::String(_) => "String",
            ElementValue::Bool(_) => "Bool",
            ElementValue::List(_) => "List",
            ElementValue::Object(_) => "Object",
            ElementValue::Null => "Null",
        }
    }

    /// Handles a parsing error based on configuration
    fn handle_error(
        &self,
        error_type: ParseJsonErrorType,
        message: String,
    ) -> Result<(), MiddlewareError> {
        log::warn!("[{}] {}", self.name, message);

        if self.config.on_error == ErrorHandling::Fail {
            Err(MiddlewareError::SourceChangeError(format!(
                "[{}] [Error: {:?}] {}",
                self.name, error_type, message
            )))
        } else {
            Ok(())
        }
    }

    /// Calculate the nesting depth of a JSON value
    fn calculate_nesting_depth(value: &Value) -> usize {
        match value {
            Value::Object(map) => {
                let max_child_depth = map
                    .values()
                    .map(Self::calculate_nesting_depth)
                    .max()
                    .unwrap_or(0);
                1 + max_child_depth
            }
            Value::Array(arr) => {
                let max_child_depth = arr
                    .iter()
                    .map(Self::calculate_nesting_depth)
                    .max()
                    .unwrap_or(0);
                1 + max_child_depth
            }
            _ => 1, // Primitive values have depth 1
        }
    }

    /// Check for invalid control characters in a JSON string
    fn check_for_invalid_characters(
        &self,
        json_string: &str,
    ) -> Result<(), (ParseJsonErrorType, String)> {
        // Check for control characters that might indicate malicious input
        if json_string
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
        {
            return Err((
                ParseJsonErrorType::InvalidInput,
                "JSON string contains invalid control characters".to_string(),
            ));
        }
        Ok(())
    }

    /// Parses the target JSON string property within the given element.
    fn parse_property(&self, element: &mut Element) -> Result<(), MiddlewareError> {
        let target_prop_name = &self.config.target_property;
        let output_prop_name = self
            .config
            .output_property
            .as_deref()
            .unwrap_or(target_prop_name);

        // Single lookup of property - store the value if found
        let json_string = match element.get_properties().get(target_prop_name) {
            Some(ElementValue::String(s)) => s.as_ref(),
            Some(other_value) => {
                let type_name = Self::get_element_value_type_name(other_value);
                return self.handle_error(
                    ParseJsonErrorType::InvalidType,
                    format!(
                        "Target property '{}' is not a String (Type: {}). Cannot parse JSON.",
                        target_prop_name, type_name
                    ),
                );
            }
            None => {
                return self.handle_error(
                    ParseJsonErrorType::MissingProperty,
                    format!(
                        "Target property '{}' not found in element. Cannot parse JSON.",
                        target_prop_name
                    ),
                );
            }
        };

        // Check size limit
        if json_string.len() > self.config.max_json_size {
            return self.handle_error(
                ParseJsonErrorType::SizeExceeded,
                format!(
                    "JSON string in property '{}' exceeds maximum allowed size ({} > {} bytes)",
                    target_prop_name,
                    json_string.len(),
                    self.config.max_json_size
                ),
            );
        }

        // Check for invalid control characters
        if let Err((error_type, message)) = self.check_for_invalid_characters(json_string) {
            return self.handle_error(error_type, message);
        }

        // Attempt to parse the JSON string using serde_json
        match serde_json::from_str::<Value>(json_string) {
            Ok(parsed_value) => {
                // Check nesting depth after parsing
                let depth = Self::calculate_nesting_depth(&parsed_value);
                if depth > self.config.max_nesting_depth {
                    return self.handle_error(
                        ParseJsonErrorType::DeepNesting,
                        format!(
                            "JSON nesting depth ({}) exceeds maximum allowed depth ({})",
                            depth, self.config.max_nesting_depth
                        ),
                    );
                }

                // Convert serde_json::Value to ElementValue
                let element_value = ElementValue::from(&parsed_value);
                // Update the element's properties
                match element {
                    Element::Node { properties, .. } | Element::Relation { properties, .. } => {
                        // Check for potential overwrite collision if output_property is specified
                        if output_prop_name != target_prop_name
                            && properties.get(output_prop_name).is_some()
                        {
                            log::warn!(
                                "[{}] Output property '{}' specified in config already exists and will be overwritten.",
                                self.name,
                                output_prop_name
                            );
                        }
                        // Insert or overwrite the property.
                        properties.insert(output_prop_name, element_value);
                    }
                }
                Ok(())
            }
            Err(e) => self.handle_error(
                ParseJsonErrorType::ParseError,
                format!(
                    "Failed to parse JSON string in property '{}': {}",
                    target_prop_name, e
                ),
            ),
        }
    }
}

/// Factory for creating ParseJson middleware instances.
pub struct ParseJsonFactory {}

impl ParseJsonFactory {
    pub fn new() -> Self {
        ParseJsonFactory {}
    }
}

impl Default for ParseJsonFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceMiddlewareFactory for ParseJsonFactory {
    fn name(&self) -> String {
        "parse_json".to_string()
    }

    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        let parse_json_config: ParseJsonConfig =
            match serde_json::from_value(Value::Object(config.config.clone())) {
                Ok(cfg) => cfg,
                Err(e) => {
                    return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                        "[{}] Invalid parse_json configuration: {}",
                        config.name, e
                    )));
                }
            };

        // Validation
        if parse_json_config.target_property.is_empty() {
            return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                "[{}] Missing or empty 'target_property' field in parse_json configuration",
                config.name
            )));
        }

        if let Some(output_prop) = &parse_json_config.output_property {
            if output_prop.is_empty() {
                return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                    "[{}] 'output_property' cannot be empty if provided in parse_json configuration",
                    config.name
                )));
            }
        }

        // Validate max_json_size
        if parse_json_config.max_json_size == 0 {
            return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                "[{}] 'max_json_size' must be greater than 0",
                config.name
            )));
        }

        // Warn about potentially risky configurations
        if parse_json_config.max_json_size > 10 * 1024 * 1024 {
            log::warn!(
                "[{}] Large 'max_json_size' configured ({}MB). This might cause memory issues with large JSON inputs.",
                config.name,
                parse_json_config.max_json_size / (1024 * 1024)
            );
        }

        log::debug!(
            "[{}] Creating ParseJson middleware with config: {:?}",
            config.name,
            parse_json_config
        );

        Ok(Arc::new(ParseJson {
            name: config.name.to_string(),
            config: parse_json_config,
        }))
    }
}
