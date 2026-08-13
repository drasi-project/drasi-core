// Copyright 2026 The Drasi Authors.
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
        ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    models::{Element, ElementValue, SourceChange, SourceMiddlewareConfig},
};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

#[cfg(test)]
mod tests;

fn default_max_capture_size() -> usize {
    1024 * 1024
}

/// A numbered or named regex capture group.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CaptureGroup {
    Number(usize),
    Name(String),
}

/// Policy for a missing target, a non-match, or an extraction error.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExtractionPolicy {
    #[default]
    Passthrough,
    Drop,
    Fail,
}

/// Policy when `output_property` already exists.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CollisionPolicy {
    Overwrite,
    Passthrough,
    Drop,
    #[default]
    Fail,
}

/// Configuration for generic regex capture extraction.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegexExtractConfig {
    /// String property to search.
    pub target_property: String,
    /// Rust regex syntax pattern, compiled when the middleware is created.
    pub pattern: String,
    /// Named or numbered capture group. Group zero selects the full match.
    pub capture_group: CaptureGroup,
    /// Property that receives the captured string.
    pub output_property: String,
    /// Maximum capture size in bytes.
    #[serde(default = "default_max_capture_size")]
    pub max_capture_size: usize,
    /// Behavior when `target_property` is absent.
    #[serde(default)]
    pub on_missing: ExtractionPolicy,
    /// Behavior when the regex does not match.
    #[serde(default)]
    pub on_no_match: ExtractionPolicy,
    /// Behavior for invalid input types, unmatched capture groups, and oversize captures.
    #[serde(default = "default_error_policy")]
    pub on_error: ExtractionPolicy,
    /// Behavior when a distinct `output_property` already exists.
    #[serde(default)]
    pub on_collision: CollisionPolicy,
}

fn default_error_policy() -> ExtractionPolicy {
    ExtractionPolicy::Fail
}

pub struct RegexExtract {
    name: String,
    regex: Regex,
    config: RegexExtractConfig,
}

enum ExtractionResult {
    Captured(String),
    Missing,
    NoMatch,
    Error(String),
    Collision,
}

#[async_trait]
impl SourceMiddleware for RegexExtract {
    async fn process(
        &self,
        source_change: SourceChange,
        _element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        match source_change {
            SourceChange::Insert { mut element } => match self.extract(&mut element)? {
                true => Ok(vec![SourceChange::Insert { element }]),
                false => Ok(Vec::new()),
            },
            SourceChange::Update { mut element } => match self.extract(&mut element)? {
                true => Ok(vec![SourceChange::Update { element }]),
                false => Ok(Vec::new()),
            },
            SourceChange::Delete { .. } | SourceChange::Future { .. } => Ok(vec![source_change]),
        }
    }
}

impl RegexExtract {
    fn extract(&self, element: &mut Element) -> Result<bool, MiddlewareError> {
        let result = self.extraction_result(element);
        match result {
            ExtractionResult::Captured(capture) => {
                let properties = match element {
                    Element::Node { properties, .. } | Element::Relation { properties, .. } => {
                        properties
                    }
                };
                properties.insert(
                    &self.config.output_property,
                    ElementValue::String(Arc::from(capture)),
                );
                Ok(true)
            }
            ExtractionResult::Missing => self.apply_policy(
                self.config.on_missing,
                format!(
                    "Target property '{}' is missing",
                    self.config.target_property
                ),
            ),
            ExtractionResult::NoMatch => self.apply_policy(
                self.config.on_no_match,
                format!(
                    "Pattern did not match property '{}'",
                    self.config.target_property
                ),
            ),
            ExtractionResult::Error(message) => self.apply_policy(self.config.on_error, message),
            ExtractionResult::Collision => match self.config.on_collision {
                CollisionPolicy::Overwrite => {
                    unreachable!("overwrite collisions are handled before policy dispatch")
                }
                CollisionPolicy::Passthrough => Ok(true),
                CollisionPolicy::Drop => Ok(false),
                CollisionPolicy::Fail => Err(self.error(format!(
                    "Output property '{}' already exists",
                    self.config.output_property
                ))),
            },
        }
    }

