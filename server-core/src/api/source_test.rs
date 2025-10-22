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

//! Tests for SourceBuilder

#[cfg(test)]
mod tests {
    use crate::api::{Properties, Source};
    use crate::bootstrap::BootstrapProviderConfig;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn test_source_application() {
        let source = Source::application("app-source").build();

        assert_eq!(source.id, "app-source");
        assert_eq!(source.source_type, "application");
        assert!(
            source.auto_start,
            "Application source should auto-start by default"
        );
        assert!(source.properties.is_empty());
        assert!(source.bootstrap_provider.is_none());
    }

    #[test]
    fn test_source_postgres() {
        let source = Source::postgres("pg-source").build();

        assert_eq!(source.id, "pg-source");
        assert_eq!(source.source_type, "postgres");
        assert!(source.auto_start);
    }

    #[test]
    fn test_source_mock() {
        let source = Source::mock("mock-source").build();

        assert_eq!(source.id, "mock-source");
        assert_eq!(source.source_type, "mock");
        assert!(source.auto_start);
    }

    #[test]
    fn test_source_platform() {
        let source = Source::platform("platform-source").build();

        assert_eq!(source.id, "platform-source");
        assert_eq!(source.source_type, "platform");
        assert!(source.auto_start);
    }

    #[test]
    fn test_source_grpc() {
        let source = Source::grpc("grpc-source").build();

        assert_eq!(source.id, "grpc-source");
        assert_eq!(source.source_type, "grpc");
        assert!(source.auto_start);
    }

    #[test]
    fn test_source_http() {
        let source = Source::http("http-source").build();

        assert_eq!(source.id, "http-source");
        assert_eq!(source.source_type, "http");
        assert!(source.auto_start);
    }

    #[test]
    fn test_source_custom() {
        let source = Source::custom("custom-source", "custom-type").build();

        assert_eq!(source.id, "custom-source");
        assert_eq!(source.source_type, "custom-type");
        assert!(source.auto_start);
    }

    #[test]
    fn test_source_auto_start_false() {
        let source = Source::application("test-source").auto_start(false).build();

        assert!(!source.auto_start, "auto_start should be false");
    }

    #[test]
    fn test_source_auto_start_true() {
        let source = Source::application("test-source").auto_start(true).build();

        assert!(source.auto_start, "auto_start should be true");
    }

    #[test]
    fn test_source_with_property() {
        let source = Source::application("test-source")
            .with_property("host", json!("localhost"))
            .with_property("port", json!(5432))
            .build();

        assert_eq!(source.properties.len(), 2);
        assert_eq!(source.properties.get("host").unwrap(), &json!("localhost"));
        assert_eq!(source.properties.get("port").unwrap(), &json!(5432));
    }

    #[test]
    fn test_source_with_properties() {
        let props = Properties::new()
            .with_string("host", "localhost")
            .with_int("port", 5432)
            .with_bool("ssl", true);

        let source = Source::postgres("pg-source").with_properties(props).build();

        assert_eq!(source.properties.len(), 3);
        assert_eq!(source.properties.get("host").unwrap(), &json!("localhost"));
        assert_eq!(source.properties.get("port").unwrap(), &json!(5432));
        assert_eq!(source.properties.get("ssl").unwrap(), &json!(true));
    }

    #[test]
    fn test_source_with_bootstrap_config() {
        let bootstrap_config = BootstrapProviderConfig::Postgres {
            config: HashMap::new(),
        };

        let source = Source::postgres("pg-source")
            .with_bootstrap(bootstrap_config)
            .build();

        assert!(source.bootstrap_provider.is_some());
        match source.bootstrap_provider.unwrap() {
            BootstrapProviderConfig::Postgres { .. } => {}
            _ => panic!("Expected Postgres bootstrap provider"),
        }
    }

    #[test]
    fn test_source_with_script_bootstrap() {
        let files = vec!["/path/to/script1.jsonl".to_string()];
        let source = Source::mock("mock-source")
            .with_script_bootstrap(files.clone())
            .build();

        assert!(source.bootstrap_provider.is_some());
        match source.bootstrap_provider.unwrap() {
            BootstrapProviderConfig::ScriptFile { file_paths } => {
                assert_eq!(file_paths, files);
            }
            _ => panic!("Expected ScriptFile bootstrap provider"),
        }
    }

