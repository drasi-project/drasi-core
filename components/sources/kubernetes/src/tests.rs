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

use crate::config::{default_annotation_excludes, is_supported_kind, KubernetesSourceConfig};
use crate::mapping::{
    build_delete_changes, build_insert_changes, build_update_changes, element_id_for,
};
use crate::properties::build_node_properties;
use drasi_core::models::{Element, ElementValue, SourceChange};
use kube::api::DynamicObject;
use serde_json::json;

fn sample_config() -> KubernetesSourceConfig {
    KubernetesSourceConfig {
        resources: vec![crate::ResourceSpec {
            api_version: "v1".to_string(),
            kind: "Pod".to_string(),
        }],
        namespaces: vec!["default".to_string()],
        include_owner_relations: true,
        exclude_annotations: default_annotation_excludes(),
        ..KubernetesSourceConfig::default()
    }
}

fn pod_obj() -> DynamicObject {
    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": "pod-1",
            "namespace": "default",
            "uid": "uid-pod-1",
            "resourceVersion": "42",
            "labels": {"app":"myapp"},
            "annotations": {
                "kubectl.kubernetes.io/last-applied-configuration": "large",
                "custom.io/note": "keep"
            },
            "ownerReferences": [{
                "apiVersion": "apps/v1",
                "kind": "ReplicaSet",
                "name": "rs-1",
                "uid": "uid-rs-1"
            }]
        },
        "spec": {
            "nodeName": "node-a",
            "containers": [
                {
                    "name": "nginx",
                    "image": "nginx:1.21",
                    "ports": [{"containerPort": 80, "protocol": "TCP"}],
                    "resources": {
                        "requests": {"cpu":"100m"},
                        "limits": {"memory":"256Mi"}
                    }
                },
                {"name": "sidecar", "image": "envoy:v1"}
            ]
        },
        "status": {
            "phase": "Running",
            "podIP": "10.1.1.1",
            "hostIP": "192.168.1.10",
            "containerStatuses": [{
                "name": "nginx",
                "ready": true,
                "restartCount": 0,
                "image": "nginx:1.21",
                "state": {"running":{}}
            }]
        }
    }))
    .expect("valid pod object")
}

fn configmap_obj() -> DynamicObject {
    serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": "cm-1",
            "namespace": "default",
            "uid": "uid-cm-1",
            "resourceVersion": "7"
        },
        "data": {
            "database_url": "postgres://db",
            "log_level": "info"
        }
    }))
    .expect("valid configmap object")
}

#[test]
fn test_supported_kind_validation() {
    assert!(is_supported_kind("Pod"));
    assert!(is_supported_kind("Deployment"));
    assert!(!is_supported_kind("CustomResourceDefinition"));
}

#[test]
fn test_config_validation_requires_resources() {
    let cfg = KubernetesSourceConfig::default();
    assert!(cfg.validate().is_err());
}

#[test]
fn test_element_id_format() {
    assert_eq!(element_id_for("Pod", "abc"), "pod:abc");
}

#[test]
fn test_pod_labels_as_object() {
    let cfg = sample_config();
    let props = build_node_properties(&pod_obj(), "Pod", &cfg).unwrap();
    match props.get("labels").expect("labels property") {
        ElementValue::Object(map) => {
            assert_eq!(map.get("app"), Some(&ElementValue::String("myapp".into())));
        }
        _ => panic!("labels should be object"),
    }
}

#[test]
fn test_annotation_filtering() {
    let cfg = sample_config();
    let props = build_node_properties(&pod_obj(), "Pod", &cfg).unwrap();
    match props.get("annotations").expect("annotations property") {
        ElementValue::Object(map) => {
            assert!(map
                .get("kubectl.kubernetes.io/last-applied-configuration")
                .is_none());
            assert_eq!(
                map.get("custom.io/note"),
                Some(&ElementValue::String("keep".into()))
            );
        }
        _ => panic!("annotations should be object"),
    }
}

#[test]
fn test_pod_containers_as_list_of_objects() {
    let cfg = sample_config();
    let props = build_node_properties(&pod_obj(), "Pod", &cfg).unwrap();
    match props.get("containers").expect("containers property") {
        ElementValue::List(items) => {
            assert_eq!(items.len(), 2);
            match &items[0] {
                ElementValue::Object(first) => {
                    assert_eq!(
                        first.get("image"),
                        Some(&ElementValue::String("nginx:1.21".into()))
                    );
                }
                _ => panic!("container should be object"),
            }
        }
        _ => panic!("containers should be list"),
    }
}

#[test]
fn test_pod_container_images_denormalized() {
    let cfg = sample_config();
    let props = build_node_properties(&pod_obj(), "Pod", &cfg).unwrap();
    match props
        .get("containerImages")
        .expect("containerImages property")
    {
        ElementValue::List(items) => {
            assert!(items.contains(&ElementValue::String("nginx:1.21".into())));
            assert!(items.contains(&ElementValue::String("envoy:v1".into())));
        }
        _ => panic!("containerImages should be list"),
    }
}

