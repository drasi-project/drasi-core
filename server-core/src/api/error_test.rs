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

//! Tests for DrasiError

#[cfg(test)]
mod tests {
    use crate::api::DrasiError;
    use std::io;

    #[test]
    fn test_error_configuration() {
        let err = DrasiError::configuration("Invalid config");
        assert!(matches!(err, DrasiError::Configuration(_)));
        assert_eq!(err.to_string(), "Configuration error: Invalid config");
    }

    #[test]
    fn test_error_initialization() {
        let err = DrasiError::initialization("Failed to initialize");
        assert!(matches!(err, DrasiError::Initialization(_)));
        assert_eq!(
            err.to_string(),
            "Initialization error: Failed to initialize"
        );
    }

    #[test]
    fn test_error_component_not_found() {
        let err = DrasiError::component_not_found("source", "test-source");
        assert!(matches!(err, DrasiError::ComponentNotFound { .. }));
        assert_eq!(err.to_string(), "Component not found: source 'test-source'");
    }

    #[test]
    fn test_error_invalid_state() {
        let err = DrasiError::invalid_state("Cannot start - not initialized");
        assert!(matches!(err, DrasiError::InvalidState(_)));
        assert_eq!(
            err.to_string(),
            "Invalid state: Cannot start - not initialized"
        );
    }

    #[test]
    fn test_error_component_error() {
        let err = DrasiError::component_error("query", "test-query", "Parse error");
        assert!(matches!(err, DrasiError::ComponentError { .. }));
        assert_eq!(
            err.to_string(),
            "Component error (query 'test-query'): Parse error"
        );
    }

    #[test]
    fn test_error_serialization() {
        let err = DrasiError::serialization("Invalid YAML");
        assert!(matches!(err, DrasiError::Serialization(_)));
        assert_eq!(err.to_string(), "Serialization error: Invalid YAML");
    }

    #[test]
    fn test_error_duplicate_component() {
        let err = DrasiError::duplicate_component("source", "duplicate-source");
        assert!(matches!(err, DrasiError::DuplicateComponent { .. }));
        assert_eq!(
            err.to_string(),
            "Duplicate component: source 'duplicate-source' already exists"
        );
    }

    #[test]
    fn test_error_validation() {
        let err = DrasiError::validation("Source not found");
        assert!(matches!(err, DrasiError::Validation(_)));
        assert_eq!(err.to_string(), "Validation error: Source not found");
    }

    #[test]
    fn test_error_internal() {
        let err = DrasiError::internal("Unexpected error");
        assert!(matches!(err, DrasiError::Internal(_)));
        assert_eq!(err.to_string(), "Internal error: Unexpected error");
    }

