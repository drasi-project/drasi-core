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
    use drasi_lib::Source;

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
