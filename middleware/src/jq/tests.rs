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
        interface::SourceMiddlewareFactory,
        models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
            SourceMiddlewareConfig,
        },
    };
    use serde_json::json;

    use crate::jq::JQFactory;

    #[tokio::test]
    pub async fn map_insert_to_update() {
        let factory = JQFactory::new();
        let config = json!({
            "Telemetry": {
                "insert": [{
                    "op": "Update",
                    "label": "\"Vehicle\"",
                    "id": ".id",
                    "query": "{
                        \"id\": .vehicleId,
                        \"currentSpeed\": .signals[] | select(.name == \"Vehicle.Speed\").value | tonumber
                    }"
                }]
            }
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "jq".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let result = subject
            .process(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("test", "t1"),
                            labels: vec!["Telemetry".into()].into(),
                            effective_from: 0,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "signals": [
                                {
                                    "name": "Vehicle.CurrentLocation.Heading",
                                    "value": "96"
                                },
                                {
                                    "name": "Vehicle.Speed",
                                    "value": "119"
                                },
                                {
                                    "name": "Vehicle.TraveledDistance",
                                    "value": "4563"
                                }
                            ],
                            "additionalProperties": {
                                "Source": "provider.telemetry"
                            },
                            "vehicleId": "v1"
                        })),
                    },
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0],
            SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "v1"),
                        labels: vec!["Vehicle".into()].into(),
                        effective_from: 0
                    },
                    properties: ElementPropertyMap::from(json!({
                        "id": "v1",
                        "currentSpeed": 119
                    }))
                }
            }
        );
    }

    mod extended_config {
        use std::{collections::HashSet, sync::Arc};

        use drasi_core::{
            in_memory_index::in_memory_element_index::InMemoryElementIndex,
            interface::{ElementIndex, SourceMiddleware, SourceMiddlewareFactory},
            models::{
                Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
                SourceChange, SourceMiddlewareConfig,
            },
        };
        use serde_json::{json, Value};

        use crate::jq::JQFactory;

        fn middleware(config: Value) -> Arc<dyn SourceMiddleware> {
            let config = SourceMiddlewareConfig {
                name: "jq-extended".into(),
                kind: "jq".into(),
                config: config.as_object().unwrap().clone(),
            };
            JQFactory::new().create(&config).unwrap()
        }

        fn node_change(operation: &str, properties: Value) -> SourceChange {
            let element = Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("github", "comment:1"),
                    labels: Arc::new([Arc::from("Comment")]),
                    effective_from: 42,
                },
                properties: ElementPropertyMap::from(properties),
            };
            match operation {
                "insert" => SourceChange::Insert { element },
                "update" => SourceChange::Update { element },
                _ => panic!("unsupported operation"),
            }
        }

        fn reconciliation_config() -> Value {
            json!({
                "preserve_input": true,
                "include_source_metadata": true,
                "reconcile": true,
                "mappings": {
                    "Comment": {
                        "insert": [
                            {
                                "op": "Insert",
                                "elementType": "Node",
                                "label": "\"DerivedNode\"",
                                "id": ".id",
                                "query": "if .valid then {\"id\":\"derived:1\",\"value\":.value} else empty end"
                            },
                            {
                                "op": "Insert",
                                "elementType": {
                                    "relation": {
                                        "inNodeId": ".inId",
                                        "outNodeId": ".outId"
                                    }
                                },
                                "label": "\"DERIVED_REL\"",
                                "id": ".id",
                                "query": "if .valid then {\"id\":\"derived-rel:1\",\"inId\":\"issue:1\",\"outId\":\"derived:1\"} else empty end"
                            }
                        ],
                        "update": [
                            {
                                "op": "Update",
                                "elementType": "Node",
                                "label": "\"DerivedNode\"",
                                "id": ".id",
                                "query": "if .valid then {\"id\":\"derived:1\",\"value\":.value} else empty end"
                            },
                            {
                                "op": "Update",
                                "elementType": {
                                    "relation": {
                                        "inNodeId": ".inId",
                                        "outNodeId": ".outId"
                                    }
                                },
                                "label": "\"DERIVED_REL\"",
                                "id": ".id",
                                "query": "if .valid then {\"id\":\"derived-rel:1\",\"inId\":\"issue:1\",\"outId\":\"derived:1\"} else empty end"
                            }
                        ]
                    }
                }
            })
        }

        async fn index_outputs(index: &InMemoryElementIndex, changes: &[SourceChange]) {
            for change in changes {
                if let SourceChange::Insert { element } | SourceChange::Update { element } = change
                {
                    index.set_element(element, &Vec::new()).await.unwrap();
                }
            }
        }

        #[tokio::test]
        async fn preserves_input_and_exposes_structured_metadata() {
            let subject = middleware(json!({
                "preserve_input": true,
                "include_source_metadata": true,
                "mappings": {
                    "Comment": {
                        "insert": [{
                            "op": "Insert",
                            "label": "\"Observed\"",
                            "id": "\"observed:1\"",
                            "query": "{\"source\": .\"$source\"}"
                        }],
                        "update": [{
                            "op": "Update",
                            "label": "\"Observed\"",
                            "id": "\"observed:1\"",
                            "query": "{\"source\": .\"$source\"}"
                        }]
                    }
                }
            }));
            let input = node_change("insert", json!({"body": "hello"}));
            let output = subject
                .process(input.clone(), &InMemoryElementIndex::new())
                .await
                .unwrap();

            assert_eq!(output.len(), 2);
            assert_eq!(output[1], input);
            let SourceChange::Insert { element } = &output[0] else {
                panic!("expected derived insert");
            };
            let ElementValue::Object(source) = element.get_properties().get("source").unwrap()
            else {
                panic!("expected source metadata object");
            };
            assert_eq!(
                source.get("operation"),
                Some(&ElementValue::String(Arc::from("insert")))
            );
            assert_eq!(
                source.get("sourceId"),
                Some(&ElementValue::String(Arc::from("github")))
            );
            assert_eq!(
                source.get("elementId"),
                Some(&ElementValue::String(Arc::from("comment:1")))
            );
            assert_eq!(
                source.get("effectiveTime"),
                Some(&ElementValue::Integer(42))
            );

            let update = node_change("update", json!({"body": "changed"}));
            let output = subject
                .process(update, &InMemoryElementIndex::new())
                .await
                .unwrap();
            let SourceChange::Update { element } = &output[0] else {
                panic!("expected derived update");
            };
            let ElementValue::Object(source) = element.get_properties().get("source").unwrap()
            else {
                panic!("expected source metadata object");
            };
            assert_eq!(
                source.get("operation"),
                Some(&ElementValue::String(Arc::from("update")))
            );
        }

        #[tokio::test]
        async fn exposes_delete_and_relation_endpoint_metadata() {
            let subject = middleware(json!({
                "preserve_input": true,
                "include_source_metadata": true,
                "mappings": {
                    "LINK": {
                        "insert": [{
                            "op": "Insert",
                            "elementType": "Node",
                            "label": "\"Observed\"",
                            "id": "\"observed:relation\"",
                            "query": "{\"source\": .\"$source\"}"
                        }],
                        "delete": [{
                            "op": "Insert",
                            "elementType": "Node",
                            "label": "\"Observed\"",
                            "id": "\"observed:delete\"",
                            "query": "{\"source\": .\"$source\"}"
                        }]
                    }
                }
            }));
            let relation = Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("github", "link:1"),
                    labels: Arc::new([Arc::from("LINK")]),
                    effective_from: 50,
                },
                in_node: ElementReference::new("github", "issue:1"),
                out_node: ElementReference::new("github", "item:1"),
                properties: ElementPropertyMap::new(),
            };
            let index = InMemoryElementIndex::new();
            index.set_element(&relation, &Vec::new()).await.unwrap();

            let insert = subject
                .process(
                    SourceChange::Insert {
                        element: relation.clone(),
                    },
                    &index,
                )
                .await
                .unwrap();
            let SourceChange::Insert { element } = &insert[0] else {
                panic!("expected derived insert");
            };
            let ElementValue::Object(source) = element.get_properties().get("source").unwrap()
            else {
                panic!("expected source metadata object");
            };
            assert!(matches!(
                source.get("inNode"),
                Some(ElementValue::Object(_))
            ));
            assert!(matches!(
                source.get("outNode"),
                Some(ElementValue::Object(_))
            ));

            let delete = subject
                .process(
                    SourceChange::Delete {
                        metadata: relation.get_metadata().clone(),
                    },
                    &index,
                )
                .await
                .unwrap();
            let SourceChange::Insert { element } = &delete[0] else {
                panic!("expected observed delete metadata");
            };
            let ElementValue::Object(source) = element.get_properties().get("source").unwrap()
            else {
                panic!("expected source metadata object");
            };
            assert_eq!(
                source.get("operation"),
                Some(&ElementValue::String(Arc::from("delete")))
            );
            assert!(matches!(
                source.get("inNode"),
                Some(ElementValue::Object(_))
            ));
        }

        #[tokio::test]
        async fn creates_fan_out_and_reconciles_invalidating_update() {
            let subject = middleware(reconciliation_config());
            let index = InMemoryElementIndex::new();
            let insert_input = node_change("insert", json!({"valid": true, "value": "old"}));
            let insert = subject.process(insert_input, &index).await.unwrap();

            assert_eq!(
                insert
                    .iter()
                    .filter(|change| matches!(change, SourceChange::Insert { .. }))
                    .count(),
                3
            );
            assert!(insert.iter().any(|change| matches!(
                change,
                SourceChange::Insert {
                    element: Element::Relation { .. }
                }
            )));
            index_outputs(&index, &insert).await;

            let update = subject
                .process(
                    node_change("update", json!({"valid": false, "value": "ignored"})),
                    &index,
                )
                .await
                .unwrap();
            let deleted_ids: HashSet<String> = update
                .iter()
                .filter_map(|change| match change {
                    SourceChange::Delete { metadata } => {
                        Some(metadata.reference.element_id.to_string())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                deleted_ids,
                HashSet::from(["derived:1".to_string(), "derived-rel:1".to_string()])
            );
            assert!(matches!(update.last(), Some(SourceChange::Update { .. })));
        }

        #[tokio::test]
        async fn delete_cleans_up_previous_fan_out() {
            let subject = middleware(reconciliation_config());
            let index = InMemoryElementIndex::new();
            let insert = subject
                .process(
                    node_change("insert", json!({"valid": true, "value": "old"})),
                    &index,
                )
                .await
                .unwrap();
            index_outputs(&index, &insert).await;

            let delete = SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("github", "comment:1"),
                    labels: Arc::new([Arc::from("Comment")]),
                    effective_from: 100,
                },
            };
            let output = subject.process(delete.clone(), &index).await.unwrap();
            let deleted_ids: HashSet<String> = output
                .iter()
                .filter_map(|change| match change {
                    SourceChange::Delete { metadata } => {
                        Some(metadata.reference.element_id.to_string())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                deleted_ids,
                HashSet::from([
                    "comment:1".to_string(),
                    "derived:1".to_string(),
                    "derived-rel:1".to_string()
                ])
            );
            assert_eq!(output.last(), Some(&delete));
        }

        #[test]
        fn reconcile_requires_preserved_input() {
            let config = SourceMiddlewareConfig {
                name: "jq-extended".into(),
                kind: "jq".into(),
                config: json!({
                    "reconcile": true,
                    "mappings": {}
                })
                .as_object()
                .unwrap()
                .clone(),
            };
            assert!(JQFactory::new().create(&config).is_err());
        }

        #[test]
        fn legacy_label_may_match_an_extended_option_name() {
            let config = SourceMiddlewareConfig {
                name: "jq-legacy".into(),
                kind: "jq".into(),
                config: json!({
                    "reconcile": {
                        "insert": [{
                            "op": "Insert",
                            "query": "."
                        }]
                    }
                })
                .as_object()
                .expect("legacy jq configuration must be an object")
                .clone(),
            };
            assert!(JQFactory::new().create(&config).is_ok());
        }
    }

    #[tokio::test]
    pub async fn map_insert_to_multiple() {
        let factory = JQFactory::new();
        let config = json!({
            "Telemetry": {
                "insert": [
                {
                    "op": "Update",
                    "label": "\"Vehicle\"",
                    "id": ".id",
                    "query": "{
                        \"id\": .vehicleId,
                        \"currentSpeed\": .signals[] | select(.name == \"Vehicle.Speed\").value
                    }"
                },
                {
                    "op": "Update",
                    "label": "\"Fleet\"",
                    "id": ".id",
                    "query": "{
                        \"id\": .fleetId,
                        \"lastReportedVehicleId\": .vehicleId
                    }"
                }]
            }
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "jq".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let result = subject
            .process(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("test", "t1"),
                            labels: vec!["Telemetry".into()].into(),
                            effective_from: 0,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "signals": [
                                {
                                    "name": "Vehicle.CurrentLocation.Heading",
                                    "value": "96"
                                },
                                {
                                    "name": "Vehicle.Speed",
                                    "value": "119"
                                },
                                {
                                    "name": "Vehicle.TraveledDistance",
                                    "value": "4563"
                                }
                            ],
                            "additionalProperties": {
                                "Source": "provider.telemetry"
                            },
                            "vehicleId": "v1",
                            "fleetId": "f1"
                        })),
                    },
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "v1"),
                    labels: vec!["Vehicle".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "v1",
                    "currentSpeed": "119"
                }))
            }
        }));
        assert!(result.contains(&SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "f1"),
                    labels: vec!["Fleet".into()].into(),
                    effective_from: 0
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "f1",
                    "lastReportedVehicleId": "v1"
                }))
            }
        }));
    }

    #[tokio::test]
    pub async fn conditional_map() {
        let factory = JQFactory::new();
        let config = json!({
            "Telemetry": {
                "insert": [{
                    "op": "Update",
                    "label": "\"Vehicle\"",
                    "id": ".id",
                    "query": "if .action == \"update\" then
                        {
                            \"id\": .vehicleId,
                            \"currentSpeed\": .signals[] | select(.name == \"Vehicle.Speed\").value
                        }
                    else 
                        empty
                    end"
                },
                {
                    "op": "Delete",
                    "label": "\"Vehicle\"",
                    "id": ".id",
                    "query": "if .action == \"delete\" then
                        { \"id\": .vehicleId}
                    else 
                        empty
                    end"
                }]
            }
        });

        let element_index = Arc::new(InMemoryElementIndex::new());
        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "jq".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.create(&mw_config).unwrap();

        let result = subject
            .process(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("test", "t1"),
                            labels: vec!["Telemetry".into()].into(),
                            effective_from: 0,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "signals": [
                                {
                                    "name": "Vehicle.CurrentLocation.Heading",
                                    "value": "96"
                                },
                                {
                                    "name": "Vehicle.Speed",
                                    "value": "119"
                                },
                                {
                                    "name": "Vehicle.TraveledDistance",
                                    "value": "4563"
                                }
                            ],
                            "additionalProperties": {
                                "Source": "provider.telemetry"
                            },
                            "action": "update",
                            "vehicleId": "v1"
                        })),
                    },
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0],
            SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("test", "v1"),
                        labels: vec!["Vehicle".into()].into(),
                        effective_from: 0
                    },
                    properties: ElementPropertyMap::from(json!({
                        "id": "v1",
                        "currentSpeed": "119"
                    }))
                }
            }
        );

        let result = subject
            .process(
                SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("test", "t1"),
                            labels: vec!["Telemetry".into()].into(),
                            effective_from: 0,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "signals": [
                                {
                                    "name": "Vehicle.CurrentLocation.Heading",
                                    "value": "96"
                                },
                                {
                                    "name": "Vehicle.Speed",
                                    "value": "119"
                                },
                                {
                                    "name": "Vehicle.TraveledDistance",
                                    "value": "4563"
                                }
                            ],
                            "additionalProperties": {
                                "Source": "provider.telemetry"
                            },
                            "action": "delete",
                            "vehicleId": "v1"
                        })),
                    },
                },
                element_index.as_ref(),
            )
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0],
            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "v1"),
                    labels: vec!["Vehicle".into()].into(),
                    effective_from: 0
                }
            }
        );
    }
}
