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

use super::StoredValueMap;
use drasi_core::models::{Element, ElementMetadata, ElementReference};
use std::sync::Arc;

#[derive(Clone, prost::Message, Hash)]
pub struct StoredElementReference {
    #[prost(string, tag = "1")]
    pub source_id: String,
    #[prost(string, tag = "2")]
    pub element_id: String,
}

#[derive(Clone, prost::Message)]
pub struct StoredElementMetadata {
    #[prost(message, required, tag = "1")]
    pub reference: StoredElementReference,

    #[prost(string, repeated, tag = "2")]
    pub labels: Vec<String>,

    #[prost(uint64, tag = "3")]
    pub effective_from: u64,
}

#[derive(Clone, ::prost::Message)]
pub struct StoredElementContainer {
    #[prost(oneof = "StoredElement", tags = "1, 2")]
    pub element: ::core::option::Option<StoredElement>,
}

#[derive(Clone, ::prost::Oneof)]
pub enum StoredElement {
    #[prost(message, tag = "1")]
    Node(StoredNode),
    #[prost(message, tag = "2")]
    Relation(StoredRelation),
}

#[derive(Clone, prost::Message)]
pub struct StoredNode {
    #[prost(message, required, tag = "1")]
    pub metadata: StoredElementMetadata,
    #[prost(message, required, tag = "2")]
    pub properties: StoredValueMap,
}

#[derive(Clone, prost::Message)]
pub struct StoredRelation {
    #[prost(message, required, tag = "1")]
    pub metadata: StoredElementMetadata,
    #[prost(message, required, tag = "2")]
    pub properties: StoredValueMap,
    #[prost(message, required, tag = "3")]
    pub in_node: StoredElementReference,
    #[prost(message, required, tag = "4")]
    pub out_node: StoredElementReference,
}

impl StoredElement {
    pub fn get_reference(&self) -> &StoredElementReference {
        match self {
            StoredElement::Node(e) => &e.metadata.reference,
            StoredElement::Relation(e) => &e.metadata.reference,
        }
    }

    pub fn get_effective_from(&self) -> u64 {
        match self {
            StoredElement::Node(e) => e.metadata.effective_from,
            StoredElement::Relation(e) => e.metadata.effective_from,
        }
    }
}

impl From<&ElementReference> for StoredElementReference {
    fn from(reference: &ElementReference) -> Self {
        StoredElementReference {
            source_id: reference.source_id.to_string(),
            element_id: reference.element_id.to_string(),
        }
    }
}

impl From<&ElementMetadata> for StoredElementMetadata {
    fn from(metadata: &ElementMetadata) -> Self {
        let r = &metadata.reference;
        StoredElementMetadata {
            reference: r.into(),
            labels: metadata.labels.iter().map(|l| l.to_string()).collect(),
            effective_from: metadata.effective_from,
        }
    }
}

impl From<&Element> for StoredElement {
    fn from(element: &Element) -> Self {
        match element {
            Element::Node {
                metadata,
                properties,
            } => StoredElement::Node(StoredNode {
                metadata: metadata.into(),
                properties: properties.into(),
            }),
            Element::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => StoredElement::Relation(StoredRelation {
                metadata: metadata.into(),
                in_node: in_node.into(),
                out_node: out_node.into(),
                properties: properties.into(),
            }),
        }
    }
}

impl From<StoredElementReference> for ElementReference {
    fn from(val: StoredElementReference) -> Self {
        ElementReference {
            source_id: Arc::from(val.source_id),
            element_id: Arc::from(val.element_id),
        }
    }
}

impl From<StoredElementMetadata> for ElementMetadata {
    fn from(val: StoredElementMetadata) -> Self {
        ElementMetadata {
            reference: val.reference.into(),
            labels: val.labels.iter().map(|l| Arc::from(l.as_str())).collect(),
            effective_from: val.effective_from,
        }
    }
}

impl From<StoredElement> for Element {
    fn from(val: StoredElement) -> Self {
        match val {
            StoredElement::Node(e) => Element::Node {
                metadata: e.metadata.into(),
                properties: e.properties.into(),
            },
            StoredElement::Relation(e) => Element::Relation {
                metadata: e.metadata.into(),
                in_node: e.in_node.into(),
                out_node: e.out_node.into(),
                properties: e.properties.into(),
            },
        }
    }
}