    fn extraction_result(&self, element: &Element) -> ExtractionResult {
        let properties = element.get_properties();
        let input = match properties.get(&self.config.target_property) {
            Some(ElementValue::String(value)) => value,
            Some(value) => {
                return ExtractionResult::Error(format!(
                    "Target property '{}' must be a String, found {}",
                    self.config.target_property,
                    element_value_type(value)
                ));
            }
            None => return ExtractionResult::Missing,
        };

        let captures = match self.regex.captures(input) {
            Some(captures) => captures,
            None => return ExtractionResult::NoMatch,
        };
        let capture = match &self.config.capture_group {
            CaptureGroup::Number(group) => captures.get(*group),
            CaptureGroup::Name(group) => captures.name(group),
        };
        let capture = match capture {
            Some(capture) => capture.as_str(),
            None => {
                return ExtractionResult::Error(format!(
                    "Capture group {:?} did not participate in the match",
                    self.config.capture_group
                ));
            }
        };

        if capture.len() > self.config.max_capture_size {
            return ExtractionResult::Error(format!(
                "Capture size {} exceeds max_capture_size {}",
                capture.len(),
                self.config.max_capture_size
            ));
        }

        if self.config.output_property != self.config.target_property
            && properties.get(&self.config.output_property).is_some()
            && self.config.on_collision != CollisionPolicy::Overwrite
        {
            return ExtractionResult::Collision;
        }

        ExtractionResult::Captured(capture.to_string())
    }

    fn apply_policy(
        &self,
        policy: ExtractionPolicy,
        message: String,
    ) -> Result<bool, MiddlewareError> {
        match policy {
            ExtractionPolicy::Passthrough => Ok(true),
            ExtractionPolicy::Drop => Ok(false),
            ExtractionPolicy::Fail => Err(self.error(message)),
        }
    }

    fn error(&self, message: String) -> MiddlewareError {
        MiddlewareError::SourceChangeError(format!("[{}] {message}", self.name))
    }
}

fn element_value_type(value: &ElementValue) -> &'static str {
    match value {
        ElementValue::Null => "Null",
        ElementValue::Bool(_) => "Bool",
        ElementValue::Integer(_) => "Integer",
        ElementValue::Float(_) => "Float",
        ElementValue::String(_) => "String",
        ElementValue::List(_) => "List",
        ElementValue::Object(_) => "Object",
        ElementValue::LocalDateTime(_) => "LocalDateTime",
        ElementValue::ZonedDateTime(_) => "ZonedDateTime",
    }
}

pub struct RegexExtractFactory;

impl RegexExtractFactory {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RegexExtractFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceMiddlewareFactory for RegexExtractFactory {
    fn name(&self) -> String {
        "regex_extract".to_string()
    }

    fn create(
        &self,
        config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        let regex_config: RegexExtractConfig =
            serde_json::from_value(Value::Object(config.config.clone())).map_err(|error| {
                MiddlewareSetupError::InvalidConfiguration(format!(
                    "[{}] Invalid regex_extract configuration: {error}",
                    config.name
                ))
            })?;

        validate_config(config, &regex_config)?;
        let regex = Regex::new(&regex_config.pattern).map_err(|error| {
            MiddlewareSetupError::InvalidConfiguration(format!(
                "[{}] Invalid regex pattern: {error}",
                config.name
            ))
        })?;
        validate_capture_group(config, &regex_config.capture_group, &regex)?;

        Ok(Arc::new(RegexExtract {
            name: config.name.to_string(),
            regex,
            config: regex_config,
        }))
    }
}

fn validate_config(
    middleware: &SourceMiddlewareConfig,
    config: &RegexExtractConfig,
) -> Result<(), MiddlewareSetupError> {
    for (name, value) in [
        ("target_property", &config.target_property),
        ("pattern", &config.pattern),
        ("output_property", &config.output_property),
    ] {
        if value.is_empty() {
            return Err(MiddlewareSetupError::InvalidConfiguration(format!(
                "[{}] '{name}' cannot be empty",
                middleware.name
            )));
        }
    }
    if config.max_capture_size == 0 {
        return Err(MiddlewareSetupError::InvalidConfiguration(format!(
            "[{}] 'max_capture_size' must be greater than zero",
            middleware.name
        )));
    }
    Ok(())
}

fn validate_capture_group(
    middleware: &SourceMiddlewareConfig,
    capture_group: &CaptureGroup,
    regex: &Regex,
) -> Result<(), MiddlewareSetupError> {
    let valid = match capture_group {
        CaptureGroup::Number(group) => *group < regex.captures_len(),
        CaptureGroup::Name(group) => {
            !group.is_empty() && regex.capture_names().flatten().any(|name| name == group)
        }
    };
    if valid {
        Ok(())
    } else {
        Err(MiddlewareSetupError::InvalidConfiguration(format!(
            "[{}] Capture group {:?} does not exist in the pattern",
            middleware.name, capture_group
        )))
    }
}
