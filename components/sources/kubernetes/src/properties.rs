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
use drasi_core::models::{ElementPropertyMap, ElementValue};
use kube::api::DynamicObject;
use serde_json::{json, Map, Value};

pub fn build_node_properties(
    obj: &DynamicObject,
    kind: &str,
    config: &KubernetesSourceConfig,
) -> anyhow::Result<ElementPropertyMap> {
    let full: Value = serde_json::to_value(obj)?;
    let mut props = ElementPropertyMap::new();

    let name = obj.metadata.name.clone().unwrap_or_default();
    let namespace = obj.metadata.namespace.clone().unwrap_or_default();
    let uid = obj.metadata.uid.clone().unwrap_or_default();
    let resource_version = obj.metadata.resource_version.clone().unwrap_or_default();
    let generation = obj.metadata.generation.unwrap_or_default();
    let creation_timestamp = obj
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| t.0.to_rfc3339())
        .unwrap_or_default();
    let deletion_timestamp = obj
        .metadata
        .deletion_timestamp
        .as_ref()
        .map(|t| t.0.to_rfc3339())
        .unwrap_or_default();

    props.insert("name", ElementValue::from(&json!(name)));
    props.insert("namespace", ElementValue::from(&json!(namespace)));
    props.insert("uid", ElementValue::from(&json!(uid)));
    props.insert(
        "resourceVersion",
        ElementValue::from(&json!(resource_version)),
    );
    props.insert("generation", ElementValue::from(&json!(generation)));
    props.insert(
        "creationTimestamp",
        ElementValue::from(&json!(creation_timestamp)),
    );
    props.insert(
        "deletionTimestamp",
        ElementValue::from(&json!(deletion_timestamp)),
    );

    let labels = obj.metadata.labels.clone().unwrap_or_default();
    let labels_value = serde_json::to_value(labels)?;
    props.insert("labels", ElementValue::from(&labels_value));

    let mut annotations = obj.metadata.annotations.clone().unwrap_or_default();
    for k in &config.exclude_annotations {
        annotations.remove(k);
    }
    let annotations_value = serde_json::to_value(annotations)?;
    props.insert("annotations", ElementValue::from(&annotations_value));

    add_kind_specific_properties(&mut props, kind, &full);

    Ok(props)
}

fn add_kind_specific_properties(props: &mut ElementPropertyMap, kind: &str, full: &Value) {
    let spec = full.get("spec").unwrap_or(&Value::Null);
    let status = full.get("status").unwrap_or(&Value::Null);

    match kind {
        "Pod" => add_pod_properties(props, spec, status),
        "Deployment" => add_deployment_properties(props, spec, status),
        "ReplicaSet" => add_replicaset_properties(props, spec, status),
        "Node" => add_node_properties(props, spec, status),
        "Service" => add_service_properties(props, spec, status),
        "ConfigMap" => add_configmap_properties(props, full),
        "Namespace" => add_namespace_properties(props, status),
        _ => {
            props.insert("spec", ElementValue::from(&to_json_string(spec)));
            props.insert("status", ElementValue::from(&to_json_string(status)));
        }
    }
}

fn add_pod_properties(props: &mut ElementPropertyMap, spec: &Value, status: &Value) {
    add_string_property(props, "phase", status.get("phase"));
    add_string_property(props, "nodeName", spec.get("nodeName"));
    add_string_property(props, "hostIP", status.get("hostIP"));
    add_string_property(props, "podIP", status.get("podIP"));
    add_string_property(props, "startTime", status.get("startTime"));

    let containers = spec
        .get("containers")
        .and_then(Value::as_array)
        .map(|v| build_container_list(v))
        .unwrap_or_default();
    props.insert(
        "containers",
        ElementValue::from(&Value::Array(containers.clone())),
    );

    let init_containers = spec
        .get("initContainers")
        .and_then(Value::as_array)
        .map(|v| build_container_list(v))
        .unwrap_or_default();
    props.insert(
        "initContainers",
        ElementValue::from(&Value::Array(init_containers)),
    );

    let container_names: Vec<Value> = containers
        .iter()
        .filter_map(|v| v.get("name").cloned())
        .collect();
    props.insert(
        "containerNames",
        ElementValue::from(&Value::Array(container_names)),
    );

    let container_images: Vec<Value> = containers
        .iter()
        .filter_map(|v| v.get("image").cloned())
        .collect();
    props.insert(
        "containerImages",
        ElementValue::from(&Value::Array(container_images)),
    );

    props.insert(
        "containerCount",
        ElementValue::from(&json!(containers.len() as i64)),
    );

    let statuses = status
        .get("containerStatuses")
        .and_then(Value::as_array)
        .map(|v| build_container_status_list(v))
        .unwrap_or_default();
    props.insert(
        "containerStatuses",
        ElementValue::from(&Value::Array(statuses)),
    );
}

