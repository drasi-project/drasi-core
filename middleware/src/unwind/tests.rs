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

mod process {
    use std::sync::Arc;

    use drasi_core::{
        in_memory_index::in_memory_element_index::InMemoryElementIndex,
        interface::{ElementIndex, SourceMiddlewareFactory},
        models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
            SourceMiddlewareConfig,
        },
    };
    use serde_json::json;

    use crate::unwind::UnwindFactory;

    #[tokio::test]
    pub async fn unwind_array_with_relations() {
        let factory = UnwindFactory::new();
        let config = json!({
            "Pod": [{
                "selector": "$.status.containerStatuses[*]",
                "label": "Container",
                "key": "$.containerID",
                "relation": "OWNS"
            }]
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "unwind".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();
        let element1 = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "p1"),
                labels: vec!["Pod".into()].into(),
                effective_from: 0,
            },
            properties: ElementPropertyMap::from(json!({
                "metadata": {
                    "name": "pod-1",
                    "namespace": "default",
                    "labels": {
                        "app": "nginx",
                        "env": "prod"
                    }
                },
                "status": {
                    "containerStatuses": [
                        {
                            "containerID": "c1",
                            "name": "nginx"
                        },
                        {
                            "containerID": "c2",
                            "name": "redis"
                        }
                    ]
                }
            })),
        };

        let result = subject
            .process(
                SourceChange::Insert {
                    element: element1.clone(),
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();

        assert_eq!(result.len(), 5);
        assert!(result.contains(&SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: vec!["Pod".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "metadata": {
                        "name": "pod-1",
                        "namespace": "default",
                        "labels": {
                            "app": "nginx",
                            "env": "prod"
                        }
                    },
                    "status": {
                        "containerStatuses": [
                            {
                                "containerID": "c1",
                                "name": "nginx"
                            },
                            {
                                "containerID": "c2",
                                "name": "redis"
                            }
                        ]
                    }
                }))
            }
        }));

        assert!(result.contains(&SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(
                        "test",
                        "$unwind-$.status.containerStatuses[*]-p1-c1"
                    ),
                    labels: vec!["Container".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "containerID": "c1",
                    "name": "nginx",
                }))
            }
        }));

        assert!(result.contains(&SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new(
                        "test",
                        "$unwind-$.status.containerStatuses[*]-p1-c1$rel"
                    ),
                    labels: vec!["OWNS".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::new(),
                out_node: ElementReference::new(
                    "test",
                    "$unwind-$.status.containerStatuses[*]-p1-c1"
                ),
                in_node: ElementReference::new("test", "p1"),
            }
        }));

        assert!(result.contains(&SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(
                        "test",
                        "$unwind-$.status.containerStatuses[*]-p1-c2"
                    ),
                    labels: vec!["Container".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "containerID": "c2",
                    "name": "redis",
                }))
            }
        }));

        assert!(result.contains(&SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new(
                        "test",
                        "$unwind-$.status.containerStatuses[*]-p1-c2$rel"
                    ),
                    labels: vec!["OWNS".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::new(),
                out_node: ElementReference::new(
                    "test",
                    "$unwind-$.status.containerStatuses[*]-p1-c2"
                ),
                in_node: ElementReference::new("test", "p1"),
            }
        }));

        element_index.set_element(&element1, &vec![]).await.unwrap();

        let result = subject
            .process(
                SourceChange::Update {
                    removed_properties: Default::default(),
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("test", "p1"),
                            labels: vec!["Pod".into()].into(),
                            effective_from: 0,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "metadata": {
                                "name": "pod-1",
                                "namespace": "default",
                                "labels": {
                                    "app": "nginx",
                                    "env": "prod"
                                }
                            },
                            "status": {
                                "containerStatuses": [
                                    {
                                        "containerID": "c2",
                                        "name": "redis"
                                    }
                                ]
                            }
                        })),
                    },
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();

        println!("Result: {result:#?}");

        assert!(result.contains(&SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new(
                    "test",
                    "$unwind-$.status.containerStatuses[*]-p1-c1"
                ),
                labels: vec!["Container".into()].into(),
                effective_from: 0
            },
        }));

        assert!(result.contains(&SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new(
                    "test",
                    "$unwind-$.status.containerStatuses[*]-p1-c1$rel"
                ),
                labels: vec!["OWNS".into()].into(),
                effective_from: 0
            },
        }));
    }

    #[tokio::test]
    pub async fn unwind_array_without_relations() {
        let factory = UnwindFactory::new();
        let config = json!({
            "Pod": [{
                "selector": "$.status.containerStatuses[*]",
                "label": "Container",
                "key": "$.containerID"
            }]
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "unwind".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let result = subject
            .process(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("test", "p1"),
                            labels: vec!["Pod".into()].into(),
                            effective_from: 0,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "metadata": {
                                "name": "pod-1",
                                "namespace": "default",
                                "labels": {
                                    "app": "nginx",
                                    "env": "prod"
                                }
                            },
                            "status": {
                                "containerStatuses": [
                                    {
                                        "containerID": "c1",
                                        "name": "nginx"
                                    },
                                    {
                                        "containerID": "c2",
                                        "name": "redis"
                                    }
                                ]
                            }
                        })),
                    },
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();

        assert_eq!(result.len(), 3);
        assert!(result.contains(&SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: vec!["Pod".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "metadata": {
                        "name": "pod-1",
                        "namespace": "default",
                        "labels": {
                            "app": "nginx",
                            "env": "prod"
                        }
                    },
                    "status": {
                        "containerStatuses": [
                            {
                                "containerID": "c1",
                                "name": "nginx"
                            },
                            {
                                "containerID": "c2",
                                "name": "redis"
                            }
                        ]
                    }
                }))
            }
        }));

        assert!(result.contains(&SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(
                        "test",
                        "$unwind-$.status.containerStatuses[*]-p1-c1"
                    ),
                    labels: vec!["Container".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "containerID": "c1",
                    "name": "nginx",
                }))
            }
        }));

        assert!(result.contains(&SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(
                        "test",
                        "$unwind-$.status.containerStatuses[*]-p1-c2"
                    ),
                    labels: vec!["Container".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "containerID": "c2",
                    "name": "redis",
                }))
            }
        }));
    }

    #[tokio::test]
    pub async fn unwind_array_without_key() {
        let factory = UnwindFactory::new();
        let config = json!({
            "Pod": [{
                "selector": "$.status.containerStatuses[*]",
                "label": "Container",
                "relation": "OWNS"
            }]
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "unwind".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let result = subject
            .process(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("test", "p1"),
                            labels: vec!["Pod".into()].into(),
                            effective_from: 0,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "metadata": {
                                "name": "pod-1",
                                "namespace": "default",
                                "labels": {
                                    "app": "nginx",
                                    "env": "prod"
                                }
                            },
                            "status": {
                                "containerStatuses": [
                                    {
                                        "containerID": "c1",
                                        "name": "nginx"
                                    },
                                    {
                                        "containerID": "c2",
                                        "name": "redis"
                                    }
                                ]
                            }
                        })),
                    },
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();

        assert_eq!(result.len(), 5);
        assert!(result.contains(&SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: vec!["Pod".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "metadata": {
                        "name": "pod-1",
                        "namespace": "default",
                        "labels": {
                            "app": "nginx",
                            "env": "prod"
                        }
                    },
                    "status": {
                        "containerStatuses": [
                            {
                                "containerID": "c1",
                                "name": "nginx"
                            },
                            {
                                "containerID": "c2",
                                "name": "redis"
                            }
                        ]
                    }
                }))
            }
        }));

        assert!(result.contains(&SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(
                        "test",
                        "$unwind-$.status.containerStatuses[*]-p1-0"
                    ),
                    labels: vec!["Container".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "containerID": "c1",
                    "name": "nginx",
                }))
            }
        }));

        assert!(result.contains(&SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new(
                        "test",
                        "$unwind-$.status.containerStatuses[*]-p1-0$rel"
                    ),
                    labels: vec!["OWNS".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::new(),
                out_node: ElementReference::new(
                    "test",
                    "$unwind-$.status.containerStatuses[*]-p1-0"
                ),
                in_node: ElementReference::new("test", "p1"),
            }
        }));

        assert!(result.contains(&SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(
                        "test",
                        "$unwind-$.status.containerStatuses[*]-p1-1"
                    ),
                    labels: vec!["Container".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "containerID": "c2",
                    "name": "redis",
                }))
            }
        }));

        assert!(result.contains(&SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new(
                        "test",
                        "$unwind-$.status.containerStatuses[*]-p1-1$rel"
                    ),
                    labels: vec!["OWNS".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::new(),
                out_node: ElementReference::new(
                    "test",
                    "$unwind-$.status.containerStatuses[*]-p1-1"
                ),
                in_node: ElementReference::new("test", "p1"),
            }
        }));
    }

    #[tokio::test]
    pub async fn conditional_unwind_array() {
        let factory = UnwindFactory::new();
        let config = json!({
            "Pod": [{
                "selector": "$.status.containerStatuses[*]",
                "label": "Container",
                "key": "$.containerID",
                "condition": "$[?(@.action == 'update')]"
            }]
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "unwind".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let result = subject
            .process(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("test", "p1"),
                            labels: vec!["Pod".into()].into(),
                            effective_from: 0,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "action": "update",
                            "metadata": {
                                "name": "pod-1",
                                "namespace": "default",
                                "labels": {
                                    "app": "nginx",
                                    "env": "prod"
                                }
                            },
                            "status": {
                                "containerStatuses": [
                                    {
                                        "containerID": "c1",
                                        "name": "nginx"
                                    },
                                    {
                                        "containerID": "c2",
                                        "name": "redis"
                                    }
                                ]
                            }
                        })),
                    },
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();

        assert_eq!(result.len(), 3);

        let result = subject
            .process(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("test", "p1"),
                            labels: vec!["Pod".into()].into(),
                            effective_from: 0,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "action": "delete",
                            "metadata": {
                                "name": "pod-1",
                                "namespace": "default",
                                "labels": {
                                    "app": "nginx",
                                    "env": "prod"
                                }
                            },
                            "status": {
                                "containerStatuses": [
                                    {
                                        "containerID": "c1",
                                        "name": "nginx"
                                    },
                                    {
                                        "containerID": "c2",
                                        "name": "redis"
                                    }
                                ]
                            }
                        })),
                    },
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();

        assert_eq!(result.len(), 1);

        element_index
            .set_element(
                &Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p1"),
                        labels: vec!["Pod".into()].into(),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({
                        "metadata": {
                            "name": "pod-1",
                            "namespace": "default",
                            "labels": {
                                "app": "nginx",
                                "env": "prod"
                            }
                        },
                        "status": {
                            "containerStatuses": [
                                {
                                    "containerID": "c1",
                                    "name": "nginx"
                                },
                                {
                                    "containerID": "c2",
                                    "name": "redis"
                                }
                            ]
                        }
                    })),
                },
                &vec![],
            )
            .await
            .unwrap();

        let result = subject
            .process(
                SourceChange::Delete {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "p1"),
                        labels: vec!["Pod".into()].into(),
                        effective_from: 0,
                    },
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();

        assert_eq!(result.len(), 1);
    }
}