    #[test]
    fn test_from_io_error() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "File not found");
        let err: DrasiError = io_err.into();
        assert!(matches!(err, DrasiError::Io(_)));
    }

    #[test]
    fn test_from_anyhow_error() {
        let anyhow_err = anyhow::anyhow!("Something went wrong");
        let err: DrasiError = anyhow_err.into();
        assert!(matches!(err, DrasiError::Internal(_)));
        assert!(err.to_string().contains("Something went wrong"));
    }

    #[test]
    fn test_from_serde_yaml_error() {
        let yaml_str = "invalid: yaml: structure:";
        let yaml_err = serde_yaml::from_str::<serde_yaml::Value>(yaml_str).unwrap_err();
        let err: DrasiError = yaml_err.into();
        assert!(matches!(err, DrasiError::Serialization(_)));
    }

    #[test]
    fn test_from_serde_json_error() {
        let json_str = "{invalid json}";
        let json_err = serde_json::from_str::<serde_json::Value>(json_str).unwrap_err();
        let err: DrasiError = json_err.into();
        assert!(matches!(err, DrasiError::Serialization(_)));
    }

    #[test]
    fn test_result_type_ok() {
        let result: crate::api::Result<i32> = Ok(42);
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_result_type_err() {
        let result: crate::api::Result<i32> = Err(DrasiError::configuration("test"));
        assert!(result.is_err());
    }

    #[test]
    fn test_component_not_found_various_kinds() {
        let source_err = DrasiError::component_not_found("source", "id");
        let query_err = DrasiError::component_not_found("query", "id");
        let reaction_err = DrasiError::component_not_found("reaction", "id");

        assert!(source_err.to_string().contains("source"));
        assert!(query_err.to_string().contains("query"));
        assert!(reaction_err.to_string().contains("reaction"));
    }

    #[test]
    fn test_duplicate_component_various_kinds() {
        let source_err = DrasiError::duplicate_component("source", "id");
        let query_err = DrasiError::duplicate_component("query", "id");
        let reaction_err = DrasiError::duplicate_component("reaction", "id");

        assert!(source_err.to_string().contains("source"));
        assert!(query_err.to_string().contains("query"));
        assert!(reaction_err.to_string().contains("reaction"));
    }

    #[test]
    fn test_database_connection_error_creation() {
        let err = DrasiError::database_connection("localhost:5432");
        assert!(matches!(err, DrasiError::DatabaseConnection { .. }));
        assert!(err.to_string().contains("Database connection error"));
        assert!(err.to_string().contains("localhost:5432"));
    }

    #[test]
    fn test_network_error_creation() {
        let err = DrasiError::Network {
            message: "Connection refused".to_string(),
            source: None,
        };
        assert!(matches!(err, DrasiError::Network { .. }));
        assert!(err.to_string().contains("Network error"));
        assert!(err.to_string().contains("Connection refused"));
    }

    #[test]
    fn test_bootstrap_error_creation() {
        let err = DrasiError::Bootstrap {
            message: "Provider not found".to_string(),
        };
        assert!(matches!(err, DrasiError::Bootstrap { .. }));
        assert!(err.to_string().contains("Bootstrap error"));
        assert!(err.to_string().contains("Provider not found"));
    }

    #[test]
    fn test_query_error_creation() {
        let err = DrasiError::Query {
            message: "Parse failed".to_string(),
        };
        assert!(matches!(err, DrasiError::Query { .. }));
        assert!(err.to_string().contains("Query error"));
        assert!(err.to_string().contains("Parse failed"));
    }

    #[test]
    fn test_timeout_error_creation() {
        let err = DrasiError::Timeout {
            operation: "database connection".to_string(),
        };
        assert!(matches!(err, DrasiError::Timeout { .. }));
        assert!(err.to_string().contains("timed out"));
        assert!(err.to_string().contains("database connection"));
    }

    #[test]
    fn test_helper_network_timeout() {
        let err = DrasiError::network_timeout("API call");
        assert!(matches!(err, DrasiError::Timeout { .. }));
        assert!(err.to_string().contains("API call"));
    }

    #[test]
    fn test_helper_network_connection_failed() {
        let err = DrasiError::network_connection_failed("example.com");
        assert!(matches!(err, DrasiError::Network { .. }));
        assert!(err.to_string().contains("example.com"));
    }

    #[test]
    fn test_helper_bootstrap_provider_not_found() {
        let err = DrasiError::bootstrap_provider_not_found("postgres");
        assert!(matches!(err, DrasiError::Bootstrap { .. }));
        assert!(err.to_string().contains("postgres"));
    }

    #[test]
    fn test_helper_bootstrap_incompatible_config() {
        let err = DrasiError::bootstrap_incompatible_config("Wrong source type");
        assert!(matches!(err, DrasiError::Bootstrap { .. }));
        assert!(err.to_string().contains("Wrong source type"));
    }

    #[test]
    fn test_helper_query_parse_error() {
        let err = DrasiError::query_parse_error("SELECT * FROM", "Unexpected EOF");
        assert!(matches!(err, DrasiError::Query { .. }));
        assert!(err.to_string().contains("SELECT * FROM"));
        assert!(err.to_string().contains("Unexpected EOF"));
    }

    #[test]
    fn test_helper_query_compilation_error() {
        let err = DrasiError::query_compilation_error("Unknown function");
        assert!(matches!(err, DrasiError::Query { .. }));
        assert!(err.to_string().contains("Unknown function"));
    }

    #[test]
    fn test_from_tokio_postgres_error() {
        // Note: Using __private_api_timeout() in test only to verify From<tokio_postgres::Error> trait.
        // Production code should use DrasiError::database_connection() for connection errors
        // or pass real PostgreSQL errors to DrasiError::database_query().
        let pg_err = tokio_postgres::Error::__private_api_timeout();
        let err: DrasiError = pg_err.into();
        assert!(matches!(err, DrasiError::Database { .. }));
    }
}
