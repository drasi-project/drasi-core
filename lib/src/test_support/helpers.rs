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

pub mod test_fixtures {
    use crate::config::{
        DrasiLibConfig, DrasiLibSettings, QueryConfig,
    };

    /// Creates a minimal valid server configuration for testing
    #[allow(dead_code)]
    pub fn create_test_server_config() -> DrasiLibConfig {
        DrasiLibConfig {
            server_core: DrasiLibSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                dispatch_buffer_capacity: None,
            },
            storage_backends: vec![],
            queries: vec![],
        }
    }

    /// Creates a test query configuration
    pub fn create_test_query_config(id: &str, sources: Vec<String>) -> QueryConfig {
        use crate::config::{QueryLanguage, SourceSubscriptionConfig};
        QueryConfig {
            id: id.to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            source_subscriptions: sources
                .into_iter()
                .map(|source_id| SourceSubscriptionConfig {
                    source_id,
                    pipeline: vec![],
                })
                .collect(),
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        }
    }

    /// Creates a test GQL query configuration
    pub fn create_test_gql_query_config(id: &str, sources: Vec<String>) -> QueryConfig {
        use crate::config::{QueryLanguage, SourceSubscriptionConfig};
        QueryConfig {
            id: id.to_string(),
            query: "MATCH (n:Person) RETURN n.name".to_string(),
            query_language: QueryLanguage::GQL,
            middleware: vec![],
            source_subscriptions: sources
                .into_iter()
                .map(|source_id| SourceSubscriptionConfig {
                    source_id,
                    pipeline: vec![],
                })
                .collect(),
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub mod mock_helpers {
    use crate::channels::*;
    use tokio::sync::mpsc;

    /// Creates a test channel pair for component events
    pub fn create_test_event_channel() -> (ComponentEventSender, ComponentEventReceiver) {
        mpsc::channel(100)
    }
}

/// Test mock implementations for unit testing without external plugin dependencies.
/// These are simple inline mocks, not the actual plugin implementations.
#[cfg(test)]
pub mod test_mocks {
    use crate::channels::dispatcher::{ChangeDispatcher, ChannelChangeDispatcher};
    use crate::channels::*;
    use crate::plugin_core::Source;
    use anyhow::Result;
    use async_trait::async_trait;
    use drasi_core::models::SourceChange;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// A simple test mock source for unit testing the SourceManager.
    /// This is NOT a real plugin - it's an inline test double.
    ///
    /// This mock source supports event injection for testing data flow through queries.
    pub struct TestMockSource {
        id: String,
        status: Arc<RwLock<ComponentStatus>>,
        #[allow(dead_code)]
        event_tx: ComponentEventSender,
        /// Dispatchers for sending events to subscribed queries
        dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
    }

    impl TestMockSource {
        pub fn new(id: String, event_tx: ComponentEventSender) -> Result<Self> {
            Ok(Self {
                id,
                status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
                event_tx,
                dispatchers: Arc::new(RwLock::new(Vec::new())),
            })
        }

        /// Inject an event into all subscribed queries.
        /// This is useful for testing query processing with mock data.
        pub async fn inject_event(&self, change: SourceChange) -> Result<()> {
            let dispatchers = self.dispatchers.read().await;
            let wrapper = SourceEventWrapper::new(
                self.id.clone(),
                SourceEvent::Change(change),
                chrono::Utc::now(),
            );
            let arc_wrapper = Arc::new(wrapper);
            for dispatcher in dispatchers.iter() {
                dispatcher.dispatch_change(arc_wrapper.clone()).await?;
            }
            Ok(())
        }
    }

    #[async_trait]
    impl Source for TestMockSource {
        fn id(&self) -> &str {
            &self.id
        }

        fn type_name(&self) -> &str {
            "mock"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        async fn start(&self) -> Result<()> {
            *self.status.write().await = ComponentStatus::Starting;

            // Send Starting event
            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Starting,
                timestamp: chrono::Utc::now(),
                message: Some("Starting source".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            *self.status.write().await = ComponentStatus::Running;

            // Send Running event
            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: Some("Source started".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            Ok(())
        }

        async fn stop(&self) -> Result<()> {
            *self.status.write().await = ComponentStatus::Stopping;

            // Send Stopping event
            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Stopping,
                timestamp: chrono::Utc::now(),
                message: Some("Stopping source".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            *self.status.write().await = ComponentStatus::Stopped;

            // Send Stopped event
            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Stopped,
                timestamp: chrono::Utc::now(),
                message: Some("Source stopped".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.status.read().await.clone()
        }

        async fn subscribe(
            &self,
            query_id: String,
            _enable_bootstrap: bool,
            _node_labels: Vec<String>,
            _relation_labels: Vec<String>,
        ) -> Result<SubscriptionResponse> {
            // Create a channel dispatcher for this subscription
            let dispatcher = ChannelChangeDispatcher::<SourceEventWrapper>::new(100);
            let receiver = dispatcher.create_receiver().await?;

            // Store the dispatcher so we can inject events later
            self.dispatchers.write().await.push(Box::new(dispatcher));

            Ok(SubscriptionResponse {
                query_id,
                source_id: self.id.clone(),
                receiver,
                bootstrap_receiver: None,
            })
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        async fn inject_event_tx(&self, _tx: ComponentEventSender) {
            // TestMockSource already has event_tx from constructor, so this is a no-op for tests
        }
    }

    /// Helper to create a TestMockSource instance
    pub fn create_test_mock_source(
        id: String,
        event_tx: ComponentEventSender,
    ) -> Arc<dyn Source> {
        let source = TestMockSource::new(id, event_tx).unwrap();
        Arc::new(source) as Arc<dyn Source>
    }

    /// A simple test mock reaction for unit testing the ReactionManager.
    /// This is NOT a real plugin - it's an inline test double.
    pub struct TestMockReaction {
        id: String,
        queries: Vec<String>,
        status: Arc<RwLock<ComponentStatus>>,
        #[allow(dead_code)]
        event_tx: ComponentEventSender,
    }

    impl TestMockReaction {
        pub fn new(id: String, queries: Vec<String>, event_tx: ComponentEventSender) -> Self {
            Self {
                id,
                queries,
                status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
                event_tx,
            }
        }
    }

    #[async_trait]
    impl crate::plugin_core::Reaction for TestMockReaction {
        fn id(&self) -> &str {
            &self.id
        }

        fn type_name(&self) -> &str {
            "log"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn query_ids(&self) -> Vec<String> {
            self.queries.clone()
        }

        async fn start(&self, _query_subscriber: Arc<dyn crate::plugin_core::QuerySubscriber>) -> anyhow::Result<()> {
            *self.status.write().await = ComponentStatus::Starting;

            // Send Starting event
            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Starting,
                timestamp: chrono::Utc::now(),
                message: Some("Starting reaction".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            *self.status.write().await = ComponentStatus::Running;

            // Send Running event
            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: Some("Reaction started".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            Ok(())
        }

        async fn stop(&self) -> anyhow::Result<()> {
            *self.status.write().await = ComponentStatus::Stopping;

            // Send Stopping event
            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Stopping,
                timestamp: chrono::Utc::now(),
                message: Some("Stopping reaction".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            *self.status.write().await = ComponentStatus::Stopped;

            // Send Stopped event
            let event = ComponentEvent {
                component_id: self.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Stopped,
                timestamp: chrono::Utc::now(),
                message: Some("Reaction stopped".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.status.read().await.clone()
        }

        async fn inject_event_tx(&self, _tx: ComponentEventSender) {
            // TestMockReaction already has event_tx from constructor, so this is a no-op for tests
        }
    }

    /// Helper to create a TestMockReaction instance
    pub fn create_test_mock_reaction(
        id: String,
        queries: Vec<String>,
        event_tx: ComponentEventSender,
    ) -> Arc<dyn crate::plugin_core::Reaction> {
        let reaction = TestMockReaction::new(id, queries, event_tx);
        Arc::new(reaction) as Arc<dyn crate::plugin_core::Reaction>
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub mod async_test_helpers {
    use std::future::Future;
    use tokio::time::{timeout, Duration};

    /// Runs an async function with a timeout
    pub async fn with_timeout<F, T>(duration: Duration, future: F) -> Result<T, String>
    where
        F: Future<Output = T>,
    {
        timeout(duration, future)
            .await
            .map_err(|_| "Test timed out".to_string())
    }

    /// Helper to wait for a condition to become true
    pub async fn wait_for_condition<F>(
        mut condition: F,
        max_duration: Duration,
    ) -> Result<(), String>
    where
        F: FnMut() -> bool,
    {
        let start = tokio::time::Instant::now();
        let check_interval = Duration::from_millis(10);

        while !condition() {
            if start.elapsed() > max_duration {
                return Err("Condition not met within timeout".to_string());
            }
            tokio::time::sleep(check_interval).await;
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub mod test_data {
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use serde_json::Value;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Creates a test node element
    pub fn create_test_node(source_name: &str, id: &str, labels: Vec<String>) -> Element {
        let reference = ElementReference::new(source_name, id);

        let mut property_map = ElementPropertyMap::new();
        property_map.insert(
            "test_property",
            crate::sources::convert_json_to_element_value(&Value::String("test_value".to_string()))
                .unwrap(),
        );

        let metadata = ElementMetadata {
            reference,
            labels: Arc::from(
                labels
                    .into_iter()
                    .map(|l| Arc::from(l.as_str()))
                    .collect::<Vec<_>>(),
            ),
            effective_from: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
        };

        Element::Node {
            metadata,
            properties: property_map,
        }
    }

    /// Creates a test source change
    pub fn create_test_source_change(op: &str, element: Element) -> SourceChange {
        match op {
            "i" => SourceChange::Insert { element },
            "u" => SourceChange::Update { element },
            "d" => {
                let metadata = match &element {
                    Element::Node { metadata, .. } => metadata.clone(),
                    Element::Relation { metadata, .. } => metadata.clone(),
                };
                SourceChange::Delete { metadata }
            }
            _ => panic!("Invalid operation: {}", op),
        }
    }
}

#[cfg(test)]
pub mod assertions {

    /// Custom assertion for checking component status
    #[macro_export]
    macro_rules! assert_component_status {
        ($component:expr, $expected:expr) => {
            assert_eq!(
                $component.status().await,
                $expected,
                "Component status mismatch. Expected {:?}, got {:?}",
                $expected,
                $component.status().await
            );
        };
    }

    /// Custom assertion for checking channel is empty
    #[macro_export]
    macro_rules! assert_channel_empty {
        ($receiver:expr) => {
            assert!(
                $receiver.try_recv().is_err(),
                "Expected channel to be empty, but it contained a message"
            );
        };
    }

    /// Custom assertion for checking channel has message
    #[macro_export]
    macro_rules! assert_channel_has_message {
        ($receiver:expr) => {
            assert!(
                $receiver.try_recv().is_ok(),
                "Expected channel to have a message, but it was empty"
            );
        };
    }
}