    #[test]
    fn test_source_with_platform_bootstrap() {
        let source = Source::platform("platform-source")
            .with_platform_bootstrap("http://localhost:8080", Some(300))
            .build();

        assert!(source.bootstrap_provider.is_some());
        match source.bootstrap_provider.unwrap() {
            BootstrapProviderConfig::Platform {
                query_api_url,
                timeout_seconds,
                ..
            } => {
                assert_eq!(query_api_url, Some("http://localhost:8080".to_string()));
                assert_eq!(timeout_seconds, Some(300));
            }
            _ => panic!("Expected Platform bootstrap provider"),
        }
    }

    #[test]
    fn test_source_with_postgres_bootstrap() {
        let source = Source::postgres("pg-source")
            .with_postgres_bootstrap()
            .build();

        assert!(source.bootstrap_provider.is_some());
        match source.bootstrap_provider.unwrap() {
            BootstrapProviderConfig::Postgres { .. } => {}
            _ => panic!("Expected Postgres bootstrap provider"),
        }
    }

    #[test]
    fn test_source_builder_chaining() {
        let source = Source::postgres("pg-source")
            .auto_start(false)
            .with_property("host", json!("localhost"))
            .with_property("port", json!(5432))
            .with_postgres_bootstrap()
            .build();

        assert_eq!(source.id, "pg-source");
        assert!(!source.auto_start);
        assert_eq!(source.properties.len(), 2);
        assert!(source.bootstrap_provider.is_some());
    }

    #[test]
    fn test_source_complex_config() {
        let source = Source::platform("complex-source")
            .auto_start(true)
            .with_properties(
                Properties::new()
                    .with_string("redis_url", "redis://localhost:6379")
                    .with_string("stream_key", "mystream")
                    .with_string("consumer_group", "group1")
                    .with_int("batch_size", 100)
                    .with_bool("auto_ack", true),
            )
            .with_platform_bootstrap("http://api:8080", Some(600))
            .build();

        assert_eq!(source.id, "complex-source");
        assert_eq!(source.source_type, "platform");
        assert!(source.auto_start);
        assert_eq!(source.properties.len(), 5);
        assert!(source.bootstrap_provider.is_some());
    }

    #[test]
    fn test_source_multiple_properties_override() {
        // Later properties should override earlier ones with same key
        let source = Source::application("test-source")
            .with_property("key", json!("value1"))
            .with_property("key", json!("value2"))
            .build();

        assert_eq!(source.properties.len(), 1);
        assert_eq!(source.properties.get("key").unwrap(), &json!("value2"));
    }

    #[test]
    fn test_source_with_properties_override_individual() {
        // with_properties should replace all previous properties
        let source = Source::application("test-source")
            .with_property("old_key", json!("old_value"))
            .with_properties(Properties::new().with_string("new_key", "new_value"))
            .build();

        assert_eq!(source.properties.len(), 1);
        assert!(source.properties.contains_key("new_key"));
        assert!(!source.properties.contains_key("old_key"));
    }

    #[test]
    fn test_source_empty_properties() {
        let source = Source::application("test-source")
            .with_properties(Properties::new())
            .build();

        assert!(source.properties.is_empty());
    }

    #[test]
    fn test_source_script_bootstrap_multiple_files() {
        let files = vec![
            "/path/to/script1.jsonl".to_string(),
            "/path/to/script2.jsonl".to_string(),
            "/path/to/script3.jsonl".to_string(),
        ];

        let source = Source::mock("multi-file-source")
            .with_script_bootstrap(files.clone())
            .build();

        match source.bootstrap_provider.unwrap() {
            BootstrapProviderConfig::ScriptFile { file_paths } => {
                assert_eq!(file_paths.len(), 3);
                assert_eq!(file_paths, files);
            }
            _ => panic!("Expected ScriptFile bootstrap provider"),
        }
    }

    #[test]
    fn test_source_platform_bootstrap_without_timeout() {
        let source = Source::platform("platform-source")
            .with_platform_bootstrap("http://localhost:8080", None)
            .build();

        match source.bootstrap_provider.unwrap() {
            BootstrapProviderConfig::Platform {
                query_api_url,
                timeout_seconds,
                ..
            } => {
                assert_eq!(query_api_url, Some("http://localhost:8080".to_string()));
                assert_eq!(timeout_seconds, None);
            }
            _ => panic!("Expected Platform bootstrap provider"),
        }
    }

}
