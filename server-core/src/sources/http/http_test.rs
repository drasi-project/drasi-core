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
mod tests {
    use super::super::*;
    use crate::config::SourceConfig;
    use crate::sources::http::direct_format::*;
    use crate::sources::Source;
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_http_source_node_insert_conversion() {
        // Test node insert conversion with direct format
        let event = DirectSourceChange::Insert {
            element: DirectElement::Node {
                id: "user_123".to_string(),
                labels: vec!["User".to_string(), "Customer".to_string()],
                properties: serde_json::from_str(r#"{"name": "John", "age": 30}"#).unwrap(),
            },
            timestamp: Some(1234567890000),
        };

        let result = convert_direct_to_source_change(&event, "test-source").unwrap();

        match result {
            drasi_core::models::SourceChange::Insert { element } => match element {
                drasi_core::models::Element::Node {
                    metadata,
                    properties,
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "user_123");
                    assert_eq!(metadata.labels.len(), 2);
                    match properties.get("name").unwrap() {
                        drasi_core::models::ElementValue::String(s) => {
                            assert_eq!(s.as_ref(), "John")
                        }
                        _ => panic!("Expected string value"),
                    }
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert operation"),
        }
    }

    #[tokio::test]
    async fn test_http_source_node_update_conversion() {
        // Test node update conversion
        let event = DirectSourceChange::Update {
            element: DirectElement::Node {
                id: "user_123".to_string(),
                labels: vec!["User".to_string(), "Premium".to_string()],
                properties: serde_json::from_str(r#"{"name": "John Doe", "age": 31}"#).unwrap(),
            },
            timestamp: Some(1234567890000),
        };

        let result = convert_direct_to_source_change(&event, "test-source").unwrap();

        match result {
            drasi_core::models::SourceChange::Update { element } => match element {
                drasi_core::models::Element::Node {
                    metadata,
                    properties,
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "user_123");
                    assert_eq!(metadata.labels.len(), 2);
                    match properties.get("name").unwrap() {
                        drasi_core::models::ElementValue::String(s) => {
                            assert_eq!(s.as_ref(), "John Doe")
                        }
                        _ => panic!("Expected string value"),
                    }
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Update operation"),
        }
    }

    #[tokio::test]
    async fn test_http_source_delete_conversion() {
        // Test delete conversion
        let event = DirectSourceChange::Delete {
            id: "user_123".to_string(),
            labels: Some(vec!["User".to_string()]),
            timestamp: Some(1234567890000),
        };

        let result = convert_direct_to_source_change(&event, "test-source").unwrap();

        match result {
            drasi_core::models::SourceChange::Delete { metadata } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "user_123");
                assert_eq!(metadata.labels.len(), 1);
                assert_eq!(metadata.effective_from, 1234567890000);
            }
            _ => panic!("Expected Delete operation"),
        }
    }

    #[tokio::test]
    async fn test_http_source_json_deserialization() {
        // Test that we can deserialize from JSON with direct format
        let json = r#"{
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "user_123",
                "labels": ["User"],
                "properties": {
                    "name": "John Doe",
                    "active": true
                }
            },
            "timestamp": 1234567890000
        }"#;

        let event: DirectSourceChange = serde_json::from_str(json).unwrap();

        match event {
            DirectSourceChange::Insert { element, timestamp } => {
                match element {
                    DirectElement::Node { id, labels, .. } => {
                        assert_eq!(id, "user_123");
                        assert_eq!(labels.len(), 1);
                    }
                    _ => panic!("Expected Node"),
                }
                assert_eq!(timestamp, Some(1234567890000));
            }
            _ => panic!("Expected Insert"),
        }
    }

    #[tokio::test]
    async fn test_http_source_start_stop() {
        let config = SourceConfig {
            id: "test-http-source".to_string(),
            source_type: "http".to_string(),
            auto_start: false,
            properties: {
                let mut props = HashMap::new();
                props.insert("port".to_string(), serde_json::json!(9999));
                props.insert("host".to_string(), serde_json::json!("127.0.0.1"));
                props
            },
            bootstrap_provider: None,
            broadcast_channel_capacity: None,
        };

        let (event_tx, _event_rx) = mpsc::channel(100);

        let source = HttpSource::new(config, event_tx).unwrap();

        // Test initial status
        assert_eq!(
            source.status().await,
            crate::channels::ComponentStatus::Stopped
        );

        // Test start
        source.start().await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        assert_eq!(
            source.status().await,
            crate::channels::ComponentStatus::Running
        );

        // Test stop
        source.stop().await.unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        assert_eq!(
            source.status().await,
            crate::channels::ComponentStatus::Stopped
        );
    }
}
