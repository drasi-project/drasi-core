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

// ============================================================================
// Construction Tests
// ============================================================================

mod construction {
    use crate::{MockSource, MockSourceBuilder, MockSourceConfig};
    use drasi_lib::Source;

    #[test]
    fn test_builder_with_valid_config() {
        let source = MockSourceBuilder::new("test-source")
            .with_data_type("counter")
            .with_interval_ms(1000)
            .build();

        assert!(source.is_ok());
    }

    #[test]
    fn test_new_with_custom_config() {
        let config = MockSourceConfig {
            data_type: "sensor".to_string(),
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
    use crate::MockSourceBuilder;
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
            .with_data_type("sensor")
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("data_type"),
            Some(&serde_json::Value::String("sensor".to_string()))
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
}

// ============================================================================
// Lifecycle Tests
// ============================================================================

mod lifecycle {
    use crate::{MockSource, MockSourceConfig};
    use drasi_lib::channels::ComponentStatus;
    use drasi_lib::Source;

    #[tokio::test]
    async fn test_initial_status_is_stopped() {
        let config = MockSourceConfig {
            data_type: "counter".to_string(),
            interval_ms: 1000,
        };

        let source = MockSource::new("test", config).unwrap();
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_status_transitions() {
        let config = MockSourceConfig {
            data_type: "counter".to_string(),
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
    use crate::MockSourceBuilder;
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
    }

    #[test]
    fn test_builder_with_all_options() {
        let source = MockSourceBuilder::new("test")
            .with_data_type("sensor")
            .with_interval_ms(1000)
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("data_type"),
            Some(&serde_json::Value::String("sensor".to_string()))
        );
        assert_eq!(
            props.get("interval_ms"),
            Some(&serde_json::Value::Number(1000.into()))
        );
    }

    #[test]
    fn test_builder_chaining() {
        let source = MockSourceBuilder::new("test")
            .with_data_type("counter")
            .with_data_type("sensor") // Override
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("data_type"),
            Some(&serde_json::Value::String("sensor".to_string()))
        );
    }

    #[test]
    fn test_builder_id() {
        let source = MockSourceBuilder::new("my-mock-source")
            .with_data_type("counter")
            .build()
            .unwrap();

        assert_eq!(source.id(), "my-mock-source");
    }
}

// ============================================================================
// Event Generation Tests
// ============================================================================

mod event_generation {
    use crate::{MockSource, MockSourceConfig};
    use drasi_lib::Source;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_counter_data_generation() {
        let config = MockSourceConfig {
            data_type: "counter".to_string(),
            interval_ms: 100,
        };

        let source = MockSource::new("test-counter", config).unwrap();
        let mut rx = source.test_subscribe();

        // Start the source
        let result = source.start().await;
        assert!(result.is_ok());

        // Collect changes
        let mut changes = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
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
    async fn test_sensor_data_generation() {
        let config = MockSourceConfig {
            data_type: "sensor".to_string(),
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
    async fn test_generic_data_generation() {
        let config = MockSourceConfig {
            data_type: "generic".to_string(),
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
}

// ============================================================================
// Config Tests
// ============================================================================

mod config {
    use crate::MockSourceConfig;

    #[test]
    fn test_config_serialization() {
        let config = MockSourceConfig {
            data_type: "sensor".to_string(),
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

        assert_eq!(config.data_type, "generic"); // default
        assert_eq!(config.interval_ms, 5000); // default
    }

    #[test]
    fn test_config_deserialization_partial() {
        let json = r#"{"data_type": "counter"}"#;
        let config: MockSourceConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.data_type, "counter");
        assert_eq!(config.interval_ms, 5000); // default
    }
}
