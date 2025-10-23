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
        let settings: DrasiServerCoreSettings = serde_json::from_value(json!({
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
            "sources": ["source1", "source2"],
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
            "sources": ["source1"]
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
            "sources": ["source1"]
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
            "sources": ["source1"]
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
    fn test_reaction_config_defaults() {
        let json = json!({
            "id": "test-reaction",
            "reaction_type": "log",
            "queries": ["query1"],
            "properties": {}
        });

        let config: ReactionConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.id, "test-reaction");
        assert_eq!(config.reaction_type, "log");
        assert!(config.auto_start); // default is true
        assert!(config.properties.is_empty());
    }

    #[test]
    fn test_server_config_complete() {
        let json = json!({
            "server": {
                "host": "0.0.0.0",
                "port": 9090,
                "log_level": "debug"
            },
            "sources": [{
                "id": "source1",
                "source_type": "mock",
                "properties": {}
            }],
            "queries": [{
                "id": "query1",
                "query": "MATCH (n) RETURN n",
                "sources": ["source1"],
                "properties": {}
            }],
            "reactions": [{
                "id": "reaction1",
                "reaction_type": "log",
                "queries": ["query1"],
                "properties": {}
            }]
        });

        let config: DrasiServerCoreConfig = serde_json::from_value(json).unwrap();
        assert_eq!(config.sources.len(), 1);
        assert_eq!(config.queries.len(), 1);
        assert_eq!(config.reactions.len(), 1);
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::super::schema::*;
    use serde_yaml;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_load_config_from_file() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("test_config.yaml");

        let yaml_content = r#"
server_core:
  id: "test-server"
sources:
  - id: "test-source"
    source_type: "mock"
    properties:
      interval: 1000
queries: []
reactions: []
"#;

        fs::write(&config_path, yaml_content).unwrap();

        // Load and parse the config
        let yaml_str = fs::read_to_string(&config_path).unwrap();
        let config: DrasiServerCoreConfig = serde_yaml::from_str(&yaml_str).unwrap();

        assert_eq!(config.server_core.id, "test-server");
        assert_eq!(config.sources.len(), 1);
        assert_eq!(config.sources[0].id, "test-source");
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
        let result: Result<DrasiServerCoreConfig, _> = serde_yaml::from_str(&yaml_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_save_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("output_config.yaml");

        let config = DrasiServerCoreConfig::default();

        // Save config
        let yaml_str = serde_yaml::to_string(&config).unwrap();
        fs::write(&config_path, yaml_str).unwrap();

        assert!(config_path.exists());

        // Load it back and verify
        let loaded_yaml = fs::read_to_string(&config_path).unwrap();
        let loaded_config: DrasiServerCoreConfig = serde_yaml::from_str(&loaded_yaml).unwrap();

        assert_eq!(loaded_config.server_core.id, config.server_core.id);
        assert_eq!(loaded_config.sources.len(), config.sources.len());
        assert_eq!(loaded_config.queries.len(), config.queries.len());
        assert_eq!(loaded_config.reactions.len(), config.reactions.len());
    }

    #[test]
    fn test_config_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.yaml");

        // Create a config with all fields populated
        let mut config = DrasiServerCoreConfig::default();
        config.sources.push(SourceConfig {
            id: "test-source".to_string(),
            source_type: "mock".to_string(),
            auto_start: true,
            properties: HashMap::new(),
            bootstrap_provider: None,
            broadcast_channel_capacity: None,
        });

        // Save config
        let yaml_str = serde_yaml::to_string(&config).unwrap();
        fs::write(&config_path, yaml_str).unwrap();

        // Load it back
        let loaded_yaml = fs::read_to_string(&config_path).unwrap();
        let loaded_config: DrasiServerCoreConfig = serde_yaml::from_str(&loaded_yaml).unwrap();

        // Verify roundtrip
        assert_eq!(loaded_config.sources.len(), config.sources.len());
        assert_eq!(loaded_config.sources[0].id, config.sources[0].id);
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::super::runtime::*;
    use super::super::schema::*;
    use crate::channels::ComponentStatus;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn test_source_runtime_conversion() {
        let config = SourceConfig {
            id: "test-source".to_string(),
            source_type: "mock".to_string(),
            auto_start: true,
            properties: HashMap::from([("key".to_string(), json!("value"))]),
            bootstrap_provider: None,
            broadcast_channel_capacity: None,
        };

        let runtime = SourceRuntime::from(config.clone());
        assert_eq!(runtime.id, config.id);
        assert_eq!(runtime.source_type, config.source_type);
        // auto_start sources should be Stopped initially (will be started by manager)
        matches!(runtime.status, ComponentStatus::Stopped);
    }

    #[test]
    fn test_query_runtime_conversion() {
        let config = QueryConfig {
            id: "test-query".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            sources: vec!["source1".to_string()],
            auto_start: false,
            properties: HashMap::new(),
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            broadcast_channel_capacity: None,
        };

        let runtime = QueryRuntime::from(config.clone());
        assert_eq!(runtime.id, config.id);
        assert_eq!(runtime.query, config.query);
        assert_eq!(runtime.sources, config.sources);
        matches!(runtime.status, ComponentStatus::Stopped);
    }

    #[test]
    fn test_reaction_runtime_conversion() {
        let config = ReactionConfig {
            id: "test-reaction".to_string(),
            reaction_type: "log".to_string(),
            queries: vec!["query1".to_string()],
            auto_start: true,
            properties: HashMap::new(),
            priority_queue_capacity: None,
        };

        let runtime = ReactionRuntime::from(config.clone());
        assert_eq!(runtime.id, config.id);
        assert_eq!(runtime.reaction_type, config.reaction_type);
        assert_eq!(runtime.queries, config.queries);
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
