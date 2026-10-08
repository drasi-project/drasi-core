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
    use drasi_lib::context::workers::{spawn_owned_worker, WorkerAlreadyOwned, WorkerCleanupError};
    use drasi_lib::recovery::ReactionRecoveryPolicy;
    use drasi_lib::Reaction;
    use std::collections::HashMap;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn log_restarts_join_the_previous_processor() -> anyhow::Result<()> {
        let reaction = LogReaction::builder("log-restart")
            .with_query("q1")
            .build()?;
        reaction.stop().await?;
        for sequence in 1..=3 {
            reaction.start().await?;
            assert!(reaction
                .start()
                .await
                .unwrap_err()
                .is::<WorkerAlreadyOwned>());
            reaction
                .enqueue_query_result(drasi_lib::channels::QueryResult::new(
                    "q1".into(),
                    sequence,
                    chrono::Utc::now(),
                    vec![drasi_lib::channels::ResultDiff::Add {
                        data: serde_json::json!({"id":sequence}),
                        row_signature: sequence,
                    }],
                    HashMap::new(),
                ))
                .await?;
            tokio::time::timeout(Duration::from_secs(1), async {
                while !reaction.base.priority_queue.is_empty().await {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            tokio::time::timeout(Duration::from_secs(1), reaction.stop()).await??;
            assert_eq!(reaction.status().await, ComponentStatus::Stopped);
            assert!(reaction.base.processing_task.read().await.is_none());
        }
        reaction.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_log_start_and_worker_cleanup_block_replacement() -> anyhow::Result<()> {
        let reaction = LogReaction::builder("log-cleanup").build()?;
        let shutdown = reaction.base.shutdown_tx.write().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), reaction.start())
                .await
                .is_err()
        );
        assert!(reaction.base.processing_task.read().await.is_none());
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .is::<WorkerAlreadyOwned>());
        drop(shutdown);
        reaction.stop().await?;

        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        spawn_owned_worker(&reaction.base.processing_task, async move {
            wait.await.expect("release log worker");
        })
        .await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), reaction.stop())
                .await
                .is_err()
        );
        let error = reaction.stop().await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<WorkerCleanupError>(),
            Some(WorkerCleanupError::TimedOut { .. })
        ));
        tokio::task::yield_now().await;
        assert!(!reaction
            .base
            .processing_task
            .read()
            .await
            .as_ref()
            .unwrap()
            .is_finished());
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .is::<WorkerAlreadyOwned>());
        release.send(()).expect("live worker");
        reaction.stop().await?;

        spawn_owned_worker(&reaction.base.processing_task, async {
            panic!("log worker failed")
        })
        .await?;
        let error = reaction.stop().await.unwrap_err();
        assert!(
            matches!(error.downcast_ref::<WorkerCleanupError>(), Some(WorkerCleanupError::Join(cause)) if cause.is_panic())
        );
        assert!(reaction.base.processing_task.read().await.is_none());
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .is::<WorkerAlreadyOwned>());
        reaction.stop().await?;
        reaction.start().await?;
        reaction.stop().await?;
        Ok(())
    }

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
                ..Default::default()
            }),
            updated: Some(TemplateSpec {
                template: "[CHG] {{before.value}} -> {{after.value}}".to_string(),
                ..Default::default()
            }),
            deleted: Some(TemplateSpec {
                template: "[DEL] Item {{before.id}}".to_string(),
                ..Default::default()
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
                    ..Default::default()
                }),
                updated: Some(TemplateSpec {
                    template: "[SENSOR-UPD] {{after.id}}".to_string(),
                    ..Default::default()
                }),
                deleted: None,
            },
        );

        let default_template = QueryConfig {
            added: Some(TemplateSpec {
                template: "[DEFAULT] {{after.id}}".to_string(),
                ..Default::default()
            }),
            updated: None,
            deleted: Some(TemplateSpec {
                template: "[DEFAULT-DEL] {{before.id}}".to_string(),
                ..Default::default()
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
                ..Default::default()
            }),
            updated: Some(TemplateSpec {
                template: "[UPD] {{after.name}}".to_string(),
                ..Default::default()
            }),
            deleted: Some(TemplateSpec {
                template: "[DEL] {{before.name}}".to_string(),
                ..Default::default()
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
                ..Default::default()
            }),
            updated: Some(TemplateSpec {
                template: "[SENSOR-UPD] {{before.temperature}}°C -> {{after.temperature}}°C"
                    .to_string(),
                ..Default::default()
            }),
            deleted: Some(TemplateSpec {
                template: "[SENSOR-DEL] {{before.id}}".to_string(),
                ..Default::default()
            }),
        };

        let default_template = QueryConfig {
            added: Some(TemplateSpec {
                template: "[DEFAULT] Added {{after.id}}".to_string(),
                ..Default::default()
            }),
            updated: Some(TemplateSpec {
                template: "[DEFAULT] Updated {{after.id}}".to_string(),
                ..Default::default()
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
                    ..Default::default()
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
                    ..Default::default()
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
                ..Default::default()
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
                    ..Default::default()
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
                    ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
            }),
            updated: Some(TemplateSpec {
                template:
                    r#"{"event": "updated", "before": {{json before}}, "after": {{json after}}}"#
                        .to_string(),
                ..Default::default()
            }),
            deleted: Some(TemplateSpec {
                template: r#"{"event": "deleted", "id": "{{before.id}}"}"#.to_string(),
                ..Default::default()
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
                ..Default::default()
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

    #[tokio::test]
    async fn test_recovery_trait_defaults() {
        let config = LogReactionConfig::default();
        let reaction = LogReaction::new("test-log", vec![], config).unwrap();

        assert!(!reaction.is_durable());
        assert!(!reaction.needs_snapshot_on_fresh_start());
        assert_eq!(
            reaction.default_recovery_policy(),
            ReactionRecoveryPolicy::AutoSkipGap
        );
    }
}
