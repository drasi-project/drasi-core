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
        DrasiLibConfig, DrasiLibSettings, QueryConfig, ReactionConfig, SourceConfig,
    };
    use serde_json::json;
    use std::collections::HashMap;

    /// Helper to convert a serde_json::Value object to HashMap<String, serde_json::Value>
    fn to_hashmap(value: serde_json::Value) -> HashMap<String, serde_json::Value> {
        match value {
            serde_json::Value::Object(map) => map.into_iter().collect(),
            _ => HashMap::new(),
        }
    }

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
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        }
    }

    /// Creates a test source configuration
    pub fn create_test_source_config(id: &str, source_type: &str) -> SourceConfig {
        let config = match source_type {
            "mock" => crate::config::SourceSpecificConfig::Mock(to_hashmap(json!({
                "data_type": "counter",
                "interval_ms": 1000
            }))),
            "postgres" => crate::config::SourceSpecificConfig::Postgres(to_hashmap(json!({
                "host": "localhost",
                "port": 5432,
                "database": "test_db",
                "user": "test_user",
                "password": "",
                "tables": [],
                "slot_name": "drasi_slot",
                "publication_name": "drasi_publication",
                "ssl_mode": "prefer",
                "table_keys": []
            }))),
            "http" => crate::config::SourceSpecificConfig::Http(to_hashmap(json!({
                "host": "localhost",
                "port": 8080,
                "timeout_ms": 30000
            }))),
            "grpc" => crate::config::SourceSpecificConfig::Grpc(to_hashmap(json!({
                "host": "localhost",
                "port": 50051,
                "timeout_ms": 30000
            }))),
            "platform" => crate::config::SourceSpecificConfig::Platform(to_hashmap(json!({
                "redis_url": "redis://localhost:6379",
                "stream_key": "drasi:changes",
                "consumer_group": "drasi-core",
                "batch_size": 10,
                "block_ms": 5000
            }))),
            "application" => crate::config::SourceSpecificConfig::Application(to_hashmap(json!({
                "properties": {}
            }))),
            _ => crate::config::SourceSpecificConfig::Application(to_hashmap(json!({
                "properties": {}
            }))),
        };

        SourceConfig {
            id: id.to_string(),
            auto_start: true,
            config,
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
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

    /// Creates a test reaction configuration
    pub fn create_test_reaction_config(id: &str, queries: Vec<String>) -> ReactionConfig {
        ReactionConfig {
            id: id.to_string(),
            queries,
            auto_start: true,
            config: crate::config::ReactionSpecificConfig::Log(to_hashmap(json!({
                "log_level": "info"
            }))),
            priority_queue_capacity: None,
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
    use crate::config::SourceConfig;
    use crate::plugin_core::{ReactionRegistry, SourceRegistry};
    use crate::reactions::{MockReaction, Reaction};
    use crate::sources::Source;
    use anyhow::Result;
    use async_trait::async_trait;
    use drasi_core::models::SourceChange;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// A simple test mock source for unit testing the SourceManager.
    /// This is NOT a real plugin - it's an inline test double.
    ///
    /// This mock source supports event injection for testing data flow through queries.
    pub struct TestMockSource {
        config: SourceConfig,
        status: Arc<RwLock<ComponentStatus>>,
        #[allow(dead_code)]
        event_tx: ComponentEventSender,
        /// Dispatchers for sending events to subscribed queries
        dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
    }

    impl TestMockSource {
        pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
            Ok(Self {
                config,
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
                self.config.id.clone(),
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
        async fn start(&self) -> Result<()> {
            *self.status.write().await = ComponentStatus::Starting;

            // Send Starting event
            let event = ComponentEvent {
                component_id: self.config.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Starting,
                timestamp: chrono::Utc::now(),
                message: Some("Starting source".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            *self.status.write().await = ComponentStatus::Running;

            // Send Running event
            let event = ComponentEvent {
                component_id: self.config.id.clone(),
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
                component_id: self.config.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Stopping,
                timestamp: chrono::Utc::now(),
                message: Some("Stopping source".to_string()),
            };
            let _ = self.event_tx.send(event).await;

            *self.status.write().await = ComponentStatus::Stopped;

            // Send Stopped event
            let event = ComponentEvent {
                component_id: self.config.id.clone(),
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

        fn get_config(&self) -> &SourceConfig {
            &self.config
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
                source_id: self.config.id.clone(),
                receiver,
                bootstrap_receiver: None,
            })
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// Creates a SourceRegistry with the TestMockSource registered for "mock" type.
    /// Use this in unit tests instead of depending on plugin crates.
    pub fn create_test_source_registry() -> SourceRegistry {
        let mut registry = SourceRegistry::new();
        registry.register("mock".to_string(), |config, event_tx| {
            let source = TestMockSource::new(config, event_tx)?;
            Ok(Arc::new(source) as Arc<dyn Source>)
        });
        registry
    }

    /// Creates a ReactionRegistry with the MockReaction registered for "log" type.
    /// Use this in unit tests instead of depending on plugin crates.
    pub fn create_test_reaction_registry() -> ReactionRegistry {
        let mut registry = ReactionRegistry::new();
        registry.register("log".to_string(), |config, event_tx| {
            let reaction = MockReaction::new(config, event_tx);
            Ok(Arc::new(reaction) as Arc<dyn Reaction>)
        });
        registry
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
