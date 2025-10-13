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
