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

#[cfg(test)]
mod mock_source_tests {
    use super::super::MockSource;
    use drasi_lib::channels::*;
    use drasi_lib::config::SourceConfig;
    use drasi_lib::sources::Source;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    /// Helper to convert a serde_json::Value object to HashMap<String, serde_json::Value>
    fn to_hashmap(value: serde_json::Value) -> HashMap<String, serde_json::Value> {
        match value {
            serde_json::Value::Object(map) => map.into_iter().collect(),
            _ => HashMap::new(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_mock_source_counter() {
        let (event_tx, _event_rx) = mpsc::channel(100);

        let config = SourceConfig {
            id: "test-counter".to_string(),
            auto_start: false,
            config: drasi_lib::config::SourceSpecificConfig::Mock(to_hashmap(json!({
                "data_type": "counter",
                "interval_ms": 100
            }))),
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: Some(DispatchMode::Broadcast),
        };

        let source = MockSource::new(config, event_tx).unwrap();
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
    async fn test_mock_source_sensor() {
        let (event_tx, _event_rx) = mpsc::channel(100);

        let config = SourceConfig {
            id: "test-sensor".to_string(),
            auto_start: false,
            config: drasi_lib::config::SourceSpecificConfig::Mock(to_hashmap(json!({
                "data_type": "sensor",
                "interval_ms": 100
            }))),
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: Some(DispatchMode::Broadcast),
        };

        let source = MockSource::new(config, event_tx).unwrap();
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

    #[tokio::test]
    async fn test_mock_source_status() {
        let (event_tx, _event_rx) = mpsc::channel(100);

        let config = SourceConfig {
            id: "test-status".to_string(),
            auto_start: false,
            config: drasi_lib::config::SourceSpecificConfig::Mock(to_hashmap(json!({
                "data_type": "counter",
                "interval_ms": 1000
            }))),
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
        };

        let source = MockSource::new(config, event_tx).unwrap();

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
