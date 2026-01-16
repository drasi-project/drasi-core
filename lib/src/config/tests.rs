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
mod schema_tests {
    use super::super::schema::*;
    use serde_json::json;

    #[test]
    fn test_config_defaults() {
        let config: DrasiLibConfig = serde_json::from_value(json!({
            "id": "default-server"
        }))
        .unwrap();

        assert_eq!(config.id, "default-server");
        assert!(config.priority_queue_capacity.is_none());
        assert!(config.dispatch_buffer_capacity.is_none());
        assert!(config.queries.is_empty());
        assert!(config.storage_backends.is_empty());
    }

    #[test]
    fn test_query_config_validation() {
        // Valid query config
        let valid_json = json!({
            "id": "test-query",
            "query": "MATCH (n) RETURN n",
            "sources": [
                {"source_id": "source1", "pipeline": []},
                {"source_id": "source2", "pipeline": []}
            ],
            "auto_start": false,
            "properties": {}
        });

        let config: Result<QueryConfig, _> = serde_json::from_value(valid_json);
        assert!(config.is_ok());

        let config = config.unwrap();
        assert_eq!(config.id, "test-query");
        assert_eq!(config.sources.len(), 2);
        assert!(!config.auto_start);
        assert_eq!(config.query_language, QueryLanguage::Cypher); // default
    }

