mod process {
    use drasi_core::{
        interface::SourceMiddlewareFactory,
        models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
            SourceMiddlewareConfig,
        },
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

        let subject = factory.create(&mw_config).unwrap();

        let result = subject
            .process(SourceChange::Insert {
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

        let subject = factory.create(&mw_config).unwrap();

        let result = subject
            .process(SourceChange::Insert {
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
}

mod factory {
    use drasi_core::{interface::SourceMiddlewareFactory, models::SourceMiddlewareConfig};
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

        assert!(subject.create(&mw_config).is_ok());
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

        assert!(subject.create(&mw_config).is_err());
    }
}
