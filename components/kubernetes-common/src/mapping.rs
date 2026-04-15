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

use crate::config::KubernetesSourceConfig;
use crate::properties::build_node_properties;
use chrono::Utc;
use drasi_core::models::{
    Element, ElementMetadata, ElementReference, SourceChange, MAX_REASONABLE_MILLIS_TIMESTAMP,
};
use kube::api::DynamicObject;
use std::sync::Arc;

pub fn build_insert_changes(
    source_id: &str,
    kind: &str,
    obj: &DynamicObject,
    config: &KubernetesSourceConfig,
) -> anyhow::Result<Vec<SourceChange>> {
    let mut changes = vec![SourceChange::Insert {
        element: build_node_element(source_id, kind, obj, config)?,
    }];

    if config.include_owner_relations {
        changes.extend(build_owner_relation_upserts(source_id, kind, obj, config));
    }

    Ok(changes)
}

pub fn build_update_changes(
    source_id: &str,
    kind: &str,
    obj: &DynamicObject,
    config: &KubernetesSourceConfig,
) -> anyhow::Result<Vec<SourceChange>> {
    let mut changes = vec![SourceChange::Update {
        element: build_node_element(source_id, kind, obj, config)?,
    }];

    if config.include_owner_relations {
        changes.extend(build_owner_relation_upserts(source_id, kind, obj, config));
    }

    Ok(changes)
}

pub fn build_delete_changes(
    source_id: &str,
    kind: &str,
    obj: &DynamicObject,
    config: &KubernetesSourceConfig,
) -> anyhow::Result<Vec<SourceChange>> {
    let uid = obj
        .metadata
        .uid
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Kubernetes object is missing metadata.uid"))?;

    let metadata = ElementMetadata {
        reference: ElementReference::new(source_id, &element_id_for(kind, &uid)),
        labels: Arc::from([Arc::<str>::from(kind)]),
        effective_from: Utc::now().timestamp_millis() as u64,
    };

    let mut changes = vec![SourceChange::Delete { metadata }];

    if config.include_owner_relations {
        changes.extend(build_owner_relation_deletes(source_id, obj));
    }

    Ok(changes)
}

pub fn element_id_for(kind: &str, uid: &str) -> String {
    format!("{}:{uid}", kind.to_lowercase())
}

pub fn extract_uid(obj: &DynamicObject) -> Option<String> {
    obj.metadata.uid.clone()
}

pub fn extract_resource_version(obj: &DynamicObject) -> Option<String> {
    obj.metadata.resource_version.clone()
}

pub fn object_created_at_millis(obj: &DynamicObject) -> Option<i64> {
    obj.metadata
        .creation_timestamp
        .as_ref()
        .map(|t| t.0.timestamp_millis())
}

fn build_node_element(
    source_id: &str,
    kind: &str,
    obj: &DynamicObject,
    config: &KubernetesSourceConfig,
) -> anyhow::Result<Element> {
    let uid = obj
        .metadata
        .uid
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Kubernetes object is missing metadata.uid"))?;
    let effective_from = Utc::now().timestamp_millis() as u64;

    if effective_from > MAX_REASONABLE_MILLIS_TIMESTAMP {
        return Err(anyhow::anyhow!(
            "Internal timestamp error: effective_from {effective_from} exceeds reasonable millis range"
        ));
    }

    let metadata = ElementMetadata {
        reference: ElementReference::new(source_id, &element_id_for(kind, &uid)),
        labels: Arc::from([Arc::<str>::from(kind)]),
        effective_from,
    };

    let properties = build_node_properties(obj, kind, config)?;
    Ok(Element::Node {
        metadata,
        properties,
    })
}

fn build_owner_relation_upserts(
    source_id: &str,
    child_kind: &str,
    obj: &DynamicObject,
    config: &KubernetesSourceConfig,
) -> Vec<SourceChange> {
    let child_uid = match obj.metadata.uid.clone() {
        Some(uid) => uid,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    let owners = obj.metadata.owner_references.clone().unwrap_or_default();
    for owner in owners {
        // Skip owner relations where the owner kind is not a configured resource
        if !config.resources.iter().any(|r| r.kind == owner.kind) {
            continue;
        }
        let owner_uid = owner.uid.clone();
        let owner_kind = owner.kind.clone();
        let rel_element = build_owner_relation_element(
            source_id,
            &owner_uid,
            &owner_kind,
            &child_uid,
            child_kind,
            owner.api_version.as_str(),
        );
        out.push(SourceChange::Update {
            element: rel_element,
        });
    }

    out
}

fn build_owner_relation_deletes(source_id: &str, obj: &DynamicObject) -> Vec<SourceChange> {
    let child_uid = match obj.metadata.uid.clone() {
        Some(uid) => uid,
        None => return Vec::new(),
    };

    let mut out = Vec::new();
    let owners = obj.metadata.owner_references.clone().unwrap_or_default();
    for owner in owners {
        let rel_element_id = format!("owns:{}:{}", owner.uid, child_uid);
        let metadata = ElementMetadata {
            reference: ElementReference::new(source_id, &rel_element_id),
            labels: Arc::from([Arc::<str>::from("OWNS")]),
            effective_from: Utc::now().timestamp_millis() as u64,
        };
        out.push(SourceChange::Delete { metadata });
    }
    out
}

fn build_owner_relation_element(
    source_id: &str,
    owner_uid: &str,
    owner_kind: &str,
    child_uid: &str,
    child_kind: &str,
    owner_api_version: &str,
) -> Element {
    let rel_element_id = format!("owns:{owner_uid}:{child_uid}");
    let metadata = ElementMetadata {
        reference: ElementReference::new(source_id, &rel_element_id),
        labels: Arc::from([Arc::<str>::from("OWNS")]),
        effective_from: Utc::now().timestamp_millis() as u64,
    };

    let in_node = ElementReference::new(source_id, &element_id_for(owner_kind, owner_uid));
    let out_node = ElementReference::new(source_id, &element_id_for(child_kind, child_uid));

    let mut props = drasi_core::models::ElementPropertyMap::new();
    props.insert(
        "ownerKind",
        drasi_core::models::ElementValue::from(&serde_json::json!(owner_kind)),
    );
    props.insert(
        "ownerApiVersion",
        drasi_core::models::ElementValue::from(&serde_json::json!(owner_api_version)),
    );

    Element::Relation {
        metadata,
        in_node,
        out_node,
        properties: props,
    }
}
