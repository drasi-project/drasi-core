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
mod tests {
    use crate::config::{QueryConfig, TemplateSpec};
    use crate::{LogReaction, LogReactionConfig};
    use drasi_lib::channels::ComponentStatus;
    use drasi_lib::Reaction;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_log_reaction_creation() {
        let config = LogReactionConfig::default();

        let reaction = LogReaction::new("test-log", vec!["query1".to_string()], config).unwrap();
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_with_default_template() {
        let default_template = QueryConfig {
            added: Some(TemplateSpec {
                template: "[NEW] Item {{after.id}}".to_string(),
            }),
            updated: Some(TemplateSpec {
                template: "[CHG] {{before.value}} -> {{after.value}}".to_string(),
            }),
            deleted: Some(TemplateSpec {
                template: "[DEL] Item {{before.id}}".to_string(),
            }),
        };

        let config = LogReactionConfig {
            routes: HashMap::new(),
            default_template: Some(default_template),
        };

        let reaction =
            LogReaction::new("test-log-templates", vec!["query1".to_string()], config).unwrap();
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_with_per_query_templates() {
        let mut routes = HashMap::new();
        routes.insert(
            "sensor-query".to_string(),
            QueryConfig {
                added: Some(TemplateSpec {
                    template: "[SENSOR] New: {{after.id}}".to_string(),
                }),
                updated: Some(TemplateSpec {
                    template: "[SENSOR-UPD] {{after.id}}".to_string(),
                }),
                deleted: None,
            },
        );

        let default_template = QueryConfig {
            added: Some(TemplateSpec {
                template: "[DEFAULT] {{after.id}}".to_string(),
            }),
            updated: None,
            deleted: Some(TemplateSpec {
                template: "[DEFAULT-DEL] {{before.id}}".to_string(),
            }),
        };

        let config = LogReactionConfig {
            routes,
            default_template: Some(default_template),
        };

        let reaction = LogReaction::new(
            "test-log-per-query",
            vec!["sensor-query".to_string(), "other-query".to_string()],
            config,
        )
        .unwrap();
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
        assert_eq!(
            reaction.query_ids(),
            vec!["sensor-query".to_string(), "other-query".to_string()]
        );
    }

    #[tokio::test]
    async fn test_log_reaction_builder_with_default_template() {
        let default_template = QueryConfig {
            added: Some(TemplateSpec {
                template: "[ADD] {{after.name}}".to_string(),
            }),
            updated: Some(TemplateSpec {
                template: "[UPD] {{after.name}}".to_string(),
            }),
            deleted: Some(TemplateSpec {
                template: "[DEL] {{before.name}}".to_string(),
            }),
        };

        let reaction = LogReaction::builder("test-log-builder")
            .with_query("query1")
            .with_default_template(default_template)
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "test-log-builder");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_builder_no_templates() {
        let reaction = LogReaction::builder("test-log-no-templates")
            .with_query("query1")
            .with_query("query2")
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "test-log-no-templates");
        assert_eq!(
            reaction.query_ids(),
            vec!["query1".to_string(), "query2".to_string()]
        );
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_builder_with_routes() {
        let sensor_config = QueryConfig {
            added: Some(TemplateSpec {
                template: "[SENSOR-ADD] {{after.id}}: {{after.temperature}}°C".to_string(),
            }),
            updated: Some(TemplateSpec {
                template: "[SENSOR-UPD] {{before.temperature}}°C -> {{after.temperature}}°C"
                    .to_string(),
            }),
            deleted: Some(TemplateSpec {
                template: "[SENSOR-DEL] {{before.id}}".to_string(),
            }),
        };

        let default_template = QueryConfig {
            added: Some(TemplateSpec {
                template: "[DEFAULT] Added {{after.id}}".to_string(),
            }),
            updated: Some(TemplateSpec {
                template: "[DEFAULT] Updated {{after.id}}".to_string(),
            }),
            deleted: None,
        };

        let reaction = LogReaction::builder("test-per-query-templates")
            .with_query("sensor-query")
            .with_query("user-query")
            .with_default_template(default_template)
            .with_route("sensor-query", sensor_config)
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "test-per-query-templates");
        assert_eq!(
            reaction.query_ids(),
            vec!["sensor-query".to_string(), "user-query".to_string()]
        );
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_config_serialization() {
        let mut routes = HashMap::new();
        routes.insert(
            "test-query".to_string(),
            QueryConfig {
                added: Some(TemplateSpec {
                    template: "Test {{after.id}}".to_string(),
                }),
                updated: None,
                deleted: None,
            },
        );

        let config = LogReactionConfig {
            routes,
            default_template: Some(QueryConfig {
                added: Some(TemplateSpec {
                    template: "Default {{after.id}}".to_string(),
                }),
                updated: None,
                deleted: None,
            }),
        };

        // Test serialization
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("Default {{after.id}}"));
        assert!(json.contains("Test {{after.id}}"));

        // Test deserialization
        let deserialized: LogReactionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, config);
    }

