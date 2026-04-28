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

//! Shared helpers for building graph [`Element`]s from the component graph.
//!
//! Used by both [`ComponentGraphSource`](super::component_graph_source) (live
//! change events) and
//! [`ComponentGraphBootstrapProvider`](crate::bootstrap::component_graph)
//! (snapshot bootstrap).

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
};
use std::sync::Arc;

use super::component_graph_source::COMPONENT_GRAPH_SOURCE_ID;
use crate::channels::ComponentStatus;

/// Create a node [`Element`] referencing the component graph source.
pub(crate) fn make_node(element_id: &str, labels: &[&str], props: ElementPropertyMap) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(COMPONENT_GRAPH_SOURCE_ID, element_id),
            labels: labels
                .iter()
                .map(|l| Arc::from(*l))
                .collect::<Vec<_>>()
                .into(),
            effective_from: now_ms(),
        },
        properties: props,
    }
}

/// Create a relation [`Element`] referencing the component graph source.
pub(crate) fn make_relation(
    element_id: &str,
    labels: &[&str],
    in_node_id: &str,
    out_node_id: &str,
    props: ElementPropertyMap,
) -> Element {
    Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new(COMPONENT_GRAPH_SOURCE_ID, element_id),
            labels: labels
                .iter()
                .map(|l| Arc::from(*l))
                .collect::<Vec<_>>()
                .into(),
            effective_from: now_ms(),
        },
        in_node: ElementReference::new(COMPONENT_GRAPH_SOURCE_ID, in_node_id),
        out_node: ElementReference::new(COMPONENT_GRAPH_SOURCE_ID, out_node_id),
        properties: props,
    }
}

/// Create an [`ElementMetadata`] for delete operations.
pub(crate) fn make_delete_metadata(element_id: &str, labels: &[&str]) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(COMPONENT_GRAPH_SOURCE_ID, element_id),
        labels: labels
            .iter()
            .map(|l| Arc::from(*l))
            .collect::<Vec<_>>()
            .into(),
        effective_from: now_ms(),
    }
}

/// Current wall-clock time in milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Human-readable string for a [`ComponentStatus`].
pub(crate) fn status_str(status: &ComponentStatus) -> &'static str {
    match status {
        ComponentStatus::Added => "Added",
        ComponentStatus::Stopped => "Stopped",
        ComponentStatus::Starting => "Starting",
        ComponentStatus::Running => "Running",
        ComponentStatus::Stopping => "Stopping",
        ComponentStatus::Removed => "Removed",
        ComponentStatus::Reconfiguring => "Reconfiguring",
        ComponentStatus::Error => "Error",
    }
}

/// Build an [`ElementPropertyMap`] from `(key, value)` string pairs.
pub(crate) fn build_props(pairs: &[(&str, &str)]) -> ElementPropertyMap {
    let mut props = ElementPropertyMap::new();
    for (k, v) in pairs {
        props.insert(k, ElementValue::String(Arc::from(*v)));
    }
    props
}