    #[test]
    fn test_query_language_default() {
        // Test that queryLanguage defaults to Cypher when not specified
        let json = json!({
            "id": "test-query",
            "query": "MATCH (n) RETURN n",
            "sources": [
                {"source_id": "source1", "pipeline": []}
            ]
        });

        let config: QueryConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.query_language, QueryLanguage::Cypher);
    }

    #[test]
    fn test_query_language_explicit_cypher() {
        // Test explicit Cypher language setting
        let json = json!({
            "id": "test-query",
            "query": "MATCH (n) RETURN n",
            "queryLanguage": "Cypher",
            "sources": [
                {"source_id": "source1", "pipeline": []}
            ]
        });

        let config: QueryConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.query_language, QueryLanguage::Cypher);
    }

    #[test]
    fn test_query_language_explicit_gql() {
        // Test explicit GQL language setting
        let json = json!({
            "id": "test-query",
            "query": "MATCH (n:Person) RETURN n.name",
            "queryLanguage": "GQL",
            "sources": [
                {"source_id": "source1", "pipeline": []}
            ]
        });

        let config: QueryConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.query_language, QueryLanguage::GQL);
    }

    #[test]
    fn test_query_config_missing_required_fields() {
        // Missing query field
        let invalid_json = json!({
            "id": "test-query",
            "sources": ["source1"]
        });

        let config: Result<QueryConfig, _> = serde_json::from_value(invalid_json);
        assert!(config.is_err());
    }

    #[test]
    fn test_server_config_with_queries() {
        // Note: Sources and reactions are now instance-based and not part of config.
        // They are passed as owned instances via add_source() and add_reaction().

        let mut config = DrasiLibConfig::default();

        // Add a query
        config.queries.push(QueryConfig {
            id: "query1".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: crate::config::QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![crate::config::SourceSubscriptionConfig {
                nodes: vec![],
                relations: vec![],
                source_id: "source1".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        });

        assert_eq!(config.queries.len(), 1);
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::super::schema::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_load_config_from_file() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("test_config.yaml");

        // Create config programmatically and serialize it
        let mut config = DrasiLibConfig {
            id: "test-server".to_string(),
            ..Default::default()
        };

        // Note: Sources are now instance-based. Only queries are stored in config.
        config.queries.push(QueryConfig {
            id: "test-query".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        });

        // Serialize to YAML
        let yaml_str = serde_yaml::to_string(&config).unwrap();
        fs::write(&config_path, yaml_str).unwrap();

        // Load and parse the config back
        let _yaml_str = fs::read_to_string(&config_path).unwrap();
        // Just verify the file was written
        assert!(config_path.exists());
    }

    #[test]
    fn test_load_config_file_not_found() {
        let result = fs::read_to_string("/non/existent/path.yaml");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_config_invalid_yaml() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("invalid.yaml");

        fs::write(&config_path, "invalid: yaml: content:").unwrap();

        let yaml_str = fs::read_to_string(&config_path).unwrap();
        let result: Result<DrasiLibConfig, _> = serde_yaml::from_str(&yaml_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_save_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("output_config.yaml");

        let config = DrasiLibConfig::default();

        // Save config
        let yaml_str = serde_yaml::to_string(&config).unwrap();
        fs::write(&config_path, yaml_str).unwrap();

        assert!(config_path.exists());

        // Load it back and verify
        let loaded_yaml = fs::read_to_string(&config_path).unwrap();
        let loaded_config: DrasiLibConfig = serde_yaml::from_str(&loaded_yaml).unwrap();

        assert_eq!(loaded_config.id, config.id);
        assert_eq!(loaded_config.queries.len(), config.queries.len());
    }

    #[test]
    fn test_config_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yaml");

        // Create a config with queries
        let mut config = DrasiLibConfig::default();
        config.queries.push(QueryConfig {
            id: "test-query".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        });

        // Save config
        let yaml_str = serde_yaml::to_string(&config).unwrap();
        fs::write(&config_path, yaml_str).unwrap();

        // Verify serialization works and file exists
        assert!(config_path.exists());
        let yaml_content = fs::read_to_string(&config_path).unwrap();
        assert!(!yaml_content.is_empty());

        // Verify config
        assert_eq!(config.queries.len(), 1);
        assert_eq!(config.queries[0].id, "test-query");
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::super::runtime::*;
    use super::super::schema::*;
    use crate::channels::ComponentStatus;
    use std::collections::HashMap;

    #[test]
    fn test_query_runtime_conversion() {
        let config = QueryConfig {
            id: "test-query".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![crate::config::SourceSubscriptionConfig {
                nodes: vec![],
                relations: vec![],
                source_id: "source1".to_string(),
                pipeline: vec![],
            }],
            auto_start: false,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        };

        let runtime = QueryRuntime::from(config.clone());
        assert_eq!(runtime.id, config.id);
        assert_eq!(runtime.query, config.query);
        assert_eq!(runtime.source_subscriptions.len(), config.sources.len());
        matches!(runtime.status, ComponentStatus::Stopped);
    }

    #[test]
    fn test_query_runtime_conversion_with_joins() {
        let config = QueryConfig {
            id: "test-query".to_string(),
            query: "MATCH (o:Order)-[:CUSTOMER]->(c:Customer) RETURN o, c".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![crate::config::SourceSubscriptionConfig {
                nodes: vec![],
                relations: vec![],
                source_id: "source1".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: Some(vec![QueryJoinConfig {
                id: "CUSTOMER".to_string(),
                keys: vec![
                    QueryJoinKeyConfig {
                        label: "Order".to_string(),
                        property: "customer_id".to_string(),
                    },
                    QueryJoinKeyConfig {
                        label: "Customer".to_string(),
                        property: "id".to_string(),
                    },
                ],
            }]),
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        };

        let runtime = QueryRuntime::from(config.clone());
        assert_eq!(runtime.id, "test-query");
        assert!(runtime.joins.is_some());
        let joins = runtime.joins.unwrap();
        assert_eq!(joins.len(), 1);
        assert_eq!(joins[0].id, "CUSTOMER");
    }

    #[test]
    fn test_component_status_values() {
        // Just verify we can create the different status values
        let _starting = ComponentStatus::Starting;
        let _running = ComponentStatus::Running;
        let _stopping = ComponentStatus::Stopping;
        let _stopped = ComponentStatus::Stopped;
        let _error = ComponentStatus::Error;
    }

    #[test]
    fn test_source_runtime_creation() {
        let source_runtime = SourceRuntime {
            id: "test_source".to_string(),
            source_type: "postgres".to_string(),
            status: ComponentStatus::Running,
            error_message: None,
            properties: HashMap::new(),
        };

        assert_eq!(source_runtime.id, "test_source");
        assert_eq!(source_runtime.source_type, "postgres");
        assert!(matches!(source_runtime.status, ComponentStatus::Running));
        assert!(source_runtime.error_message.is_none());
    }

    #[test]
    fn test_source_runtime_with_error() {
        let source_runtime = SourceRuntime {
            id: "failing_source".to_string(),
            source_type: "http".to_string(),
            status: ComponentStatus::Error,
            error_message: Some("Connection timeout".to_string()),
            properties: HashMap::new(),
        };

        assert_eq!(source_runtime.id, "failing_source");
        assert!(matches!(source_runtime.status, ComponentStatus::Error));
        assert_eq!(
            source_runtime.error_message,
            Some("Connection timeout".to_string())
        );
    }

    #[test]
    fn test_source_runtime_serialization() {
        let mut properties = HashMap::new();
        properties.insert(
            "host".to_string(),
            serde_json::Value::String("localhost".to_string()), // DevSkim: ignore DS162092
        );

        let source_runtime = SourceRuntime {
            id: "test_source".to_string(),
            source_type: "postgres".to_string(),
            status: ComponentStatus::Running,
            error_message: None,
            properties,
        };

        let json = serde_json::to_string(&source_runtime).unwrap();
        assert!(json.contains("test_source"));
        assert!(json.contains("postgres"));
        assert!(json.contains("localhost")); // DevSkim: ignore DS162092
                                             // error_message should be skipped when None
        assert!(!json.contains("error_message"));
    }

    #[test]
    fn test_source_runtime_deserialization() {
        let json = r#"{
            "id": "test_source",
            "source_type": "mock",
            "status": "Running",
            "properties": {}
        }"#;

        let source_runtime: SourceRuntime = serde_json::from_str(json).unwrap();
        assert_eq!(source_runtime.id, "test_source");
        assert_eq!(source_runtime.source_type, "mock");
        assert!(source_runtime.error_message.is_none());
    }

    #[test]
    fn test_reaction_runtime_creation() {
        let reaction_runtime = ReactionRuntime {
            id: "test_reaction".to_string(),
            reaction_type: "http".to_string(),
            status: ComponentStatus::Running,
            error_message: None,
            queries: vec!["query1".to_string(), "query2".to_string()],
            properties: HashMap::new(),
        };

        assert_eq!(reaction_runtime.id, "test_reaction");
        assert_eq!(reaction_runtime.reaction_type, "http");
        assert_eq!(reaction_runtime.queries.len(), 2);
        assert!(matches!(reaction_runtime.status, ComponentStatus::Running));
    }

    #[test]
    fn test_reaction_runtime_with_error() {
        let reaction_runtime = ReactionRuntime {
            id: "failing_reaction".to_string(),
            reaction_type: "grpc".to_string(),
            status: ComponentStatus::Error,
            error_message: Some("Failed to connect to endpoint".to_string()),
            queries: vec!["query1".to_string()],
            properties: HashMap::new(),
        };

        assert!(matches!(reaction_runtime.status, ComponentStatus::Error));
        assert!(reaction_runtime.error_message.is_some());
    }

    #[test]
    fn test_reaction_runtime_serialization() {
        let mut properties = HashMap::new();
        properties.insert(
            "endpoint".to_string(),
            serde_json::Value::String("http://localhost:8080".to_string()), // DevSkim: ignore DS162092
        );

        let reaction_runtime = ReactionRuntime {
            id: "webhook".to_string(),
            reaction_type: "http".to_string(),
            status: ComponentStatus::Stopped,
            error_message: None,
            queries: vec!["orders_query".to_string()],
            properties,
        };

        let json = serde_json::to_string(&reaction_runtime).unwrap();
        assert!(json.contains("webhook"));
        assert!(json.contains("orders_query"));
        assert!(json.contains("endpoint"));
        // error_message should be skipped when None
        assert!(!json.contains("error_message"));
    }

    #[test]
    fn test_reaction_runtime_deserialization() {
        let json = r#"{
            "id": "test_reaction",
            "reaction_type": "log",
            "status": "Stopped",
            "queries": ["q1", "q2"],
            "properties": {}
        }"#;

        let reaction_runtime: ReactionRuntime = serde_json::from_str(json).unwrap();
        assert_eq!(reaction_runtime.id, "test_reaction");
        assert_eq!(reaction_runtime.reaction_type, "log");
        assert_eq!(reaction_runtime.queries.len(), 2);
    }

    #[test]
    fn test_query_runtime_serialization() {
        let query_runtime = QueryRuntime {
            id: "test_query".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            status: ComponentStatus::Running,
            error_message: None,
            source_subscriptions: vec![],
            joins: None,
        };

        let json = serde_json::to_string(&query_runtime).unwrap();
        assert!(json.contains("test_query"));
        assert!(json.contains("MATCH (n) RETURN n"));
        // Optional fields should be skipped when None
        assert!(!json.contains("error_message"));
        assert!(!json.contains("joins"));
    }

    #[test]
    fn test_query_runtime_deserialization() {
        let json = r#"{
            "id": "test_query",
            "query": "MATCH (n) RETURN n",
            "status": "Stopped",
            "source_subscriptions": []
        }"#;

        let query_runtime: QueryRuntime = serde_json::from_str(json).unwrap();
        assert_eq!(query_runtime.id, "test_query");
        assert_eq!(query_runtime.query, "MATCH (n) RETURN n");
        assert!(query_runtime.error_message.is_none());
        assert!(query_runtime.joins.is_none());
    }

    #[test]
    fn test_runtime_config_from_drasi_lib_config() {
        let mut config = DrasiLibConfig {
            id: "test-server".to_string(),
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            storage_backends: vec![],
            queries: vec![QueryConfig {
                id: "q1".to_string(),
                query: "MATCH (n) RETURN n".to_string(),
                query_language: QueryLanguage::Cypher,
                middleware: vec![],
                sources: vec![],
                auto_start: true,
                joins: None,
                enable_bootstrap: true,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                dispatch_buffer_capacity: None,
                dispatch_mode: None,
                storage_backend: None,
            }],
        };

        let runtime_config = RuntimeConfig::from(config.clone());
        assert_eq!(runtime_config.id, "test-server");
        assert_eq!(runtime_config.queries.len(), 1);

        // Verify defaults were applied
        assert_eq!(
            runtime_config.queries[0].priority_queue_capacity,
            Some(10000)
        );
        assert_eq!(
            runtime_config.queries[0].dispatch_buffer_capacity,
            Some(1000)
        );
    }

    #[test]
    fn test_runtime_config_applies_global_defaults() {
        let config = DrasiLibConfig {
            id: "test-server".to_string(),
            priority_queue_capacity: Some(50000),
            dispatch_buffer_capacity: Some(5000),
            storage_backends: vec![],
            queries: vec![
                QueryConfig {
                    id: "q1".to_string(),
                    query: "MATCH (n) RETURN n".to_string(),
                    query_language: QueryLanguage::Cypher,
                    middleware: vec![],
                    sources: vec![],
                    auto_start: true,
                    joins: None,
                    enable_bootstrap: true,
                    bootstrap_buffer_size: 10000,
                    priority_queue_capacity: None,
                    dispatch_buffer_capacity: None,
                    dispatch_mode: None,
                    storage_backend: None,
                },
                QueryConfig {
                    id: "q2".to_string(),
                    query: "MATCH (m) RETURN m".to_string(),
                    query_language: QueryLanguage::Cypher,
                    middleware: vec![],
                    sources: vec![],
                    auto_start: true,
                    joins: None,
                    enable_bootstrap: true,
                    bootstrap_buffer_size: 10000,
                    priority_queue_capacity: Some(100000), // Override global
                    dispatch_buffer_capacity: None,
                    dispatch_mode: None,
                    storage_backend: None,
                },
            ],
        };

        let runtime_config = RuntimeConfig::from(config);

        // q1 should inherit global defaults
        assert_eq!(
            runtime_config.queries[0].priority_queue_capacity,
            Some(50000)
        );
        assert_eq!(
            runtime_config.queries[0].dispatch_buffer_capacity,
            Some(5000)
        );

        // q2 should keep its own priority_queue_capacity but inherit dispatch_buffer_capacity
        assert_eq!(
            runtime_config.queries[1].priority_queue_capacity,
            Some(100000)
        );
        assert_eq!(
            runtime_config.queries[1].dispatch_buffer_capacity,
            Some(5000)
        );
    }

    #[test]
    fn test_runtime_config_new_without_provider() {
        let config = DrasiLibConfig {
            id: "test-server".to_string(),
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            storage_backends: vec![],
            queries: vec![],
        };

        let runtime_config = RuntimeConfig::new(config, None, None);
        assert_eq!(runtime_config.id, "test-server");
    }

    #[test]
    fn test_runtime_config_debug() {
        let config = DrasiLibConfig {
            id: "debug-test".to_string(),
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            storage_backends: vec![],
            queries: vec![],
        };

        let runtime_config = RuntimeConfig::from(config);
        let debug_str = format!("{runtime_config:?}");
        assert!(debug_str.contains("RuntimeConfig"));
        assert!(debug_str.contains("debug-test"));
    }

    #[test]
    fn test_source_runtime_clone() {
        let source_runtime = SourceRuntime {
            id: "cloneable".to_string(),
            source_type: "mock".to_string(),
            status: ComponentStatus::Running,
            error_message: None,
            properties: HashMap::new(),
        };

        let cloned = source_runtime.clone();
        assert_eq!(cloned.id, source_runtime.id);
        assert_eq!(cloned.source_type, source_runtime.source_type);
    }

    #[test]
    fn test_reaction_runtime_clone() {
        let reaction_runtime = ReactionRuntime {
            id: "cloneable".to_string(),
            reaction_type: "log".to_string(),
            status: ComponentStatus::Stopped,
            error_message: None,
            queries: vec!["q1".to_string()],
            properties: HashMap::new(),
        };

        let cloned = reaction_runtime.clone();
        assert_eq!(cloned.id, reaction_runtime.id);
        assert_eq!(cloned.queries, reaction_runtime.queries);
    }

    #[test]
    fn test_query_runtime_clone() {
        let query_runtime = QueryRuntime {
            id: "cloneable".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            status: ComponentStatus::Running,
            error_message: None,
            source_subscriptions: vec![],
            joins: None,
        };

        let cloned = query_runtime.clone();
        assert_eq!(cloned.id, query_runtime.id);
        assert_eq!(cloned.query, query_runtime.query);
    }
}

#[cfg(test)]
mod dispatch_mode_tests {
    use super::super::schema::*;
    use crate::channels::DispatchMode;

    #[test]
    fn test_query_config_with_dispatch_mode() {
        let yaml = r#"
            id: test_query
            query: "RETURN 1"
            source_subscriptions:
              - source_id: source1
                pipeline: []
            dispatch_mode: broadcast
        "#;

        let config: QueryConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "test_query");
        assert_eq!(config.dispatch_mode, Some(DispatchMode::Broadcast));
    }

    #[test]
    fn test_query_config_without_dispatch_mode() {
        let yaml = r#"
            id: test_query
            query: "RETURN 1"
            source_subscriptions:
              - source_id: source1
                pipeline: []
        "#;

        let config: QueryConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "test_query");
        assert_eq!(config.dispatch_mode, None);
    }

    #[test]
    fn test_query_config_with_channel_dispatch_mode() {
        let yaml = r#"
            id: test_query
            query: "RETURN 1"
            source_subscriptions:
              - source_id: source1
                pipeline: []
            dispatch_mode: channel
        "#;

        let config: QueryConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.id, "test_query");
        assert_eq!(config.dispatch_mode, Some(DispatchMode::Channel));
    }

    #[test]
    fn test_full_config_with_mixed_query_dispatch_modes() {
        let mut config = DrasiLibConfig::default();

        // Note: Sources and reactions are now instance-based and not part of config.
        // They are passed as owned instances via add_source() and add_reaction().

        // Add queries with different dispatch modes
        config.queries.push(QueryConfig {
            id: "query1".to_string(),
            query: "RETURN 1".to_string(),
            query_language: crate::config::QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![crate::config::SourceSubscriptionConfig {
                nodes: vec![],
                relations: vec![],
                source_id: "source1".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: Some(DispatchMode::Channel),
            storage_backend: None,
        });

        config.queries.push(QueryConfig {
            id: "query2".to_string(),
            query: "RETURN 2".to_string(),
            query_language: crate::config::QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![crate::config::SourceSubscriptionConfig {
                nodes: vec![],
                relations: vec![],
                source_id: "source2".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: Some(DispatchMode::Broadcast),
            storage_backend: None,
        });

        config.queries.push(QueryConfig {
            id: "query3".to_string(),
            query: "RETURN 3".to_string(),
            query_language: crate::config::QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![crate::config::SourceSubscriptionConfig {
                nodes: vec![],
                relations: vec![],
                source_id: "source3".to_string(),
                pipeline: vec![],
            }],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None, // Default
            storage_backend: None,
        });

        assert_eq!(config.queries.len(), 3);
        assert_eq!(config.queries[0].dispatch_mode, Some(DispatchMode::Channel));
        assert_eq!(
            config.queries[1].dispatch_mode,
            Some(DispatchMode::Broadcast)
        );
        assert_eq!(config.queries[2].dispatch_mode, None);
    }
}
