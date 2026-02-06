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

//! Unit tests for the mock source plugin.
//!
//! Tests are organized into the following modules:
//! - `construction`: Tests for creating mock sources
//! - `properties`: Tests for source properties and type name
//! - `builder`: Tests for the `MockSourceBuilder`
//! - `lifecycle`: Tests for start/stop behavior
//! - `event_generation`: Tests for data generation modes
//! - `validation`: Tests for configuration validation

// ============================================================================
// Construction Tests
// ============================================================================

mod construction {
    use crate::{DataType, MockSource, MockSourceBuilder, MockSourceConfig};
    use drasi_lib::Source;

    #[test]
    fn test_builder_with_valid_config() {
        let source = MockSourceBuilder::new("test-source")
            .with_data_type(DataType::Counter)
            .with_interval_ms(1000)
            .build();

        assert!(source.is_ok());
    }

    #[test]
    fn test_new_with_custom_config() {
        let config = MockSourceConfig {
            data_type: DataType::sensor_reading(10),
            interval_ms: 500,
        };

        let source = MockSource::new("sensor-source", config).unwrap();
        assert_eq!(source.id(), "sensor-source");
    }

    #[test]
    fn test_with_dispatch_creates_source() {
        let config = MockSourceConfig::default();
        let source = MockSource::with_dispatch(
            "dispatch-source",
            config,
            Some(drasi_lib::channels::DispatchMode::Channel),
            Some(1000),
        );
        assert!(source.is_ok());
        assert_eq!(source.unwrap().id(), "dispatch-source");
    }
}

// ============================================================================
// Properties Tests
// ============================================================================

mod properties {
    use crate::{DataType, MockSourceBuilder};
    use drasi_lib::Source;

    #[test]
    fn test_id_returns_correct_value() {
        let source = MockSourceBuilder::new("my-mock-source").build().unwrap();
        assert_eq!(source.id(), "my-mock-source");
    }

    #[test]
    fn test_type_name_returns_mock() {
        let source = MockSourceBuilder::new("test").build().unwrap();
        assert_eq!(source.type_name(), "mock");
    }

    #[test]
    fn test_properties_contains_data_type() {
        let source = MockSourceBuilder::new("test")
            .with_data_type(DataType::sensor_reading(5))
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("data_type"),
            Some(&serde_json::Value::String("sensor_reading".to_string()))
        );
    }

    #[test]
    fn test_properties_contains_interval_ms() {
        let source = MockSourceBuilder::new("test")
            .with_interval_ms(2000)
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("interval_ms"),
            Some(&serde_json::Value::Number(2000.into()))
        );
    }

    #[test]
    fn test_properties_contains_sensor_count_for_sensor_reading() {
        let source = MockSourceBuilder::new("test")
            .with_data_type(DataType::sensor_reading(10))
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("sensor_count"),
            Some(&serde_json::Value::Number(10.into()))
        );
    }

    #[test]
    fn test_properties_no_sensor_count_for_counter() {
        let source = MockSourceBuilder::new("test")
            .with_data_type(DataType::Counter)
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(props.get("sensor_count"), None);
    }

    #[test]
    fn test_properties_no_sensor_count_for_generic() {
        let source = MockSourceBuilder::new("test")
            .with_data_type(DataType::Generic)
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(props.get("sensor_count"), None);
    }
}

// ============================================================================
// Lifecycle Tests
// ============================================================================

mod lifecycle {
    use crate::{DataType, MockSource, MockSourceConfig};
    use drasi_lib::channels::ComponentStatus;
    use drasi_lib::Source;

    #[tokio::test]
    async fn test_initial_status_is_stopped() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 1000,
        };

        let source = MockSource::new("test", config).unwrap();
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_status_transitions() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 1000,
        };

        let source = MockSource::new("test-status", config).unwrap();

        // Initial status
        assert_eq!(source.status().await, ComponentStatus::Stopped);

        // Start the source
        source.start().await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        assert_eq!(source.status().await, ComponentStatus::Running);

        // Stop the source
        source.stop().await.unwrap();
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }
}

// ============================================================================
// Builder Tests
// ============================================================================

mod builder {
    use crate::{DataType, MockSourceBuilder};
    use drasi_lib::Source;

