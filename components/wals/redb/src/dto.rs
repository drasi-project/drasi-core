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

//! Explicit wire format for WAL records.
//!
//! Core domain types (`SourceChange`, `Element`, etc.) intentionally do NOT
//! implement `serde::Serialize`/`Deserialize`. This crate owns the wire format
//! for the redb WAL and converts between core types and DTOs on the boundary.
//!
//! This indirection:
//! - Keeps core types free of serialization concerns
//! - Makes the WAL binary format explicit and versionable
//! - Allows the wire format to evolve independently from the core domain model

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Wire-format version. Incremented on breaking DTO changes.
///
/// A WAL record with a different version is treated as a decode failure; in
/// practice the WAL is ephemeral and is expected to be pruned or cleared on
/// version changes.
pub(crate) const WAL_FORMAT_VERSION: u32 = 1;

/// Envelope around every persisted event — carries the format version.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WalRecord {
    pub version: u32,
    pub change: SourceChangeDto,
}

/// DTO mirror of [`SourceChange`]. `Future` is intentionally omitted — the WAL
/// contract rejects that variant at append time.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum SourceChangeDto {
    Insert { element: ElementDto },
    Update { element: ElementDto },
    Delete { metadata: ElementMetadataDto },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ElementDto {
    Node {
        metadata: ElementMetadataDto,
        properties: PropertyMapDto,
    },
    Relation {
        metadata: ElementMetadataDto,
        in_node: ElementReferenceDto,
        out_node: ElementReferenceDto,
        properties: PropertyMapDto,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ElementReferenceDto {
    pub source_id: String,
    pub element_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ElementMetadataDto {
    pub reference: ElementReferenceDto,
    pub labels: Vec<String>,
    pub effective_from: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ElementValueDto {
    Null,
    Bool(bool),
    Float(f64),
    Integer(i64),
    String(String),
    List(Vec<ElementValueDto>),
    Object(PropertyMapDto),
}

/// Property map is serialized as an ordered vec of (key, value) pairs to
/// preserve the stable `BTreeMap` iteration order from the core type.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PropertyMapDto {
    pub entries: Vec<(String, ElementValueDto)>,
}

// ---------------------------------------------------------------------------
// Core -> DTO
// ---------------------------------------------------------------------------

impl From<&SourceChange> for SourceChangeDto {
    fn from(change: &SourceChange) -> Self {
        match change {
            SourceChange::Insert { element } => SourceChangeDto::Insert {
                element: element.into(),
            },
            SourceChange::Update { element } => SourceChangeDto::Update {
                element: element.into(),
            },
            SourceChange::Delete { metadata } => SourceChangeDto::Delete {
                metadata: metadata.into(),
            },
            SourceChange::Future { .. } => {
                // Caller is expected to reject Future variants before reaching
                // DTO conversion. Reaching here indicates a programming bug.
                panic!("SourceChange::Future cannot be converted to DTO — reject earlier");
            }
        }
    }
}

impl From<&Element> for ElementDto {
    fn from(element: &Element) -> Self {
        match element {
            Element::Node {
                metadata,
                properties,
            } => ElementDto::Node {
                metadata: metadata.into(),
                properties: properties.into(),
            },
            Element::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => ElementDto::Relation {
                metadata: metadata.into(),
                in_node: in_node.into(),
                out_node: out_node.into(),
                properties: properties.into(),
            },
        }
    }
}

impl From<&ElementMetadata> for ElementMetadataDto {
    fn from(md: &ElementMetadata) -> Self {
        ElementMetadataDto {
            reference: (&md.reference).into(),
            labels: md.labels.iter().map(|l| l.to_string()).collect(),
            effective_from: md.effective_from,
        }
    }
}

impl From<&ElementReference> for ElementReferenceDto {
    fn from(r: &ElementReference) -> Self {
        ElementReferenceDto {
            source_id: r.source_id.to_string(),
            element_id: r.element_id.to_string(),
        }
    }
}

impl From<&ElementValue> for ElementValueDto {
    fn from(v: &ElementValue) -> Self {
        match v {
            ElementValue::Null => ElementValueDto::Null,
            ElementValue::Bool(b) => ElementValueDto::Bool(*b),
            ElementValue::Float(f) => ElementValueDto::Float(f.into_inner()),
            ElementValue::Integer(i) => ElementValueDto::Integer(*i),
            ElementValue::String(s) => ElementValueDto::String(s.to_string()),
            ElementValue::List(items) => {
                ElementValueDto::List(items.iter().map(Into::into).collect())
            }
            ElementValue::Object(map) => ElementValueDto::Object(map.into()),
        }
    }
}

impl From<&ElementPropertyMap> for PropertyMapDto {
    fn from(map: &ElementPropertyMap) -> Self {
        let entries: Vec<(String, ElementValueDto)> =
            map.map_iter(|k, v| (k.to_string(), v.into())).collect();
        PropertyMapDto { entries }
    }
}

// ---------------------------------------------------------------------------
// DTO -> Core
// ---------------------------------------------------------------------------

impl From<SourceChangeDto> for SourceChange {
    fn from(dto: SourceChangeDto) -> Self {
        match dto {
            SourceChangeDto::Insert { element } => SourceChange::Insert {
                element: element.into(),
            },
            SourceChangeDto::Update { element } => SourceChange::Update {
                element: element.into(),
            },
            SourceChangeDto::Delete { metadata } => SourceChange::Delete {
                metadata: metadata.into(),
            },
        }
    }
}

impl From<ElementDto> for Element {
    fn from(dto: ElementDto) -> Self {
        match dto {
            ElementDto::Node {
                metadata,
                properties,
            } => Element::Node {
                metadata: metadata.into(),
                properties: properties.into(),
            },
            ElementDto::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => Element::Relation {
                metadata: metadata.into(),
                in_node: in_node.into(),
                out_node: out_node.into(),
                properties: properties.into(),
            },
        }
    }
}

impl From<ElementMetadataDto> for ElementMetadata {
    fn from(dto: ElementMetadataDto) -> Self {
        let labels: Vec<Arc<str>> = dto.labels.into_iter().map(Arc::from).collect();
        ElementMetadata {
            reference: dto.reference.into(),
            labels: Arc::from(labels),
            effective_from: dto.effective_from,
        }
    }
}

impl From<ElementReferenceDto> for ElementReference {
    fn from(dto: ElementReferenceDto) -> Self {
        ElementReference::new(&dto.source_id, &dto.element_id)
    }
}

impl From<ElementValueDto> for ElementValue {
    fn from(dto: ElementValueDto) -> Self {
        match dto {
            ElementValueDto::Null => ElementValue::Null,
            ElementValueDto::Bool(b) => ElementValue::Bool(b),
            ElementValueDto::Float(f) => ElementValue::Float(OrderedFloat(f)),
            ElementValueDto::Integer(i) => ElementValue::Integer(i),
            ElementValueDto::String(s) => ElementValue::String(Arc::from(s)),
            ElementValueDto::List(items) => {
                ElementValue::List(items.into_iter().map(Into::into).collect())
            }
            ElementValueDto::Object(map) => ElementValue::Object(map.into()),
        }
    }
}

impl From<PropertyMapDto> for ElementPropertyMap {
    fn from(dto: PropertyMapDto) -> Self {
        let mut map = ElementPropertyMap::new();
        for (key, value) in dto.entries {
            map.insert(&key, value.into());
        }
        map
    }
}
