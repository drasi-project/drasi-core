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

//! User-facing DTO types for source mapping configuration.
//!
//! These types use `camelCase` serialization to match the user-facing YAML/JSON
//! configuration format. They convert to the internal runtime types via `From` impls.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::config::{
    EffectiveFromConfig, ElementTemplate, ElementType, MappingCondition, OperationType,
    SourceMapping, TimestampFormat,
};

/// DTO for source mapping configuration (user-facing, camelCase).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceMappingDto {
    /// Optional condition for when this mapping applies
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<MappingConditionDto>,

    /// Static operation type
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationTypeDto>,

    /// Path to extract operation from context (e.g., "payload.action")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_from: Option<String>,

    /// Mapping from extracted values to operation types
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_map: Option<HashMap<String, OperationTypeDto>>,

    /// Element type to create
    pub element_type: ElementTypeDto,

    /// Timestamp configuration for effective_from
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<EffectiveFromConfigDto>,

    /// Template for element creation
    pub template: ElementTemplateDto,
}

/// DTO for mapping condition (user-facing, camelCase).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MappingConditionDto {
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

/// DTO for operation type (user-facing).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OperationTypeDto {
    Insert,
    Update,
    Delete,
}

/// DTO for element type (user-facing).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ElementTypeDto {
    Node,
    Relation,
}

/// DTO for effective_from timestamp configuration (user-facing, camelCase).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum EffectiveFromConfigDto {
    /// Simple template string (auto-detect format)
    Simple(String),
    /// Explicit configuration with format
    #[serde(rename_all = "camelCase")]
    Explicit {
        /// Template for the timestamp value
        value: String,
        /// Format of the timestamp
        format: TimestampFormatDto,
    },
}

/// DTO for timestamp format (user-facing).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TimestampFormatDto {
    Iso8601,
    UnixSeconds,
    UnixMillis,
    UnixNanos,
}

/// DTO for element template (user-facing, camelCase).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ElementTemplateDto {
    /// Template for element ID
    pub id: String,

    /// Templates for element labels
    pub labels: Vec<String>,

    /// Templates for element properties
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,

    /// Template for relation source node ID (relations only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,

    /// Template for relation target node ID (relations only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

// --- From<Dto> -> Runtime conversions ---

impl From<SourceMappingDto> for SourceMapping {
    fn from(dto: SourceMappingDto) -> Self {
        Self {
            when: dto.when.map(Into::into),
            operation: dto.operation.map(Into::into),
            operation_from: dto.operation_from,
            operation_map: dto
                .operation_map
                .map(|m| m.into_iter().map(|(k, v)| (k, v.into())).collect()),
            element_type: dto.element_type.into(),
            effective_from: dto.effective_from.map(Into::into),
            template: dto.template.into(),
        }
    }
}

impl From<MappingConditionDto> for MappingCondition {
    fn from(dto: MappingConditionDto) -> Self {
        Self {
            header: dto.header,
            field: dto.field,
            equals: dto.equals,
            contains: dto.contains,
            regex: dto.regex,
        }
    }
}

impl From<OperationTypeDto> for OperationType {
    fn from(dto: OperationTypeDto) -> Self {
        match dto {
            OperationTypeDto::Insert => Self::Insert,
            OperationTypeDto::Update => Self::Update,
            OperationTypeDto::Delete => Self::Delete,
        }
    }
}

impl From<ElementTypeDto> for ElementType {
    fn from(dto: ElementTypeDto) -> Self {
        match dto {
            ElementTypeDto::Node => Self::Node,
            ElementTypeDto::Relation => Self::Relation,
        }
    }
}

impl From<EffectiveFromConfigDto> for EffectiveFromConfig {
    fn from(dto: EffectiveFromConfigDto) -> Self {
        match dto {
            EffectiveFromConfigDto::Simple(s) => Self::Simple(s),
            EffectiveFromConfigDto::Explicit { value, format } => Self::Explicit {
                value,
                format: format.into(),
            },
        }
    }
}

impl From<TimestampFormatDto> for TimestampFormat {
    fn from(dto: TimestampFormatDto) -> Self {
        match dto {
            TimestampFormatDto::Iso8601 => Self::Iso8601,
            TimestampFormatDto::UnixSeconds => Self::UnixSeconds,
            TimestampFormatDto::UnixMillis => Self::UnixMillis,
            TimestampFormatDto::UnixNanos => Self::UnixNanos,
        }
    }
}

impl From<ElementTemplateDto> for ElementTemplate {
    fn from(dto: ElementTemplateDto) -> Self {
        Self {
            id: dto.id,
            labels: dto.labels,
            properties: dto.properties,
            from: dto.from,
            to: dto.to,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dto_deserializes_camel_case() {
        let json = r#"{
            "elementType": "node",
            "operation": "insert",
            "operationFrom": "payload.action",
            "operationMap": {
                "created": "insert",
                "deleted": "delete"
            },
            "effectiveFrom": {
                "value": "{{payload.timestamp}}",
                "format": "unix_millis"
            },
            "template": {
                "id": "{{key}}",
                "labels": ["Order"],
                "properties": {"name": "{{payload.name}}"}
            }
        }"#;

        let dto: SourceMappingDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.element_type, ElementTypeDto::Node);
        assert_eq!(dto.operation, Some(OperationTypeDto::Insert));
        assert_eq!(dto.operation_from, Some("payload.action".to_string()));
        assert!(dto.operation_map.is_some());

        // Convert to runtime
        let runtime: SourceMapping = dto.into();
        assert_eq!(runtime.element_type, ElementType::Node);
        assert_eq!(runtime.operation, Some(OperationType::Insert));
    }

    #[test]
    fn test_dto_with_condition() {
        let json = r#"{
            "when": {
                "field": "payload.type",
                "equals": "order"
            },
            "elementType": "node",
            "operation": "insert",
            "template": {
                "id": "{{key}}",
                "labels": ["Order"]
            }
        }"#;

        let dto: SourceMappingDto = serde_json::from_str(json).unwrap();
        let condition = dto.when.as_ref().unwrap();
        assert_eq!(condition.field, Some("payload.type".to_string()));
        assert_eq!(condition.equals, Some("order".to_string()));

        let runtime: SourceMapping = dto.into();
        let cond = runtime.when.unwrap();
        assert_eq!(cond.field, Some("payload.type".to_string()));
    }

    #[test]
    fn test_dto_simple_effective_from() {
        let json = r#"{
            "elementType": "node",
            "operation": "update",
            "effectiveFrom": "{{payload.ts}}",
            "template": {
                "id": "{{key}}",
                "labels": ["Item"]
            }
        }"#;

        let dto: SourceMappingDto = serde_json::from_str(json).unwrap();
        assert_eq!(
            dto.effective_from,
            Some(EffectiveFromConfigDto::Simple("{{payload.ts}}".to_string()))
        );

        let runtime: SourceMapping = dto.into();
        assert_eq!(
            runtime.effective_from,
            Some(EffectiveFromConfig::Simple("{{payload.ts}}".to_string()))
        );
    }
}
