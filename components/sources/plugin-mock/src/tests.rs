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
    use crate::{MockSource, MockSourceConfig};
    use drasi_lib::channels::ComponentStatus;
    use drasi_lib::plugin_core::Source;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_mock_source_counter() {
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
    async fn test_mock_source_sensor() {
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

    #[tokio::test]
    async fn test_mock_source_status() {
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
