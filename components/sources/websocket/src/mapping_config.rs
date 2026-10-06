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

use std::collections::HashMap;

use drasi_source_mapping::{
    EffectiveFromConfig, EffectiveFromConfigDto, ElementTemplate, ElementTemplateDto, ElementType,
    ElementTypeDto, MappingCondition, MappingConditionDto, OperationType, OperationTypeDto,
    SourceMapping, SourceMappingDto, TimestampFormat, TimestampFormatDto,
};
use serde::{Deserialize, Serialize};

/// Strict user-facing shape for a WebSocket source mapping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::SourceMapping)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourceMappingSchema {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<MappingConditionSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationTypeSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_map: Option<HashMap<String, OperationTypeSchema>>,
    pub element_type: ElementTypeSchema,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<EffectiveFromConfigSchema>,
    pub template: ElementTemplateSchema,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::MappingCondition)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MappingConditionSchema {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::OperationType)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OperationTypeSchema {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::ElementType)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ElementTypeSchema {
    Node,
    Relation,
}

#[derive(Debug, Clone, Serialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::EffectiveFromConfig)]
#[serde(untagged)]
pub(crate) enum EffectiveFromConfigSchema {
    Simple(String),
    Explicit(ExplicitEffectiveFromConfigSchema),
}

impl<'de> Deserialize<'de> for EffectiveFromConfigSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(value) => Ok(Self::Simple(value)),
            serde_json::Value::Object(_) => serde_json::from_value(value)
                .map(Self::Explicit)
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "effectiveFrom must be a template string or explicit object",
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::ExplicitEffectiveFromConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExplicitEffectiveFromConfigSchema {
    pub value: String,
    pub format: TimestampFormatSchema,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::TimestampFormat)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TimestampFormatSchema {
    Iso8601,
    UnixSeconds,
    UnixMillis,
    UnixNanos,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::ElementTemplate)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ElementTemplateSchema {
    pub id: String,
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

pub(crate) mod mapping_serde {
    use serde::{de::Error as _, Deserialize, Serialize};

    use super::{mapping_to_dto, SourceMapping, SourceMappingDto, SourceMappingSchema};

    pub fn serialize<S>(mappings: &[SourceMapping], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        mappings
            .iter()
            .map(mapping_to_dto)
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<SourceMapping>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Vec::<serde_json::Value>::deserialize(deserializer)?
            .into_iter()
            .map(|mapping| {
                serde_json::from_value::<SourceMappingSchema>(mapping.clone())
                    .map_err(D::Error::custom)?;
                serde_json::from_value::<SourceMappingDto>(mapping)
                    .map(Into::into)
                    .map_err(D::Error::custom)
            })
            .collect()
    }
}

pub(crate) fn mapping_to_dto(mapping: &SourceMapping) -> SourceMappingDto {
    SourceMappingDto {
        when: mapping.when.as_ref().map(condition_to_dto),
        operation: mapping.operation.as_ref().map(operation_to_dto),
        operation_from: mapping.operation_from.clone(),
        operation_map: mapping.operation_map.as_ref().map(|operations| {
            operations
                .iter()
                .map(|(name, operation)| (name.clone(), operation_to_dto(operation)))
                .collect()
        }),
        element_type: element_type_to_dto(&mapping.element_type),
        effective_from: mapping.effective_from.as_ref().map(effective_from_to_dto),
        template: template_to_dto(&mapping.template),
    }
}

fn condition_to_dto(condition: &MappingCondition) -> MappingConditionDto {
    MappingConditionDto {
        header: condition.header.clone(),
        field: condition.field.clone(),
        equals: condition.equals.clone(),
        contains: condition.contains.clone(),
        regex: condition.regex.clone(),
    }
}

fn operation_to_dto(operation: &OperationType) -> OperationTypeDto {
    match operation {
        OperationType::Insert => OperationTypeDto::Insert,
        OperationType::Update => OperationTypeDto::Update,
        OperationType::Delete => OperationTypeDto::Delete,
    }
}

fn element_type_to_dto(element_type: &ElementType) -> ElementTypeDto {
    match element_type {
        ElementType::Node => ElementTypeDto::Node,
        ElementType::Relation => ElementTypeDto::Relation,
    }
}

fn effective_from_to_dto(effective_from: &EffectiveFromConfig) -> EffectiveFromConfigDto {
    match effective_from {
        EffectiveFromConfig::Simple(value) => EffectiveFromConfigDto::Simple(value.clone()),
        EffectiveFromConfig::Explicit { value, format } => EffectiveFromConfigDto::Explicit {
            value: value.clone(),
            format: timestamp_format_to_dto(format),
        },
    }
}

fn timestamp_format_to_dto(format: &TimestampFormat) -> TimestampFormatDto {
    match format {
        TimestampFormat::Iso8601 => TimestampFormatDto::Iso8601,
        TimestampFormat::UnixSeconds => TimestampFormatDto::UnixSeconds,
        TimestampFormat::UnixMillis => TimestampFormatDto::UnixMillis,
        TimestampFormat::UnixNanos => TimestampFormatDto::UnixNanos,
    }
}

fn template_to_dto(template: &ElementTemplate) -> ElementTemplateDto {
    ElementTemplateDto {
        id: template.id.clone(),
        labels: template.labels.clone(),
        properties: template.properties.clone(),
        from: template.from.clone(),
        to: template.to.clone(),
    }
}
