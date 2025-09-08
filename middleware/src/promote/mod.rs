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
use jsonpath_rust::{path::config::JsonPathConfig, JsonPathInst};
use log::debug;
use serde::{de, Deserialize, Deserializer};
use serde_json::Value;
use std::{str::FromStr, sync::Arc};

#[cfg(test)]
mod tests;

/// Defines how to handle cases where the target property already exists
#[derive(Default, Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ConflictStrategy {
    /// Overwrite the existing property
    #[default]
    Overwrite,
    /// Skip this mapping and keep the existing property
    Skip,
    /// Fail with an error
    Fail,
}

/// JSONPath expression wrapper for selecting values to promote
#[derive(Debug, Clone)]
pub struct JsonPathExpression {
    expression: String,
    path: JsonPathInst,
}

impl JsonPathExpression {
    /// Execute the JSONPath expression and return all matching values
    fn execute(&self, value: &Value) -> Vec<Value> {
        self.path
            .find_slice(value, JsonPathConfig::default())
            .into_iter()
            .map(|v| (*v).clone())
            .collect()
    }
}

impl FromStr for JsonPathExpression {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Note: jsonpath_rust crate seems to allow empty strings,
        // so the check must be done explicitly before calling from_str.
        match JsonPathInst::from_str(s) {
            Ok(path) => Ok(JsonPathExpression {
                expression: s.to_string(),
                path,
            }),
            Err(e) => Err(format!("Failed to parse rule: {e}")),
        }
    }
}

/// Configuration for a single JSONPath mapping
#[derive(Debug, Clone, Deserialize)]
pub struct PromoteMapping {
    /// JSONPath expression to select the value to promote
    #[serde(deserialize_with = "deserialize_jsonpath")]
    pub path: JsonPathExpression,
    /// Name for the promoted value
    pub target_name: String,
}

fn deserialize_jsonpath<'de, D>(deserializer: D) -> Result<JsonPathExpression, D::Error>
where
    D: Deserializer<'de>,
{
    let expression = String::deserialize(deserializer)?;
    // Explicitly check for empty string before parsing
    if expression.is_empty() {
        return Err(de::Error::custom("Empty JSONPath"));
    }
    JsonPathExpression::from_str(&expression).map_err(de::Error::custom)
}

/// Configuration for the promote middleware
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromoteMiddlewareConfig {
    /// List of mappings that define what values to promote
    pub mappings: Vec<PromoteMapping>,
    /// Strategy for handling property name conflicts
    #[serde(default)]
    pub on_conflict: ConflictStrategy,
    /// Strategy for handling JSONPath errors
    #[serde(default)]
    pub on_error: ErrorHandling,
}

pub struct PromoteMiddleware {
    name: String,
    mappings: Vec<PromoteMapping>,
    on_conflict: ConflictStrategy,
    on_error: ErrorHandling,
}

impl PromoteMiddleware {
    pub fn new(name: String, config: PromoteMiddlewareConfig) -> Self {
        PromoteMiddleware {
            name,
            mappings: config.mappings,
            on_conflict: config.on_conflict,
            on_error: config.on_error,
        }
    }

    /// Extract value using JSONPath and handle according to error strategy
    fn extract_value(
        &self,
        json_obj: &Value,
        path: &JsonPathExpression,
    ) -> Result<Option<Value>, MiddlewareError> {
        // Check for null or empty results
        let results = path.execute(json_obj);

        if results.is_empty() {
            let msg = format!(
                "[{}] JSONPath '{}' selected no values",
                self.name, path.expression
            );
            match self.on_error {
                ErrorHandling::Skip => {
                    debug!("{}", msg);
                    Ok(None)
                }
                ErrorHandling::Fail => Err(MiddlewareError::SourceChangeError(msg)),
            }
        } else if results.len() > 1 {
            // Multiple results
            let msg = format!(
                "[{}] JSONPath '{}' selected multiple values ({})",
                self.name,
                path.expression,
                results.len()
            );
            match self.on_error {
                ErrorHandling::Skip => {
                    debug!("{}", msg);
                    Ok(None)
                }
                ErrorHandling::Fail => Err(MiddlewareError::SourceChangeError(msg)),
            }
        } else {
            // Single result
            Ok(Some(results[0].clone()))
        }
    }

