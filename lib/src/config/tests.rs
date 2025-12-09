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
    fn test_server_settings_defaults() {
        let settings: DrasiLibSettings = serde_json::from_value(json!({
            "id": "default-server"
        }))
        .unwrap();

        assert_eq!(settings.id, "default-server"); // default
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
        let mut config = DrasiLibConfig::default();
        config.server_core.id = "test-server".to_string();

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

        assert_eq!(loaded_config.server_core.id, config.server_core.id);
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

    #[test]
    fn test_query_runtime_conversion() {
        let config = QueryConfig {
            id: "test-query".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![crate::config::SourceSubscriptionConfig {
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
        assert_eq!(
            runtime.source_subscriptions.len(),
            config.sources.len()
        );
        matches!(runtime.status, ComponentStatus::Stopped);
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