fn add_deployment_properties(props: &mut ElementPropertyMap, spec: &Value, status: &Value) {
    add_i64_property(props, "desiredReplicas", spec.get("replicas"));
    add_i64_property(props, "readyReplicas", status.get("readyReplicas"));
    add_i64_property(props, "availableReplicas", status.get("availableReplicas"));
    add_i64_property(props, "updatedReplicas", status.get("updatedReplicas"));
    add_string_property(
        props,
        "strategy",
        spec.get("strategy").and_then(|s| s.get("type")),
    );

    let containers = spec
        .get("template")
        .and_then(|t| t.get("spec"))
        .and_then(|s| s.get("containers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let names = containers
        .iter()
        .filter_map(|c| c.get("name").cloned())
        .collect::<Vec<_>>();
    let images = containers
        .iter()
        .filter_map(|c| c.get("image").cloned())
        .collect::<Vec<_>>();
    props.insert("containerNames", ElementValue::from(&Value::Array(names)));
    props.insert("containerImages", ElementValue::from(&Value::Array(images)));
}

fn add_replicaset_properties(props: &mut ElementPropertyMap, spec: &Value, status: &Value) {
    add_i64_property(props, "desiredReplicas", spec.get("replicas"));
    add_i64_property(props, "readyReplicas", status.get("readyReplicas"));
    add_i64_property(props, "availableReplicas", status.get("availableReplicas"));

    let containers = spec
        .get("template")
        .and_then(|t| t.get("spec"))
        .and_then(|s| s.get("containers"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let images = containers
        .iter()
        .filter_map(|c| c.get("image").cloned())
        .collect::<Vec<_>>();
    props.insert("containerImages", ElementValue::from(&Value::Array(images)));
}

fn add_node_properties(props: &mut ElementPropertyMap, spec: &Value, status: &Value) {
    add_bool_property(props, "unschedulable", spec.get("unschedulable"));
    add_string_property(props, "podCIDR", spec.get("podCIDR"));
    add_string_property(
        props,
        "kubeletVersion",
        status.get("nodeInfo").and_then(|n| n.get("kubeletVersion")),
    );
    add_string_property(
        props,
        "osImage",
        status.get("nodeInfo").and_then(|n| n.get("osImage")),
    );
    add_string_property(
        props,
        "architecture",
        status.get("nodeInfo").and_then(|n| n.get("architecture")),
    );

    let alloc = status
        .get("allocatable")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    add_string_property(props, "allocatableCpu", alloc.get("cpu"));
    add_string_property(props, "allocatableMemory", alloc.get("memory"));

    let ready = status
        .get("conditions")
        .and_then(Value::as_array)
        .map(|conds| {
            conds.iter().any(|c| {
                c.get("type").and_then(Value::as_str) == Some("Ready")
                    && c.get("status").and_then(Value::as_str) == Some("True")
            })
        })
        .unwrap_or(false);
    props.insert("ready", ElementValue::Bool(ready));
}

fn add_service_properties(props: &mut ElementPropertyMap, spec: &Value, status: &Value) {
    add_string_property(props, "serviceType", spec.get("type"));
    add_string_property(props, "clusterIP", spec.get("clusterIP"));
    if let Some(lb_ip) = status
        .get("loadBalancer")
        .and_then(|lb| lb.get("ingress"))
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|first| first.get("ip"))
    {
        add_string_property(props, "loadBalancerIP", Some(lb_ip));
    }

    let ports = spec
        .get("ports")
        .and_then(Value::as_array)
        .map(|arr| arr.to_vec())
        .unwrap_or_default();
    props.insert("ports", ElementValue::from(&Value::Array(ports)));

    let selector = spec
        .get("selector")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    props.insert(
        "selector",
        ElementValue::from(&Value::Object(Map::<String, Value>::from_iter(selector))),
    );
}

fn add_configmap_properties(props: &mut ElementPropertyMap, full: &Value) {
    let data = full
        .get("data")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let binary_data = full
        .get("binaryData")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let data_keys = data.keys().cloned().map(Value::String).collect::<Vec<_>>();
    props.insert("dataKeys", ElementValue::from(&Value::Array(data_keys)));

    let binary_keys = binary_data
        .keys()
        .cloned()
        .map(Value::String)
        .collect::<Vec<_>>();
    props.insert(
        "binaryDataKeys",
        ElementValue::from(&Value::Array(binary_keys)),
    );

    props.insert(
        "data",
        ElementValue::from(&Value::Object(Map::<String, Value>::from_iter(data))),
    );
}

fn add_namespace_properties(props: &mut ElementPropertyMap, status: &Value) {
    add_string_property(props, "phase", status.get("phase"));
}

fn build_container_list(containers: &[Value]) -> Vec<Value> {
    containers
        .iter()
        .map(|c| {
            let mut container = Map::new();
            insert_if_present(&mut container, "name", c.get("name"));
            insert_if_present(&mut container, "image", c.get("image"));
            insert_if_present(&mut container, "imagePullPolicy", c.get("imagePullPolicy"));
            insert_if_present(&mut container, "ports", c.get("ports"));

            let mut resources = Map::new();
            if let Some(req) = c
                .get("resources")
                .and_then(|r| r.get("requests"))
                .and_then(Value::as_object)
            {
                resources.insert("requests".to_string(), Value::Object(req.clone()));
            }
            if let Some(limits) = c
                .get("resources")
                .and_then(|r| r.get("limits"))
                .and_then(Value::as_object)
            {
                resources.insert("limits".to_string(), Value::Object(limits.clone()));
            }
            container.insert("resources".to_string(), Value::Object(resources));
            Value::Object(container)
        })
        .collect()
}

fn build_container_status_list(statuses: &[Value]) -> Vec<Value> {
    statuses
        .iter()
        .map(|s| {
            let state = s
                .get("state")
                .and_then(Value::as_object)
                .and_then(|obj| obj.keys().next().cloned())
                .unwrap_or_default();
            let reason = s
                .get("state")
                .and_then(|st| st.get("waiting"))
                .and_then(|w| w.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            json!({
                "name": s.get("name").and_then(Value::as_str).unwrap_or_default(),
                "ready": s.get("ready").and_then(Value::as_bool).unwrap_or(false),
                "restartCount": s.get("restartCount").and_then(Value::as_i64).unwrap_or_default(),
                "image": s.get("image").and_then(Value::as_str).unwrap_or_default(),
                "state": state,
                "reason": reason
            })
        })
        .collect()
}

fn insert_if_present(map: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(v) = value {
        map.insert(key.to_string(), v.clone());
    }
}

fn to_json_string(v: &Value) -> Value {
    Value::String(serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()))
}

fn add_string_property(props: &mut ElementPropertyMap, key: &str, value: Option<&Value>) {
    let val = value
        .and_then(Value::as_str)
        .map_or_else(String::new, ToString::to_string);
    props.insert(key, ElementValue::from(&json!(val)));
}

fn add_i64_property(props: &mut ElementPropertyMap, key: &str, value: Option<&Value>) {
    let val = value.and_then(Value::as_i64).unwrap_or_default();
    props.insert(key, ElementValue::from(&json!(val)));
}

fn add_bool_property(props: &mut ElementPropertyMap, key: &str, value: Option<&Value>) {
    let val = value.and_then(Value::as_bool).unwrap_or(false);
    props.insert(key, ElementValue::from(&json!(val)));
}
