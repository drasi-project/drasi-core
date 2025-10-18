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

//! Tests for channel events

#[cfg(test)]
mod tests {
    use crate::channels::*;
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use std::sync::Arc;

    #[test]
    fn test_component_event_source_starting() {
        let event = ComponentEvent {
            component_id: "source-1".to_string(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting source".to_string()),
        };

        assert_eq!(event.component_id, "source-1");
        assert!(matches!(event.component_type, ComponentType::Source));
        assert!(matches!(event.status, ComponentStatus::Starting));
        assert_eq!(event.message, Some("Starting source".to_string()));
    }

    #[test]
    fn test_component_event_query_running() {
        let event = ComponentEvent {
            component_id: "query-1".to_string(),
            component_type: ComponentType::Query,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: None,
        };

        assert_eq!(event.component_id, "query-1");
        assert!(matches!(event.component_type, ComponentType::Query));
        assert!(matches!(event.status, ComponentStatus::Running));
        assert!(event.message.is_none());
    }

    #[test]
    fn test_component_event_reaction_stopping() {
        let event = ComponentEvent {
            component_id: "reaction-1".to_string(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Stopping,
            timestamp: chrono::Utc::now(),
            message: Some("Graceful shutdown".to_string()),
        };

        assert_eq!(event.component_id, "reaction-1");
        assert!(matches!(event.component_type, ComponentType::Reaction));
        assert!(matches!(event.status, ComponentStatus::Stopping));
        assert!(event.message.is_some());
    }

    #[test]
    fn test_component_event_stopped() {
        let event = ComponentEvent {
            component_id: "component-1".to_string(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: None,
        };

        assert!(matches!(event.status, ComponentStatus::Stopped));
    }

    #[test]
    fn test_component_event_error() {
        let event = ComponentEvent {
            component_id: "component-1".to_string(),
            component_type: ComponentType::Query,
            status: ComponentStatus::Error,
            timestamp: chrono::Utc::now(),
            message: Some("Connection failed".to_string()),
        };

        assert!(matches!(event.status, ComponentStatus::Error));
        assert!(event.message.unwrap().contains("Connection failed"));
    }

    #[test]
    fn test_source_event_change() {
        let metadata = ElementMetadata {
            reference: ElementReference::new("source1", "node1"),
            labels: Arc::from(vec![Arc::from("Person")]),
            effective_from: 12345,
        };

        let element = Element::Node {
            metadata,
            properties: ElementPropertyMap::new(),
        };

        let change = SourceChange::Insert { element };
        let event = SourceEvent::Change(change);

        assert!(matches!(event, SourceEvent::Change(_)));
    }

    #[test]
    fn test_source_event_control() {
        let control = SourceControl::Subscription {
            query_id: "query-1".to_string(),
            query_node_id: "node-1".to_string(),
            node_labels: vec!["Person".to_string()],
            rel_labels: vec!["KNOWS".to_string()],
            operation: ControlOperation::Insert,
        };
        let event = SourceEvent::Control(control);

        assert!(matches!(
            event,
            SourceEvent::Control(SourceControl::Subscription { .. })
        ));
    }

    #[test]
    fn test_source_event_wrapper() {
        let metadata = ElementMetadata {
            reference: ElementReference::new("source1", "node1"),
            labels: Arc::from(vec![Arc::from("Person")]),
            effective_from: 12345,
        };

        let element = Element::Node {
            metadata,
            properties: ElementPropertyMap::new(),
        };

        let change = SourceChange::Insert { element };
        let wrapper = SourceEventWrapper {
            source_id: "test-source".to_string(),
            event: SourceEvent::Change(change),
            timestamp: chrono::Utc::now(),
            profiling: None,
        };

        assert_eq!(wrapper.source_id, "test-source");
        assert!(matches!(wrapper.event, SourceEvent::Change(_)));
    }

    #[test]
    fn test_query_result_creation() {
        use std::collections::HashMap;

        let result = QueryResult {
            query_id: "query-1".to_string(),
            results: vec![],
            metadata: HashMap::new(),
            profiling: None,
            timestamp: chrono::Utc::now(),
        };

        assert_eq!(result.query_id, "query-1");
        assert!(result.results.is_empty());
        assert!(result.metadata.is_empty());
    }

    #[test]
    fn test_query_result_with_metadata() {
        use std::collections::HashMap;
        let mut metadata = HashMap::new();
        metadata.insert("key".to_string(), serde_json::json!("value"));

        let result = QueryResult {
            query_id: "query-1".to_string(),
            results: vec![],
            metadata: metadata.clone(),
            profiling: None,
            timestamp: chrono::Utc::now(),
        };

        assert!(!result.metadata.is_empty());
        assert_eq!(
            result.metadata.get("key"),
            Some(&serde_json::json!("value"))
        );
    }

    #[test]
    fn test_bootstrap_request() {
        let request = BootstrapRequest {
            query_id: "query-1".to_string(),
            node_labels: vec!["Person".to_string()],
            relation_labels: vec!["KNOWS".to_string()],
            request_id: "req-123".to_string(),
        };

        assert_eq!(request.query_id, "query-1");
        assert_eq!(request.request_id, "req-123");
        assert_eq!(request.node_labels.len(), 1);
        assert_eq!(request.relation_labels.len(), 1);
    }

    #[test]
    fn test_bootstrap_response_success() {
        let response = BootstrapResponse {
            request_id: "req-123".to_string(),
            status: BootstrapStatus::Completed { total_count: 100 },
            message: Some("Bootstrap complete".to_string()),
        };

        assert!(matches!(response.status, BootstrapStatus::Completed { .. }));
        assert_eq!(response.message, Some("Bootstrap complete".to_string()));
    }

    #[test]
    fn test_bootstrap_response_failure() {
        let response = BootstrapResponse {
            request_id: "req-123".to_string(),
            status: BootstrapStatus::Failed {
                error: "timeout".to_string(),
            },
            message: Some("Bootstrap failed: timeout".to_string()),
        };

        assert!(matches!(response.status, BootstrapStatus::Failed { .. }));
        assert!(response.message.unwrap().contains("timeout"));
    }

    #[test]
    fn test_event_channels_creation() {
        let (channels, receivers) = EventChannels::new();

        // Verify channel senders exist
        assert!(channels.source_event_tx.max_capacity() > 0);
        assert!(channels.query_result_tx.max_capacity() > 0);
        assert!(channels.component_event_tx.max_capacity() > 0);
        assert!(channels.bootstrap_request_tx.max_capacity() > 0);

        // Verify receivers exist
        drop(receivers.source_event_rx);
        drop(receivers.query_result_rx);
        drop(receivers.component_event_rx);
        drop(receivers.bootstrap_request_rx);
    }

    #[test]
    fn test_component_status_variants() {
        let statuses = vec![
            ComponentStatus::Starting,
            ComponentStatus::Running,
            ComponentStatus::Stopping,
            ComponentStatus::Stopped,
            ComponentStatus::Error,
        ];

        assert_eq!(statuses.len(), 5);
    }

    #[test]
    fn test_component_type_variants() {
        let types = vec![ComponentType::Source, ComponentType::Query, ComponentType::Reaction];

        assert_eq!(types.len(), 3);
    }

    #[test]
    fn test_source_control_subscription() {
        let control = SourceControl::Subscription {
            query_id: "query-1".to_string(),
            query_node_id: "node-1".to_string(),
            node_labels: vec!["Person".to_string()],
            rel_labels: vec!["KNOWS".to_string()],
            operation: ControlOperation::Insert,
        };
        assert!(matches!(control, SourceControl::Subscription { .. }));
    }

    #[tokio::test]
    async fn test_send_receive_component_event() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let event = ComponentEvent {
            component_id: "test".to_string(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: None,
        };

        tx.send(event).await.unwrap();
        let received = rx.recv().await.unwrap();

        assert_eq!(received.component_id, "test");
        assert!(matches!(received.status, ComponentStatus::Running));
    }

    #[tokio::test]
    async fn test_send_receive_query_result() {
        use std::collections::HashMap;
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let result = QueryResult {
            query_id: "query-1".to_string(),
            results: vec![],
            metadata: HashMap::new(),
            profiling: None,
            timestamp: chrono::Utc::now(),
        };

        tx.send(result).await.unwrap();
        let received = rx.recv().await.unwrap();

        assert_eq!(received.query_id, "query-1");
        assert!(received.results.is_empty());
    }

    #[tokio::test]
    async fn test_send_receive_bootstrap_request() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        let request = BootstrapRequest {
            query_id: "query-1".to_string(),
            node_labels: vec!["Person".to_string()],
            relation_labels: vec!["KNOWS".to_string()],
            request_id: "req-123".to_string(),
        };

        tx.send(request).await.unwrap();
        let received = rx.recv().await.unwrap();

        assert_eq!(received.query_id, "query-1");
        assert_eq!(received.request_id, "req-123");
    }

    #[tokio::test]
    async fn test_channel_closes_when_sender_dropped() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ComponentEvent>(10);

        drop(tx);

        let result = rx.recv().await;
        assert!(
            result.is_none(),
            "Channel should be closed after sender dropped"
        );
    }

    #[tokio::test]
    async fn test_multiple_events_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        for i in 1..=5 {
            let event = ComponentEvent {
                component_id: format!("component-{}", i),
                component_type: ComponentType::Source,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: None,
            };
            tx.send(event).await.unwrap();
        }

        for i in 1..=5 {
            let received = rx.recv().await.unwrap();
            assert_eq!(received.component_id, format!("component-{}", i));
        }
    }
}