    #[test]
    fn test_builder_defaults() {
        let source = MockSourceBuilder::new("test").build().unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("data_type"),
            Some(&serde_json::Value::String("generic".to_string()))
        );
        assert_eq!(
            props.get("interval_ms"),
            Some(&serde_json::Value::Number(5000.into()))
        );
        // sensor_count should not be present for generic
        assert_eq!(props.get("sensor_count"), None);
    }

    #[test]
    fn test_builder_with_all_options() {
        let source = MockSourceBuilder::new("test")
            .with_data_type(DataType::sensor_reading(10))
            .with_interval_ms(1000)
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("data_type"),
            Some(&serde_json::Value::String("sensor_reading".to_string()))
        );
        assert_eq!(
            props.get("interval_ms"),
            Some(&serde_json::Value::Number(1000.into()))
        );
        assert_eq!(
            props.get("sensor_count"),
            Some(&serde_json::Value::Number(10.into()))
        );
    }

    #[test]
    fn test_builder_chaining() {
        let source = MockSourceBuilder::new("test")
            .with_data_type(DataType::Counter)
            .with_data_type(DataType::sensor_reading(5)) // Override
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("data_type"),
            Some(&serde_json::Value::String("sensor_reading".to_string()))
        );
    }

    #[test]
    fn test_builder_id() {
        let source = MockSourceBuilder::new("my-mock-source")
            .with_data_type(DataType::Counter)
            .build()
            .unwrap();

        assert_eq!(source.id(), "my-mock-source");
    }
}

// ============================================================================
// Event Generation Tests
// ============================================================================

