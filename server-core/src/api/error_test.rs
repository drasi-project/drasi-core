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
    fn test_error_display_configuration() {
        let err = DrasiError::Configuration("test message".to_string());
        assert_eq!(format!("{}", err), "Configuration error: test message");
    }

    #[test]
    fn test_error_display_component_not_found() {
        let err = DrasiError::ComponentNotFound {
            kind: "query".to_string(),
            id: "my-query".to_string(),
        };
        assert_eq!(format!("{}", err), "Component not found: query 'my-query'");
    }

    #[test]
    fn test_error_display_component_error() {
        let err = DrasiError::ComponentError {
            kind: "source".to_string(),
            id: "my-source".to_string(),
            message: "connection failed".to_string(),
        };
        assert_eq!(
            format!("{}", err),
            "Component error (source 'my-source'): connection failed"
        );
    }

    #[test]
    fn test_error_into_string() {
        let err = DrasiError::configuration("test");
        let msg = err.to_string();
        assert!(!msg.is_empty());
    }

    #[test]
    fn test_error_pattern_matching() {
        let err = DrasiError::component_not_found("source", "test-id");
        match err {
            DrasiError::ComponentNotFound { kind, id } => {
                assert_eq!(kind, "source");
                assert_eq!(id, "test-id");
            }
            _ => panic!("Wrong error variant"),
        }
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
    fn test_error_with_empty_message() {
        let err = DrasiError::configuration("");
        assert_eq!(err.to_string(), "Configuration error: ");
    }

    #[test]
    fn test_error_with_long_message() {
        let long_msg = "a".repeat(1000);
        let err = DrasiError::internal(&long_msg);
        assert!(err.to_string().contains(&long_msg));
    }

    #[test]
    fn test_error_with_special_characters() {
        let err = DrasiError::validation("Error: can't parse \"value\" with \n newline");
        assert!(err.to_string().contains("can't parse"));
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
}