mod factory {
    use drasi_core::{interface::SourceMiddlewareFactory, models::SourceMiddlewareConfig};
    use serde_json::json;

    use crate::unwind::UnwindFactory;

    #[tokio::test]
    pub async fn construct_map_middleware() {
        let subject = UnwindFactory::new();
        let config = json!({
            "Pod": [{
                "selector": "$.status.containerStatuses[*]",
                "label": "Container",
                "key": "$.containerID",
                "relation": "OWNS"
            }]
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "unwind".into(),
            config: config.as_object().unwrap().clone(),
        };

        assert!(subject.create(&mw_config).is_ok());
    }

    #[tokio::test]
    pub async fn invalid_selector() {
        let subject = UnwindFactory::new();
        let config = json!({
            "Pod": [{
                "selector": "z$.status.containerStatuses[*]",
                "label": "Container",
                "key": "$.containerID",
                "relation": "OWNS"
            }]
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "unwind".into(),
            config: config.as_object().unwrap().clone(),
        };

        assert!(subject.create(&mw_config).is_err());
    }
}

mod canonical_mutation {
    use std::{collections::BTreeSet, sync::Arc};

    use drasi_core::{
        in_memory_index::in_memory_element_index::InMemoryElementIndex,
        interface::{ElementIndex, SourceMiddleware, SourceMiddlewareFactory},
        models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
            SourceMiddlewareConfig,
        },
    };
    use serde_json::{json, Value};

    use crate::unwind::UnwindFactory;

    const CHILD: &str = "$unwind-$.status.containerStatuses[*]-p1-c1";

    fn subject() -> Arc<dyn SourceMiddleware> {
        UnwindFactory::new()
            .create(&SourceMiddlewareConfig {
                name: "test".into(),
                kind: "unwind".into(),
                config: json!({"Pod": [{
                    "selector": "$.status.containerStatuses[*]",
                    "label": "Container",
                    "key": "$.containerID"
                }]})
                .as_object()
                .unwrap()
                .clone(),
            })
            .unwrap()
    }

    fn pod(time: u64, properties: Value) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "p1"),
                labels: vec!["Pod".into()].into(),
                effective_from: time,
            },
            properties: ElementPropertyMap::from(properties),
        }
    }

    fn statuses(containers: Value) -> Value {
        json!({"status": {"containerStatuses": containers}})
    }

    async fn index_with(element: &Element) -> InMemoryElementIndex {
        let index = InMemoryElementIndex::new();
        index.set_element(element, &vec![]).await.unwrap();
        index
    }

    fn deleted(changes: &[SourceChange]) -> Vec<String> {
        changes
            .iter()
            .filter_map(|change| match change {
                SourceChange::Delete { metadata } => {
                    Some(metadata.reference.element_id.to_string())
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn removing_the_array_property_deletes_children_and_keeps_the_removal() {
        let old = pod(
            0,
            statuses(json!([
                {"containerID": "c1", "name": "nginx"},
                {"containerID": "c2", "name": "redis"}
            ])),
        );
        let index = index_with(&old).await;
        let change = SourceChange::update_with_removals(
            pod(1, json!({"label": "x"})),
            BTreeSet::from(["status".to_string()]),
        )
        .unwrap();
        let result = subject().process(change.clone(), &index).await.unwrap();
        let mut gone = deleted(&result);
        gone.sort();
        assert_eq!(
            gone,
            [
                CHILD.to_string(),
                "$unwind-$.status.containerStatuses[*]-p1-c2".to_string()
            ]
        );
        assert_eq!(result.last(), Some(&change));
    }

    #[tokio::test]
    async fn omitting_the_array_property_removes_its_children_like_removing_it() {
        let old = pod(0, statuses(json!([{"containerID": "c1", "name": "nginx"}])));
        let index = index_with(&old).await;
        let change = SourceChange::update(pod(1, json!({"label": "x"})));
        let result = subject().process(change.clone(), &index).await.unwrap();
        // An update is evaluated as the patch it carries; the parent stays a patch.
        assert_eq!(deleted(&result), [CHILD.to_string()]);
        assert_eq!(result.last(), Some(&change));
    }

    #[tokio::test]
    async fn changed_item_update_removes_the_fields_it_no_longer_has() {
        let old = pod(
            0,
            statuses(json!([{"containerID": "c1", "name": "nginx", "extra": 1}])),
        );
        let index = index_with(&old).await;
        let change = SourceChange::update(pod(
            1,
            statuses(json!([{"containerID": "c1", "name": "nginx"}])),
        ));
        let result = subject().process(change, &index).await.unwrap();
        let child = result
            .iter()
            .find_map(|change| match change {
                SourceChange::Update {
                    element,
                    removed_properties,
                } if element.get_reference().element_id.as_ref() == CHILD => {
                    Some(removed_properties.clone())
                }
                _ => None,
            })
            .expect("child update");
        assert_eq!(child, BTreeSet::from(["extra".to_string()]));
    }

    #[tokio::test]
    async fn replace_stays_a_replacement_and_deletes_dropped_children() {
        let old = pod(
            0,
            statuses(json!([
                {"containerID": "c1", "name": "nginx"},
                {"containerID": "c2", "name": "redis"}
            ])),
        );
        let index = index_with(&old).await;
        let replacement = pod(1, statuses(json!([{"containerID": "c2", "name": "redis"}])));
        let result = subject()
            .process(SourceChange::replace(replacement.clone()), &index)
            .await
            .unwrap();
        assert_eq!(deleted(&result), [CHILD.to_string()]);
        assert!(result.iter().any(|change| matches!(
            change,
            SourceChange::Replace { element }
                if element.get_reference().element_id.as_ref()
                    == "$unwind-$.status.containerStatuses[*]-p1-c2"
        )));
        assert_eq!(result.last(), Some(&SourceChange::replace(replacement)));
        assert!(!result
            .iter()
            .any(|change| matches!(change, SourceChange::Insert { .. })));
    }
}