mod event_generation {
    use crate::{DataType, MockSource, MockSourceConfig};
    use drasi_core::models::{Element, SourceChange};
    use drasi_lib::channels::SourceEvent;
    use drasi_lib::Source;
    use std::collections::HashSet;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_counter_data_generation() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 100,
        };

        let source = MockSource::new("test-counter", config).unwrap();
        let mut rx = source.test_subscribe();

        // Start the source
        let result = source.start().await;
        assert!(result.is_ok());

        // Collect changes
        let mut changes = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Ok(event) = rx.recv().await {
                changes.push(event);
                if changes.len() >= 3 {
                    break;
                }
            }
        })
        .await
        .expect("Timeout waiting for changes");

        // Verify we got counter values
        assert_eq!(changes.len(), 3);

        // Stop the source
        source.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_counter_generates_correct_schema() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 50,
        };

        let source = MockSource::new("test-counter-schema", config).unwrap();
        let mut rx = source.test_subscribe();

        source.start().await.unwrap();

        // Get first event
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("Timeout")
            .expect("No event received");

        source.stop().await.unwrap();

        // Verify the event structure
        match &event.event {
            SourceEvent::Change(SourceChange::Insert { element }) => match element {
                Element::Node {
                    metadata,
                    properties,
                } => {
                    // Verify label
                    assert_eq!(metadata.labels.len(), 1);
                    assert_eq!(metadata.labels[0].as_ref(), "Counter");

                    // Verify element ID format
                    assert!(
                        metadata.reference.element_id.starts_with("counter_"),
                        "Element ID should start with 'counter_', got: {}",
                        metadata.reference.element_id
                    );

                    // Verify properties exist
                    assert!(
                        properties.get("value").is_some(),
                        "Counter should have 'value' property"
                    );
                    assert!(
                        properties.get("timestamp").is_some(),
                        "Counter should have 'timestamp' property"
                    );
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert event with Change"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_counter_values_are_sequential() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 50,
        };

        let source = MockSource::new("test-counter-seq", config).unwrap();
        let mut rx = source.test_subscribe();

        source.start().await.unwrap();

        // Collect 3 events
        let mut values = Vec::new();
        for _ in 0..3 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("Timeout")
                .expect("No event");

            if let SourceEvent::Change(SourceChange::Insert {
                element: Element::Node { properties, .. },
            }) = &event.event
            {
                if let Some(drasi_core::models::ElementValue::Integer(v)) = properties.get("value")
                {
                    values.push(*v);
                }
            }
        }

        source.stop().await.unwrap();

        // Verify sequential values (1, 2, 3)
        assert_eq!(values.len(), 3);
        assert_eq!(values[0], 1);
        assert_eq!(values[1], 2);
        assert_eq!(values[2], 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_counter_always_generates_insert_events() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 50,
        };

        let source = MockSource::new("test-counter-insert", config).unwrap();
        let mut rx = source.test_subscribe();

        source.start().await.unwrap();

        // Collect 5 events and verify all are Insert
        for i in 0..5 {
            let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
                .await
                .expect("Timeout")
                .expect("No event");

            match &event.event {
                SourceEvent::Change(SourceChange::Insert { .. }) => {
                    // Expected
                }
                other => panic!("Event {i} should be Insert, got: {other:?}"),
            }
        }

        source.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sensor_data_generation() {
        let config = MockSourceConfig {
            data_type: DataType::sensor_reading(5),
            interval_ms: 100,
        };

        let source = MockSource::new("test-sensor", config).unwrap();
        let mut rx = source.test_subscribe();

        // Start the source
        source.start().await.unwrap();

        // Collect a few sensor readings
        let mut readings = Vec::new();
        tokio::time::timeout(std::time::Duration::from_millis(350), async {
            while let Ok(event) = rx.recv().await {
                readings.push(event);
                if readings.len() >= 3 {
                    break;
                }
            }
        })
        .await
        .expect("Timeout waiting for readings");

        assert!(readings.len() >= 3);

        // Stop the source
        source.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sensor_generates_correct_schema() {
        let config = MockSourceConfig {
            data_type: DataType::sensor_reading(5),
            interval_ms: 50,
        };

        let source = MockSource::new("test-sensor-schema", config).unwrap();
        let mut rx = source.test_subscribe();

        source.start().await.unwrap();

        // Get first event
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("Timeout")
            .expect("No event received");

        source.stop().await.unwrap();

        // Extract the element from the event
        let (metadata, properties) = match &event.event {
            SourceEvent::Change(SourceChange::Insert { element })
            | SourceEvent::Change(SourceChange::Update { element }) => match element {
                Element::Node {
                    metadata,
                    properties,
                } => (metadata, properties),
                _ => panic!("Expected Node element"),
            },
            other => panic!("Expected Insert or Update event, got: {other:?}"),
        };

        // Verify label
        assert_eq!(metadata.labels.len(), 1);
        assert_eq!(metadata.labels[0].as_ref(), "SensorReading");

        // Verify element ID format
        assert!(
            metadata.reference.element_id.starts_with("sensor_"),
            "Element ID should start with 'sensor_', got: {}",
            metadata.reference.element_id
        );

        // Verify required properties exist
        assert!(
            properties.get("sensor_id").is_some(),
            "SensorReading should have 'sensor_id' property"
        );
        assert!(
            properties.get("temperature").is_some(),
            "SensorReading should have 'temperature' property"
        );
        assert!(
            properties.get("humidity").is_some(),
            "SensorReading should have 'humidity' property"
        );
        assert!(
            properties.get("timestamp").is_some(),
            "SensorReading should have 'timestamp' property"
        );

        // Verify temperature is in expected range [20.0, 30.0)
        if let Some(drasi_core::models::ElementValue::Float(temp)) = properties.get("temperature") {
            let temp_val = temp.into_inner();
            assert!(
                (20.0..30.0).contains(&temp_val),
                "Temperature should be in [20.0, 30.0), got: {temp_val}"
            );
        }

        // Verify humidity is in expected range [40.0, 60.0)
        if let Some(drasi_core::models::ElementValue::Float(humidity)) = properties.get("humidity")
        {
            let humidity_val = humidity.into_inner();
            assert!(
                (40.0..60.0).contains(&humidity_val),
                "Humidity should be in [40.0, 60.0), got: {humidity_val}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sensor_insert_then_update_behavior() {
        // Use only 2 sensors to increase chance of seeing same sensor twice
        let config = MockSourceConfig {
            data_type: DataType::sensor_reading(2),
            interval_ms: 30,
        };

        let source = MockSource::new("test-sensor-update", config).unwrap();
        let mut rx = source.test_subscribe();

        source.start().await.unwrap();

        // Collect events until we see both Insert and Update, or timeout
        let mut seen_insert = false;
        let mut seen_update = false;
        let mut insert_sensor_ids: HashSet<String> = HashSet::new();

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !seen_update {
                if let Ok(event) = rx.recv().await {
                    match &event.event {
                        SourceEvent::Change(SourceChange::Insert { element }) => {
                            seen_insert = true;
                            if let Element::Node { metadata, .. } = element {
                                insert_sensor_ids.insert(metadata.reference.element_id.to_string());
                            }
                        }
                        SourceEvent::Change(SourceChange::Update { element }) => {
                            seen_update = true;
                            // Verify this sensor was previously inserted
                            if let Element::Node { metadata, .. } = element {
                                assert!(
                                    insert_sensor_ids
                                        .contains(metadata.reference.element_id.as_ref()),
                                    "Update should be for a previously inserted sensor"
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        })
        .await;

        source.stop().await.unwrap();

        assert!(result.is_ok(), "Should see Update event within timeout");
        assert!(seen_insert, "Should have seen at least one Insert");
        assert!(seen_update, "Should have seen at least one Update");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_sensor_respects_sensor_count() {
        let sensor_count = 3u32;
        let config = MockSourceConfig {
            data_type: DataType::sensor_reading(sensor_count),
            interval_ms: 30,
        };

        let source = MockSource::new("test-sensor-count", config).unwrap();
        let mut rx = source.test_subscribe();

        source.start().await.unwrap();

        // Collect many events
        let mut sensor_ids: HashSet<u32> = HashSet::new();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            for _ in 0..20 {
                if let Ok(event) = rx.recv().await {
                    if let SourceEvent::Change(change) = &event.event {
                        let element = match change {
                            SourceChange::Insert { element } => element,
                            SourceChange::Update { element } => element,
                            _ => continue,
                        };
                        if let Element::Node { metadata, .. } = element {
                            // Extract sensor ID number from "sensor_N"
                            if let Some(id_str) =
                                metadata.reference.element_id.strip_prefix("sensor_")
                            {
                                if let Ok(id) = id_str.parse::<u32>() {
                                    sensor_ids.insert(id);
                                }
                            }
                        }
                    }
                }
            }
        })
        .await
        .ok();

        source.stop().await.unwrap();

        // Verify all sensor IDs are within [0, sensor_count)
        for id in &sensor_ids {
            assert!(
                *id < sensor_count,
                "Sensor ID {id} should be < {sensor_count}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_generic_data_generation() {
        let config = MockSourceConfig {
            data_type: DataType::Generic,
            interval_ms: 100,
        };

        let source = MockSource::new("test-generic", config).unwrap();
        let mut rx = source.test_subscribe();

        // Start the source
        source.start().await.unwrap();

        // Collect changes
        let mut changes = Vec::new();
        tokio::time::timeout(std::time::Duration::from_millis(350), async {
            while let Ok(event) = rx.recv().await {
                changes.push(event);
                if changes.len() >= 2 {
                    break;
                }
            }
        })
        .await
        .expect("Timeout waiting for changes");

        assert!(changes.len() >= 2);

        // Stop the source
        source.stop().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_generic_generates_correct_schema() {
        let config = MockSourceConfig {
            data_type: DataType::Generic,
            interval_ms: 50,
        };

        let source = MockSource::new("test-generic-schema", config).unwrap();
        let mut rx = source.test_subscribe();

        source.start().await.unwrap();

        // Get first event
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("Timeout")
            .expect("No event received");

        source.stop().await.unwrap();

        // Verify the event structure
        match &event.event {
            SourceEvent::Change(SourceChange::Insert { element }) => match element {
                Element::Node {
                    metadata,
                    properties,
                } => {
                    // Verify label
                    assert_eq!(metadata.labels.len(), 1);
                    assert_eq!(metadata.labels[0].as_ref(), "Generic");

                    // Verify element ID format
                    assert!(
                        metadata.reference.element_id.starts_with("generic_"),
                        "Element ID should start with 'generic_', got: {}",
                        metadata.reference.element_id
                    );

                    // Verify properties exist
                    assert!(
                        properties.get("value").is_some(),
                        "Generic should have 'value' property"
                    );
                    assert!(
                        properties.get("message").is_some(),
                        "Generic should have 'message' property"
                    );
                    assert!(
                        properties.get("timestamp").is_some(),
                        "Generic should have 'timestamp' property"
                    );

                    // Verify message content
                    if let Some(drasi_core::models::ElementValue::String(msg)) =
                        properties.get("message")
                    {
                        assert_eq!(msg.as_ref(), "Generic mock data");
                    }
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert event with Change"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_generic_always_generates_insert_events() {
        let config = MockSourceConfig {
            data_type: DataType::Generic,
            interval_ms: 50,
        };

        let source = MockSource::new("test-generic-insert", config).unwrap();
        let mut rx = source.test_subscribe();

        source.start().await.unwrap();

        // Collect 5 events and verify all are Insert
        for i in 0..5 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("Timeout")
                .expect("No event");

            match &event.event {
                SourceEvent::Change(SourceChange::Insert { .. }) => {
                    // Expected
                }
                other => panic!("Event {i} should be Insert, got: {other:?}"),
            }
        }

        source.stop().await.unwrap();
    }
}

// ============================================================================
// Config Tests
// ============================================================================

mod config {
    use crate::{DataType, MockSourceConfig};

    #[test]
    fn test_config_serialization() {
        let config = MockSourceConfig {
            data_type: DataType::sensor_reading(10),
            interval_ms: 1000,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: MockSourceConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.data_type, deserialized.data_type);
        assert_eq!(config.interval_ms, deserialized.interval_ms);
    }

    #[test]
    fn test_config_deserialization_with_defaults() {
        let json = r#"{}"#;
        let config: MockSourceConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.data_type, DataType::Generic); // default
        assert_eq!(config.interval_ms, 5000); // default
    }

    #[test]
    fn test_config_deserialization_partial() {
        let json = r#"{"data_type": {"type": "counter"}}"#;
        let config: MockSourceConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.data_type, DataType::Counter);
        assert_eq!(config.interval_ms, 5000); // default
    }

    #[test]
    fn test_config_sensor_reading_with_custom_count() {
        let json = r#"{"data_type": {"type": "sensor_reading", "sensor_count": 20}}"#;
        let config: MockSourceConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.data_type, DataType::sensor_reading(20));
        assert_eq!(config.data_type.sensor_count(), Some(20));
    }

    #[test]
    fn test_config_sensor_reading_with_default_count() {
        let json = r#"{"data_type": {"type": "sensor_reading"}}"#;
        let config: MockSourceConfig = serde_json::from_str(json).unwrap();

        // sensor_count defaults to 5
        assert_eq!(config.data_type.sensor_count(), Some(5));
    }

    #[test]
    fn test_data_type_display() {
        assert_eq!(DataType::Counter.to_string(), "counter");
        assert_eq!(DataType::sensor_reading(5).to_string(), "sensor_reading");
        assert_eq!(DataType::Generic.to_string(), "generic");
    }

    #[test]
    fn test_data_type_sensor_count_helper() {
        assert_eq!(DataType::Counter.sensor_count(), None);
        assert_eq!(DataType::Generic.sensor_count(), None);
        assert_eq!(DataType::sensor_reading(10).sensor_count(), Some(10));
    }
}

// ============================================================================
// Validation Tests
// ============================================================================

mod validation {
    use crate::{DataType, MockSource, MockSourceBuilder, MockSourceConfig};

    #[test]
    fn test_validation_rejects_zero_interval() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 0,
        };

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_rejects_zero_sensor_count() {
        let config = MockSourceConfig {
            data_type: DataType::sensor_reading(0),
            interval_ms: 1000,
        };

        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validation_accepts_valid_config() {
        let config = MockSourceConfig {
            data_type: DataType::sensor_reading(5),
            interval_ms: 1000,
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_new_rejects_invalid_config() {
        let config = MockSourceConfig {
            data_type: DataType::sensor_reading(0),
            interval_ms: 1000,
        };

        assert!(MockSource::new("test", config).is_err());
    }

    #[test]
    fn test_builder_rejects_zero_interval() {
        let result = MockSourceBuilder::new("test")
            .with_data_type(DataType::Counter)
            .with_interval_ms(0)
            .build();

        assert!(result.is_err());
    }

    #[test]
    fn test_builder_rejects_zero_sensor_count() {
        let result = MockSourceBuilder::new("test")
            .with_data_type(DataType::sensor_reading(0))
            .with_interval_ms(1000)
            .build();

        assert!(result.is_err());
    }

    #[test]
    fn test_with_dispatch_rejects_invalid_config() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 0,
        };

        let result = MockSource::with_dispatch("test", config, None, None);
        assert!(result.is_err());
    }
}

// ============================================================================
// Inject Event Tests
// ============================================================================

mod inject_event {
    use crate::{DataType, MockSource, MockSourceConfig};
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    };
    use drasi_lib::channels::SourceEvent;
    use drasi_lib::Source;
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_inject_event_delivers_to_subscribers() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 10000, // Long interval so no auto-generated events interfere
        };

        let source = MockSource::new("test-inject", config).unwrap();
        let mut rx = source.test_subscribe();

        // Create a custom element to inject
        let element_id = "custom_injected_1";
        let reference = ElementReference::new("test-inject", element_id);

        let mut properties = ElementPropertyMap::new();
        properties.insert("custom_value", ElementValue::Integer(42));

        let metadata = ElementMetadata {
            reference,
            labels: Arc::from(vec![Arc::from("CustomLabel")]),
            effective_from: 1234567890,
        };

        let element = Element::Node {
            metadata,
            properties,
        };
        let change = SourceChange::Insert { element };

        // Inject the event
        source.inject_event(change).await.unwrap();

        // Receive and verify
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("Timeout")
            .expect("No event received");

        match &event.event {
            SourceEvent::Change(SourceChange::Insert { element }) => match element {
                Element::Node {
                    metadata,
                    properties,
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "custom_injected_1");
                    assert_eq!(metadata.labels[0].as_ref(), "CustomLabel");
                    assert_eq!(
                        properties.get("custom_value"),
                        Some(&ElementValue::Integer(42))
                    );
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert event"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_inject_update_event() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 10000,
        };

        let source = MockSource::new("test-inject-update", config).unwrap();
        let mut rx = source.test_subscribe();

        // Create an Update event
        let reference = ElementReference::new("test-inject-update", "updated_element");
        let mut properties = ElementPropertyMap::new();
        properties.insert(
            "updated_field",
            ElementValue::String(Arc::from("new_value")),
        );

        let metadata = ElementMetadata {
            reference,
            labels: Arc::from(vec![Arc::from("TestLabel")]),
            effective_from: 0,
        };

        let element = Element::Node {
            metadata,
            properties,
        };
        let change = SourceChange::Update { element };

        // Inject the Update event
        source.inject_event(change).await.unwrap();

        // Verify it's received as an Update
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("Timeout")
            .expect("No event");

        match &event.event {
            SourceEvent::Change(SourceChange::Update { element }) => {
                if let Element::Node { metadata, .. } = element {
                    assert_eq!(metadata.reference.element_id.as_ref(), "updated_element");
                }
            }
            other => panic!("Expected Update event, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_inject_delete_event() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 10000,
        };

        let source = MockSource::new("test-inject-delete", config).unwrap();
        let mut rx = source.test_subscribe();

        // Create a Delete event (Delete only takes metadata, not element)
        let reference = ElementReference::new("test-inject-delete", "deleted_element");
        let metadata = ElementMetadata {
            reference,
            labels: Arc::from(vec![Arc::from("TestLabel")]),
            effective_from: 0,
        };

        let change = SourceChange::Delete { metadata };

        // Inject the Delete event
        source.inject_event(change).await.unwrap();

        // Verify it's received as a Delete
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("Timeout")
            .expect("No event");

        match &event.event {
            SourceEvent::Change(SourceChange::Delete { metadata }) => {
                assert_eq!(metadata.reference.element_id.as_ref(), "deleted_element");
            }
            other => panic!("Expected Delete event, got: {other:?}"),
        }
    }
}

// ============================================================================
// Builder Advanced Options Tests
// ============================================================================

mod builder_advanced {
    use crate::{DataType, MockSource, MockSourceBuilder};
    use drasi_lib::channels::DispatchMode;
    use drasi_lib::Source;

    #[test]
    fn test_builder_with_dispatch_mode_channel() {
        let source = MockSourceBuilder::new("test-dispatch-channel")
            .with_data_type(DataType::Counter)
            .with_dispatch_mode(DispatchMode::Channel)
            .build()
            .unwrap();

        assert_eq!(source.id(), "test-dispatch-channel");
    }

    #[test]
    fn test_builder_with_dispatch_mode_broadcast() {
        let source = MockSourceBuilder::new("test-dispatch-broadcast")
            .with_data_type(DataType::Counter)
            .with_dispatch_mode(DispatchMode::Broadcast)
            .build()
            .unwrap();

        assert_eq!(source.id(), "test-dispatch-broadcast");
    }

    #[test]
    fn test_builder_with_dispatch_buffer_capacity() {
        let source = MockSourceBuilder::new("test-buffer")
            .with_data_type(DataType::Counter)
            .with_dispatch_buffer_capacity(5000)
            .build()
            .unwrap();

        assert_eq!(source.id(), "test-buffer");
    }

    #[test]
    fn test_builder_with_auto_start_true() {
        let source = MockSourceBuilder::new("test-auto-start-true")
            .with_data_type(DataType::Counter)
            .with_auto_start(true)
            .build()
            .unwrap();

        assert!(source.auto_start());
    }

    #[test]
    fn test_builder_with_auto_start_false() {
        let source = MockSourceBuilder::new("test-auto-start-false")
            .with_data_type(DataType::Counter)
            .with_auto_start(false)
            .build()
            .unwrap();

        assert!(!source.auto_start());
    }

    #[test]
    fn test_builder_auto_start_default_is_true() {
        let source = MockSourceBuilder::new("test-auto-start-default")
            .with_data_type(DataType::Counter)
            .build()
            .unwrap();

        assert!(source.auto_start());
    }

    #[test]
    fn test_builder_static_method() {
        // Test that MockSource::builder() works the same as MockSourceBuilder::new()
        let source = MockSource::builder("test-static-builder")
            .with_data_type(DataType::Generic)
            .with_interval_ms(2000)
            .build()
            .unwrap();

        assert_eq!(source.id(), "test-static-builder");
        let props = source.properties();
        assert_eq!(
            props.get("data_type"),
            Some(&serde_json::Value::String("generic".to_string()))
        );
        assert_eq!(
            props.get("interval_ms"),
            Some(&serde_json::Value::Number(2000.into()))
        );
    }

    #[test]
    fn test_builder_with_all_dispatch_options() {
        let source = MockSourceBuilder::new("test-all-dispatch")
            .with_data_type(DataType::sensor_reading(10))
            .with_interval_ms(500)
            .with_dispatch_mode(DispatchMode::Channel)
            .with_dispatch_buffer_capacity(2000)
            .with_auto_start(false)
            .build()
            .unwrap();

        assert_eq!(source.id(), "test-all-dispatch");
        assert!(!source.auto_start());
    }
}

// ============================================================================
// Lifecycle Advanced Tests
// ============================================================================

mod lifecycle_advanced {
    use crate::{DataType, MockSource, MockSourceConfig};
    use drasi_lib::channels::ComponentStatus;
    use drasi_lib::Source;

    #[tokio::test]
    async fn test_multiple_start_stop_cycles() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 100,
        };

        let source = MockSource::new("test-cycles", config).unwrap();

        // Cycle 1
        assert_eq!(source.status().await, ComponentStatus::Stopped);
        source.start().await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        assert_eq!(source.status().await, ComponentStatus::Running);
        source.stop().await.unwrap();
        assert_eq!(source.status().await, ComponentStatus::Stopped);

        // Cycle 2
        source.start().await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        assert_eq!(source.status().await, ComponentStatus::Running);
        source.stop().await.unwrap();
        assert_eq!(source.status().await, ComponentStatus::Stopped);

        // Cycle 3
        source.start().await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        assert_eq!(source.status().await, ComponentStatus::Running);
        source.stop().await.unwrap();
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_stop_when_already_stopped() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 1000,
        };

        let source = MockSource::new("test-stop-stopped", config).unwrap();

        // Source is already stopped, stop again should not error
        let result = source.stop().await;
        assert!(result.is_ok());
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_data_generation_stops_after_stop() {
        let config = MockSourceConfig {
            data_type: DataType::Counter,
            interval_ms: 50,
        };

        let source = MockSource::new("test-gen-stops", config).unwrap();
        let mut rx = source.test_subscribe();

        // Start and collect some events
        source.start().await.unwrap();

        let mut count = 0;
        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            while rx.recv().await.is_ok() {
                count += 1;
                if count >= 2 {
                    break;
                }
            }
        })
        .await
        .ok();

        assert!(count >= 2, "Should have received events while running");

        // Stop the source
        source.stop().await.unwrap();

        // Wait a bit and verify no more events arrive
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        // Try to receive - should timeout or get no new events
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await;

        // Either timeout or channel closed is acceptable
        assert!(
            result.is_err() || result.unwrap().is_err(),
            "Should not receive events after stop"
        );
    }
}

// ============================================================================
// Default Config Tests
// ============================================================================

mod default_config {
    use crate::{DataType, MockSourceConfig};

    #[test]
    fn test_mock_source_config_default() {
        let config = MockSourceConfig::default();

        assert_eq!(config.data_type, DataType::Generic);
        assert_eq!(config.interval_ms, 5000);
    }

    #[test]
    fn test_data_type_default() {
        let data_type = DataType::default();
        assert_eq!(data_type, DataType::Generic);
    }

    #[test]
    fn test_default_config_validates() {
        let config = MockSourceConfig::default();
        assert!(config.validate().is_ok());
    }
}
