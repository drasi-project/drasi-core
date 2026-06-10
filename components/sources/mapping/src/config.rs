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

//! Configuration types for source payload mapping.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Mapping configuration from source payload to graph change event.
///
/// Defines how to transform an incoming payload into a `SourceChange`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceMapping {
    /// Optional condition for when this mapping applies
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<MappingCondition>,

    /// Static operation type
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationType>,

    /// Path to extract operation from context (e.g., "payload.action")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_from: Option<String>,

    /// Mapping from extracted values to operation types
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_map: Option<HashMap<String, OperationType>>,

    /// Element type to create
    pub element_type: ElementType,

    /// Timestamp configuration for effective_from
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<EffectiveFromConfig>,

    /// Template for element creation
    pub template: ElementTemplate,
}

/// Condition for when a mapping applies
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MappingCondition {
    /// Header to check (HTTP-specific, but included for compatibility)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,

    /// Payload field path to check (dot notation)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,

    /// Value must equal this
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,

    /// Value must contain this
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,

    /// Value must match this regex
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

/// Operation type for source changes
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OperationType {
    Insert,
    Update,
    Delete,
}

/// Element type for source changes
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ElementType {
    Node,
    Relation,
}

/// Configuration for effective_from timestamp
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum EffectiveFromConfig {
    /// Simple template string (auto-detect format)
    Simple(String),
    /// Explicit configuration with format
    Explicit {
        /// Template for the timestamp value
        value: String,
        /// Format of the timestamp
        format: TimestampFormat,
    },
}

/// Timestamp format for effective_from
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TimestampFormat {
    /// ISO 8601 datetime string
    Iso8601,
    /// Unix timestamp in seconds
    UnixSeconds,
    /// Unix timestamp in milliseconds
    UnixMillis,
    /// Unix timestamp in nanoseconds
    UnixNanos,
}

/// Template for element creation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ElementTemplate {
    /// Template for element ID
    pub id: String,

    /// Templates for element labels
    pub labels: Vec<String>,

    /// Templates for element properties (can be individual templates or a single object template)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,

    /// Template for relation source node ID (relations only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,

    /// Template for relation target node ID (relations only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

impl SourceMapping {
    /// Validate mapping configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        // Must have either static operation or dynamic operation_from
        if self.operation.is_none() && self.operation_from.is_none() {
            return Err(anyhow::anyhow!(
                "either 'operation' or 'operation_from' must be specified"
            ));
        }

        // If using operation_from, should have operation_map
        if self.operation_from.is_some() && self.operation_map.is_none() {
            return Err(anyhow::anyhow!(
                "'operation_map' is required when using 'operation_from'"
            ));
        }

        // Validate template
        self.template.validate(&self.element_type)?;

        Ok(())
    }
}

impl ElementTemplate {
    /// Validate element template configuration
    pub fn validate(&self, element_type: &ElementType) -> anyhow::Result<()> {
        if self.id.is_empty() {
            return Err(anyhow::anyhow!("template.id cannot be empty"));
        }

        if self.labels.is_empty() {
            return Err(anyhow::anyhow!("template.labels cannot be empty"));
        }

        // Relations require from and to
        if *element_type == ElementType::Relation {
            if self.from.is_none() {
                return Err(anyhow::anyhow!(
                    "template.from is required for relation elements"
                ));
            }
            if self.to.is_none() {
                return Err(anyhow::anyhow!(
                    "template.to is required for relation elements"
                ));
            }
        }

        Ok(())
    }
}