    /// Process a single element, promoting properties according to the mappings
    fn process_element(&self, element: &mut Element) -> Result<(), MiddlewareError> {
        // Access the properties mutably
        let properties = match element {
            Element::Node { properties, .. } => properties,
            Element::Relation { properties, .. } => properties,
        };

        // Create a JSON representation from the properties once before the loop.
        // Use an immutable borrow for the conversion.
        let json_props: Value = {
            let map: serde_json::Map<String, serde_json::Value> = (&*properties).into();
            Value::Object(map)
        };

        for mapping in &self.mappings {
            // Extract the value using JSONPath from the pre-converted JSON object
            let value = match self.extract_value(&json_props, &mapping.path)? {
                Some(v) => v,
                None => continue, // Skip this mapping
            };

            // Check if the property already exists using the mutable properties reference
            if let Some(existing) = properties.get(&mapping.target_name) {
                if *existing != ElementValue::Null {
                    match self.on_conflict {
                        ConflictStrategy::Overwrite => {
                            debug!(
                                "[{}] Overwriting existing property '{}'",
                                self.name, mapping.target_name
                            );
                        }
                        ConflictStrategy::Skip => {
                            debug!(
                                "[{}] Skipping promotion to '{}' due to existing property",
                                self.name, mapping.target_name
                            );
                            continue;
                        }
                        ConflictStrategy::Fail => {
                            return Err(MiddlewareError::SourceChangeError(format!(
                                "[{}] Property '{}' already exists and conflict strategy is 'fail'",
                                self.name, mapping.target_name
                            )));
                        }
                    }
                }
            }

            // Convert JSON value to ElementValue and insert using the mutable properties reference
            let element_value = ElementValue::from(&value);
            properties.insert(&mapping.target_name, element_value);
        }

        Ok(())
    }
}

#[async_trait]
impl SourceMiddleware for PromoteMiddleware {
    async fn process(
        &self,
        source_change: SourceChange,
        _element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        match source_change {
            SourceChange::Insert { mut element } => match self.process_element(&mut element) {
                Ok(_) => Ok(vec![SourceChange::Insert { element }]),
                Err(e) => Err(e),
            },
            SourceChange::Update { mut element } => match self.process_element(&mut element) {
                Ok(_) => Ok(vec![SourceChange::Update { element }]),
                Err(e) => Err(e),
            },
            // Pass through other change types
            SourceChange::Delete { .. } | SourceChange::Future { .. } => Ok(vec![source_change]),
        }
    }
}

pub struct PromoteMiddlewareFactory {}

impl PromoteMiddlewareFactory {
    pub fn new() -> Self {
        PromoteMiddlewareFactory {}
    }
}

impl Default for PromoteMiddlewareFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceMiddlewareFactory for PromoteMiddlewareFactory {
    fn name(&self) -> String {
        "promote".to_string()
    }

    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        // Parse configuration
        let promote_config: PromoteMiddlewareConfig =
            match serde_json::from_value(serde_json::Value::Object(config.config.clone())) {
                Ok(cfg) => cfg,
                Err(e) => {
                    // Check if the error message is the one we set for empty paths
                    if e.to_string().contains("Empty JSONPath") {
                        return Err(MiddlewareSetupError::InvalidConfiguration(
                            format!("[{}] {}", config.name, e), // Use the custom message
                        ));
                    }
                    // Otherwise, wrap the generic serde error
                    return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                        "[{}] Invalid configuration: {}",
                        config.name, e
                    )));
                }
            };

        // Validate configuration
        if promote_config.mappings.is_empty() {
            return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                "[{}] At least one mapping must be specified",
                config.name
            )));
        }

        // Validate each mapping - only check target_name now
        for (i, mapping) in promote_config.mappings.iter().enumerate() {
            if mapping.target_name.is_empty() {
                return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                    "[{}] Empty target_name in mapping at index {}",
                    config.name, i
                )));
            }
        }

        log::info!(
            "[{}] Creating Promote middleware with {} mappings",
            config.name,
            promote_config.mappings.len()
        );

        // Create middleware instance
        Ok(Arc::new(PromoteMiddleware::new(
            config.name.to_string(),
            promote_config,
        )))
    }
}