    #[tokio::test]
    async fn test_invalid_template_syntax() {
        let default_template = QueryConfig {
            added: Some(TemplateSpec {
                template: "[ADD] {{after.id".to_string(), // Missing closing brace
            }),
            updated: None,
            deleted: None,
        };

        let config = LogReactionConfig {
            routes: HashMap::new(),
            default_template: Some(default_template),
        };

        let result = LogReaction::new("test-invalid-template", vec!["query1".to_string()], config);
        assert!(result.is_err());
        let err = result.err().expect("expected error");
        assert!(err.to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_route_without_matching_query() {
        let mut routes = HashMap::new();
        routes.insert(
            "non-existent-query".to_string(),
            QueryConfig {
                added: Some(TemplateSpec {
                    template: "[ADD] {{after.id}}".to_string(),
                }),
                updated: None,
                deleted: None,
            },
        );

        let config = LogReactionConfig {
            routes,
            default_template: None,
        };

        let result = LogReaction::new(
            "test-invalid-route",
            vec!["query1".to_string(), "query2".to_string()],
            config,
        );
        assert!(result.is_err());
        let err = result.err().expect("expected error");
        assert!(err
            .to_string()
            .contains("does not match any subscribed query"));
    }

    #[tokio::test]
    async fn test_route_with_dotted_notation() {
        let mut routes = HashMap::new();
        routes.insert(
            "sensor-data".to_string(),
            QueryConfig {
                added: Some(TemplateSpec {
                    template: "[SENSOR] {{after.id}}".to_string(),
                }),
                updated: None,
                deleted: None,
            },
        );

        let config = LogReactionConfig {
            routes,
            default_template: None,
        };

        // Should match "source.sensor-data" with route "sensor-data"
        let result = LogReaction::new(
            "test-dotted-route",
            vec!["source.sensor-data".to_string()],
            config,
        );
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_builder_invalid_template() {
        let invalid_template = QueryConfig {
            added: Some(TemplateSpec {
                template: "{{unclosed".to_string(),
            }),
            updated: None,
            deleted: None,
        };

        let result = LogReaction::builder("test-builder-invalid")
            .with_query("query1")
            .with_default_template(invalid_template)
            .build();

        assert!(result.is_err());
        let err = result.err().expect("expected error");
        assert!(err.to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_builder_route_validation() {
        let sensor_config = QueryConfig {
            added: Some(TemplateSpec {
                template: "[SENSOR] {{after.id}}".to_string(),
            }),
            updated: None,
            deleted: None,
        };

        let result = LogReaction::builder("test-builder-route-validation")
            .with_query("query1")
            .with_route("unsubscribed-query", sensor_config)
            .build();

        assert!(result.is_err());
        let err = result.err().expect("expected error");
        assert!(err
            .to_string()
            .contains("does not match any subscribed query"));
    }

    #[tokio::test]
    async fn test_valid_complex_template() {
        let complex_template = QueryConfig {
            added: Some(TemplateSpec {
                template: r#"{"event": "added", "id": "{{after.id}}", "data": {{json after}}}"#
                    .to_string(),
            }),
            updated: Some(TemplateSpec {
                template:
                    r#"{"event": "updated", "before": {{json before}}, "after": {{json after}}}"#
                        .to_string(),
            }),
            deleted: Some(TemplateSpec {
                template: r#"{"event": "deleted", "id": "{{before.id}}"}"#.to_string(),
            }),
        };

        let config = LogReactionConfig {
            routes: HashMap::new(),
            default_template: Some(complex_template),
        };

        let result = LogReaction::new("test-complex-template", vec!["query1".to_string()], config);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_empty_template_is_valid() {
        let empty_template = QueryConfig {
            added: Some(TemplateSpec {
                template: String::new(), // Empty template should be valid
            }),
            updated: None,
            deleted: None,
        };

        let config = LogReactionConfig {
            routes: HashMap::new(),
            default_template: Some(empty_template),
        };

        let result = LogReaction::new("test-empty-template", vec!["query1".to_string()], config);
        assert!(result.is_ok());
    }
}