#[test]
fn test_pod_container_statuses() {
    let cfg = sample_config();
    let props = build_node_properties(&pod_obj(), "Pod", &cfg).unwrap();
    match props
        .get("containerStatuses")
        .expect("containerStatuses property")
    {
        ElementValue::List(items) => {
            assert_eq!(items.len(), 1);
        }
        _ => panic!("containerStatuses should be list"),
    }
}

#[test]
fn test_configmap_data_as_object() {
    let cfg = sample_config();
    let props = build_node_properties(&configmap_obj(), "ConfigMap", &cfg).unwrap();
    match props.get("data").expect("data property") {
        ElementValue::Object(map) => {
            assert_eq!(
                map.get("database_url"),
                Some(&ElementValue::String("postgres://db".into()))
            );
        }
        _ => panic!("data should be object"),
    }
}

#[test]
fn test_configmap_datakeys_as_list() {
    let cfg = sample_config();
    let props = build_node_properties(&configmap_obj(), "ConfigMap", &cfg).unwrap();
    match props.get("dataKeys").expect("dataKeys property") {
        ElementValue::List(items) => {
            assert!(items.contains(&ElementValue::String("database_url".into())));
            assert!(items.contains(&ElementValue::String("log_level".into())));
        }
        _ => panic!("dataKeys should be list"),
    }
}

#[test]
fn test_insert_change_contains_node_element() {
    let cfg = sample_config();
    let changes = build_insert_changes("src1", "Pod", &pod_obj(), &cfg).unwrap();
    assert!(!changes.is_empty());
    assert!(matches!(
        changes.first().expect("insert change"),
        SourceChange::Insert {
            element: Element::Node { .. }
        }
    ));
}

#[test]
fn test_update_change_contains_node_element() {
    let cfg = sample_config();
    let changes = build_update_changes("src1", "Pod", &pod_obj(), &cfg).unwrap();
    assert!(matches!(
        changes.first().expect("update change"),
        SourceChange::Update {
            element: Element::Node { .. }
        }
    ));
}

#[test]
fn test_delete_change_contains_delete_metadata() {
    let cfg = sample_config();
    let changes = build_delete_changes("src1", "Pod", &pod_obj(), &cfg).unwrap();
    assert!(matches!(
        changes.first().expect("delete change"),
        SourceChange::Delete { .. }
    ));
}

#[test]
fn test_owner_relation_construction() {
    let cfg = sample_config();
    let changes = build_insert_changes("src1", "Pod", &pod_obj(), &cfg).unwrap();
    assert!(changes.iter().any(|c| matches!(
        c,
        SourceChange::Update {
            element: Element::Relation { .. }
        }
    )));
}

#[test]
fn test_service_ports_as_list() {
    let obj: DynamicObject = serde_json::from_value(json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name":"svc-1", "namespace":"default", "uid":"uid-svc-1"},
        "spec": {
            "type": "ClusterIP",
            "clusterIP": "10.10.10.10",
            "ports": [{"port": 80, "protocol":"TCP"}],
            "selector": {"app":"myapp"}
        }
    }))
    .unwrap();
    let cfg = sample_config();
    let props = build_node_properties(&obj, "Service", &cfg).unwrap();
    assert!(matches!(props.get("ports"), Some(ElementValue::List(_))));
}

#[test]
fn test_deployment_replica_counts() {
    let obj: DynamicObject = serde_json::from_value(json!({
        "apiVersion":"apps/v1",
        "kind":"Deployment",
        "metadata":{"name":"dep-1","namespace":"default","uid":"uid-dep-1"},
        "spec":{"replicas": 3, "template":{"spec":{"containers":[{"name":"api","image":"api:v1"}]}}},
        "status":{"readyReplicas": 2, "availableReplicas": 2, "updatedReplicas": 2}
    }))
    .unwrap();
    let cfg = sample_config();
    let props = build_node_properties(&obj, "Deployment", &cfg).unwrap();
    assert_eq!(
        props.get("desiredReplicas"),
        Some(&ElementValue::Integer(3))
    );
    assert_eq!(props.get("readyReplicas"), Some(&ElementValue::Integer(2)));
}

#[test]
fn test_crd_fallback_to_json_strings_for_unknown_kind() {
    let obj: DynamicObject = serde_json::from_value(json!({
        "apiVersion":"example.com/v1",
        "kind":"Widget",
        "metadata":{"name":"w-1","namespace":"default","uid":"uid-widget-1"},
        "spec":{"size":"large"},
        "status":{"phase":"ok"}
    }))
    .unwrap();
    let cfg = sample_config();
    let props = build_node_properties(&obj, "Widget", &cfg).unwrap();
    assert!(matches!(props.get("spec"), Some(ElementValue::String(_))));
    assert!(matches!(props.get("status"), Some(ElementValue::String(_))));
}
