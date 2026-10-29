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

    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        SourceMiddlewareConfig,
    };
    use serde_json::json;

    use crate::map::MapFactory;

    #[tokio::test]
    pub async fn map_insert_to_update() {
        let factory = MapFactory::new();
        let config = json!({
            "Telemetry": {
                "insert": [{
                    "selector": "$[?(@.additionalProperties.Source == 'provider.telemetry')]",
                    "op": "Update",
                    "label": "Vehicle",
                    "id": "$.vehicleId",
                    "properties": {
                        "id": "$.vehicleId",
                        "currentSpeed": "$.signals[?(@.name == 'Vehicle.Speed')].value"
                    }
                }]
            }
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "map".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.build(&mw_config).unwrap();

        let result = subject
            .apply(SourceChange::Insert {
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
            })
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0],
            SourceChange::Update {
                removed_properties: Default::default(),
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
    }

    #[tokio::test]
    pub async fn map_insert_to_multiple() {
        let factory = MapFactory::new();
        let config = json!({
            "Telemetry": {
                "insert": [
                {
                    "selector": "$[?(@.additionalProperties.Source == 'provider.telemetry')]",
                    "op": "Update",
                    "label": "Vehicle",
                    "id": "$.vehicleId",
                    "properties": {
                        "id": "$.vehicleId",
                        "currentSpeed": "$.signals[?(@.name == 'Vehicle.Speed')].value"
                    }
                },
                {
                    "selector": "$[?(@.additionalProperties.Source == 'provider.telemetry')]",
                    "op": "Update",
                    "label": "Fleet",
                    "id": "$.fleetId",
                    "properties": {
                        "id": "$.fleetId",
                        "lastReportedVehicleId": "$.vehicleId"
                    }
                }]
            }
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "map".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.build(&mw_config).unwrap();

        let result = subject
            .apply(SourceChange::Insert {
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
            })
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&SourceChange::Update {
            removed_properties: Default::default(),
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
            removed_properties: Default::default(),
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
        let factory = MapFactory::new();
        let config = json!({
            "Telemetry": {
                "insert": [{
                    "selector": "$[?(@.additionalProperties.Source == 'provider.telemetry')]",
                    "op": "Update",
                    "label": "Vehicle",
                    "id": "$.vehicleId",
                    "condition": "$[?(@.action == 'update')]",
                    "properties": {
                        "id": "$.vehicleId",
                        "currentSpeed": "$.signals[?(@.name == 'Vehicle.Speed')].value"
                    }
                },
                {
                    "selector": "$[?(@.additionalProperties.Source == 'provider.telemetry')]",
                    "op": "Delete",
                    "label": "Vehicle",
                    "id": "$.vehicleId",
                    "condition": "$[?(@.action == 'delete')]"
                }]
            }
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "map".into(),
            config: config.as_object().unwrap().clone(),
        };

        let subject = factory.build(&mw_config).unwrap();

        let result = subject
            .apply(SourceChange::Insert {
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
            })
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0],
            SourceChange::Update {
                removed_properties: Default::default(),
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
            .apply(SourceChange::Insert {
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
            })
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

mod factory {
    use drasi_core_types::models::SourceMiddlewareConfig;
    use serde_json::json;

    use crate::map::MapFactory;

    #[tokio::test]
    pub async fn construct_map_middleware() {
        let subject = MapFactory::new();
        let config = json!({
            "Telemetry": {
                "insert": [{
                    "selector": "$[?(@.additionalProperties.Source == 'provider.telemetry')]",
                    "op": "Update",
                    "label": "Vehicle",
                    "id": "$.vehicleId",
                    "properties": {
                        "id": "$.vehicleId",
                        "currentSpeed": "$.signals[?(@.name == 'Vehicle.Speed')].value"
                    }
                }]
            }
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "map".into(),
            config: config.as_object().unwrap().clone(),
        };

        assert!(subject.build(&mw_config).is_ok());
    }

    #[tokio::test]
    pub async fn invalid_selector() {
        let subject = MapFactory::new();
        let config = json!({
            "Telemetry": {
                "insert": [{
                    "selector": "z$[?(@.additionalProperties.Source == 'provider.telemetry')]",
                    "op": "Update",
                    "label": "Vehicle",
                    "id": "$.vehicleId",
                    "properties": {
                        "id": "$.vehicleId",
                        "currentSpeed": "$.signals[?(@.name == 'Vehicle.Speed')].value"
                    }
                }]
            }
        });

        let mw_config = SourceMiddlewareConfig {
            name: "test".into(),
            kind: "map".into(),
            config: config.as_object().unwrap().clone(),
        };

        assert!(subject.build(&mw_config).is_err());
    }
}

mod canonical_mutation {
    use std::collections::BTreeSet;

    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        SourceMiddlewareConfig,
    };
    use serde_json::json;

    use crate::map::{Map, MapFactory};

    fn subject(op: Option<&str>) -> Map {
        let mut mapping = json!({
            "label": "Vehicle",
            "id": "$.vehicleId",
            "properties": {"id": "$.vehicleId", "speed": "$.speed"}
        });
        if let Some(op) = op {
            mapping["op"] = json!(op);
        }
        MapFactory::new()
            .build(&SourceMiddlewareConfig {
                name: "test".into(),
                kind: "map".into(),
                config: json!({"Telemetry": {"insert": [mapping.clone()], "update": [mapping]}})
                    .as_object()
                    .unwrap()
                    .clone(),
            })
            .unwrap()
    }

    fn telemetry(properties: serde_json::Value) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "t1"),
                labels: vec!["Telemetry".into()].into(),
                effective_from: 1,
            },
            properties: ElementPropertyMap::from(properties),
        }
    }

    #[tokio::test]
    pub async fn removal_patches_skip_update_rules_but_keep_meaningful_event_mappings() {
        let change = SourceChange::update_with_removals(
            telemetry(json!({"vehicleId": "v1"})),
            BTreeSet::from(["speed".to_string()]),
        )
        .unwrap();
        // A stateless mapping cannot tell which mapped output a removal affects.
        assert!(subject(None)
            .apply(change.clone())
            .await
            .unwrap()
            .is_empty());

        // Mapping to a full image or a deletion is well defined.
        let result = subject(Some("Insert")).apply(change.clone()).await.unwrap();
        assert!(matches!(result.as_slice(), [SourceChange::Insert { .. }]));
        let result = subject(Some("Replace"))
            .apply(change.clone())
            .await
            .unwrap();
        assert!(matches!(result.as_slice(), [SourceChange::Replace { .. }]));
        let result = subject(Some("Delete")).apply(change).await.unwrap();
        assert!(matches!(result.as_slice(), [SourceChange::Delete { .. }]));
    }

    #[tokio::test]
    pub async fn malformed_updates_are_errors_not_ignored_events() {
        let change = SourceChange::Update {
            element: telemetry(json!({"vehicleId":"v1","speed":3})),
            removed_properties: BTreeSet::from(["speed".to_string()]),
        };
        assert!(subject(None).apply(change).await.is_err());
    }

    #[tokio::test]
    pub async fn updates_without_removals_and_replacements_keep_their_kind() {
        let result = subject(None)
            .apply(SourceChange::update(telemetry(
                json!({"vehicleId": "v1", "speed": null}),
            )))
            .await
            .unwrap();
        let [SourceChange::Update {
            element,
            removed_properties,
        }] = result.as_slice()
        else {
            panic!("an update maps to one update");
        };
        assert!(removed_properties.is_empty());
        assert_eq!(
            element.get_properties().get("speed"),
            Some(&drasi_core_types::models::ElementValue::Null)
        );

        let result = subject(None)
            .apply(SourceChange::replace(telemetry(
                json!({"vehicleId": "v1", "speed": 3}),
            )))
            .await
            .unwrap();
        assert!(matches!(result.as_slice(), [SourceChange::Replace { .. }]));
    }
}
