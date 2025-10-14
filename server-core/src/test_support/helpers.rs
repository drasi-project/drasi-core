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
        DrasiServerCoreConfig, DrasiServerCoreSettings, QueryConfig, ReactionConfig, SourceConfig,
    };
    use serde_json::json;
    use std::collections::HashMap;

    /// Creates a minimal valid server configuration for testing
    #[allow(dead_code)]
    pub fn create_test_server_config() -> DrasiServerCoreConfig {
        DrasiServerCoreConfig {
            server: DrasiServerCoreSettings {
                id: "test-server".to_string(),
                log_level: "debug".to_string(),
                max_connections: 100,
                shutdown_timeout_seconds: 30,
                disable_persistence: false,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        }
    }

    /// Creates a test source configuration
    pub fn create_test_source_config(id: &str, source_type: &str) -> SourceConfig {
        let mut properties = HashMap::new();

        match source_type {
            "mock" => {
                properties.insert("data_type".to_string(), json!("counter"));
                properties.insert("interval_ms".to_string(), json!(1000));
            }
            "process" => {
                properties.insert("executable".to_string(), json!("python3"));
                properties.insert("script".to_string(), json!("test.py"));
            }
            _ => {}
        }

        SourceConfig {
            id: id.to_string(),
            source_type: source_type.to_string(),
            auto_start: true,
            properties,
            bootstrap_provider: None,
        }
    }

    /// Creates a test query configuration
    pub fn create_test_query_config(id: &str, sources: Vec<String>) -> QueryConfig {
        use crate::config::QueryLanguage;
        QueryConfig {
            id: id.to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            sources,
            auto_start: true,
            properties: HashMap::new(),
            joins: None,
        }
    }

    /// Creates a test GQL query configuration
    pub fn create_test_gql_query_config(id: &str, sources: Vec<String>) -> QueryConfig {
        use crate::config::QueryLanguage;
        QueryConfig {
            id: id.to_string(),
            query: "MATCH (n:Person) RETURN n.name".to_string(),
            query_language: QueryLanguage::GQL,
            sources,
            auto_start: true,
            properties: HashMap::new(),
            joins: None,
        }
    }

    /// Creates a test reaction configuration
    pub fn create_test_reaction_config(id: &str, queries: Vec<String>) -> ReactionConfig {
        let mut properties = HashMap::new();
        properties.insert("log_level".to_string(), json!("info"));

        ReactionConfig {
            id: id.to_string(),
            reaction_type: "log".to_string(),
            queries,
            auto_start: true,
            properties,
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub mod mock_helpers {
    use crate::channels::*;
    use tokio::sync::mpsc;

    /// Creates a test channel pair for source changes
    pub fn create_test_source_change_channel() -> (SourceChangeSender, SourceChangeReceiver) {
        mpsc::channel(100)
    }

    /// Creates a test channel pair for query results
    pub fn create_test_query_result_channel() -> (QueryResultSender, QueryResultReceiver) {
        mpsc::channel(100)
    }

    /// Creates a test channel pair for component events
    pub fn create_test_event_channel() -> (ComponentEventSender, ComponentEventReceiver) {
        mpsc::channel(100)
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
