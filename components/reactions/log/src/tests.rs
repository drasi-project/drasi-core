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
    use crate::{LogReaction, LogReactionConfig};
    use crate::config::{QueryTemplates, TemplateSpec};
    use drasi_lib::channels::ComponentStatus;
    use drasi_lib::plugin_core::Reaction;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_log_reaction_creation() {
        let config = LogReactionConfig::default();

        let reaction = LogReaction::new("test-log", vec!["query1".to_string()], config);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_with_default_templates() {
        let config = LogReactionConfig {
            added_template: Some("[NEW] Item {{after.id}}".to_string()),
            updated_template: Some("[CHG] {{before.value}} -> {{after.value}}".to_string()),
            deleted_template: Some("[DEL] Item {{before.id}}".to_string()),
            query_templates: HashMap::new(),
        };

        let reaction = LogReaction::new("test-log-templates", vec!["query1".to_string()], config);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_with_per_query_templates() {
        let mut query_templates = HashMap::new();
        query_templates.insert("sensor-query".to_string(), QueryTemplates {
            added: Some(TemplateSpec {
                template: "[SENSOR] New: {{after.id}}".to_string(),
            }),
            updated: Some(TemplateSpec {
                template: "[SENSOR-UPD] {{after.id}}".to_string(),
            }),
            deleted: None,
        });

        let config = LogReactionConfig {
            added_template: Some("[DEFAULT] {{after.id}}".to_string()),
            updated_template: None,
            deleted_template: Some("[DEFAULT-DEL] {{before.id}}".to_string()),
            query_templates,
        };

        let reaction = LogReaction::new(
            "test-log-per-query",
            vec!["sensor-query".to_string(), "other-query".to_string()],
            config,
        );
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
        assert_eq!(
            reaction.query_ids(),
            vec!["sensor-query".to_string(), "other-query".to_string()]
        );
    }

    #[tokio::test]
    async fn test_log_reaction_builder_default_templates() {
        let reaction = LogReaction::builder("test-log-builder")
            .from_query("query1")
            .with_added_template("[ADD] {{after.name}}")
            .with_updated_template("[UPD] {{after.name}}")
            .with_deleted_template("[DEL] {{before.name}}")
            .build();

        assert_eq!(reaction.id(), "test-log-builder");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_builder_no_templates() {
        let reaction = LogReaction::builder("test-log-no-templates")
            .from_query("query1")
            .from_query("query2")
            .build();

        assert_eq!(reaction.id(), "test-log-no-templates");
        assert_eq!(
            reaction.query_ids(),
            vec!["query1".to_string(), "query2".to_string()]
        );
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_builder_with_query_templates() {
        let reaction = LogReaction::builder("test-per-query-templates")
            .from_query("sensor-query")
            .from_query("user-query")
            .with_added_template("[DEFAULT] Added {{after.id}}")
            .with_updated_template("[DEFAULT] Updated {{after.id}}")
            .with_query_templates(
                "sensor-query",
                Some("[SENSOR-ADD] {{after.id}}: {{after.temperature}}°C"),
                Some("[SENSOR-UPD] {{before.temperature}}°C -> {{after.temperature}}°C"),
                Some("[SENSOR-DEL] {{before.id}}"),
            )
            .build();

        assert_eq!(reaction.id(), "test-per-query-templates");
        assert_eq!(
            reaction.query_ids(),
            vec!["sensor-query".to_string(), "user-query".to_string()]
        );
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_builder_individual_query_templates() {
        let reaction = LogReaction::builder("test-individual-templates")
            .from_query("query1")
            .from_query("query2")
            .with_added_template("[DEFAULT-ADD] {{after.id}}")
            .with_query_added_template("query1", "[Q1-ADD] {{after.name}}")
            .with_query_updated_template("query1", "[Q1-UPD] {{before.name}} -> {{after.name}}")
            .with_query_deleted_template("query2", "[Q2-DEL] Deleted {{before.id}}")
            .build();

        assert_eq!(reaction.id(), "test-individual-templates");
        assert_eq!(
            reaction.query_ids(),
            vec!["query1".to_string(), "query2".to_string()]
        );
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_builder_mixed_templates() {
        // Test that per-query templates override defaults
        let reaction = LogReaction::builder("test-mixed-templates")
            .from_query("special-query")
            .from_query("normal-query")
            // Set defaults for all queries
            .with_added_template("[DEFAULT] {{after.id}}")
            .with_updated_template("[DEFAULT] {{after.id}}")
            .with_deleted_template("[DEFAULT] {{before.id}}")
            // Override for specific query
            .with_query_added_template("special-query", "[SPECIAL] {{after.special_field}}")
            // normal-query will use defaults
            .build();

        assert_eq!(reaction.id(), "test-mixed-templates");
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_log_reaction_config_serialization() {
        let mut query_templates = HashMap::new();
        query_templates.insert("test-query".to_string(), QueryTemplates {
            added: Some(TemplateSpec {
                template: "Test {{after.id}}".to_string(),
            }),
            updated: None,
            deleted: None,
        });

        let config = LogReactionConfig {
            added_template: Some("Default {{after.id}}".to_string()),
            updated_template: None,
            deleted_template: None,
            query_templates,
        };

        // Test serialization
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("Default {{after.id}}"));
        assert!(json.contains("Test {{after.id}}"));

        // Test deserialization
        let deserialized: LogReactionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, config);
    }
}
