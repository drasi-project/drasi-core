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

//! Tests for the MySQL Stored Procedure reaction.
//!
//! Unit tests cover configuration, template validation, route validation, and
//! template resolution. The end-to-end tests drive the reaction through
//! `enqueue_query_result` against a real MySQL database (via testcontainers)
//! and assert that the configured stored procedures ran with the expected,
//! positionally-bound parameters.

mod mysql_helpers;

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::reactions::common::OperationType;
use drasi_lib::Reaction;
use drasi_reaction_storedproc_mysql::config::{
    MySqlStoredProcReactionConfig, QueryConfig, TemplateSpec,
};
use drasi_reaction_storedproc_mysql::MySqlStoredProcReaction;
use mysql_helpers::{setup_mysql, MysqlConfig};
use serde_json::json;
use serial_test::serial;
use tokio::time::sleep;

// ============================================================================
// Database helpers
// ============================================================================

/// Create the log table and the ADD/UPDATE/DELETE stored procedures.
///
/// The UPDATE procedure records both the old and the new temperature so tests
/// can assert that `before` and `after` are bound independently.
async fn setup_stored_procedures(config: &MysqlConfig) {
    let opts = config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect to database");

    use mysql_async::prelude::Queryable;

    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS sensor_log (
            id INT AUTO_INCREMENT PRIMARY KEY,
            operation VARCHAR(10) NOT NULL,
            sensor_id TEXT NOT NULL,
            old_temperature DOUBLE,
            new_temperature DOUBLE,
            logged_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .await
    .expect("Failed to create sensor_log table");

    for proc in [
        "log_sensor_added",
        "log_sensor_updated",
        "log_sensor_deleted",
    ] {
        let _ = conn
            .query_drop(format!("DROP PROCEDURE IF EXISTS {proc}"))
            .await;
    }

    conn.query_drop(
        "CREATE PROCEDURE log_sensor_added(IN p_sensor_id TEXT, IN p_temperature DOUBLE)
        BEGIN
            INSERT INTO sensor_log (operation, sensor_id, new_temperature)
            VALUES ('ADD', p_sensor_id, p_temperature);
        END",
    )
    .await
    .expect("Failed to create log_sensor_added procedure");

    conn.query_drop(
        "CREATE PROCEDURE log_sensor_updated(
            IN p_sensor_id TEXT, IN p_old_temperature DOUBLE, IN p_new_temperature DOUBLE)
        BEGIN
            INSERT INTO sensor_log (operation, sensor_id, old_temperature, new_temperature)
            VALUES ('UPDATE', p_sensor_id, p_old_temperature, p_new_temperature);
        END",
    )
    .await
    .expect("Failed to create log_sensor_updated procedure");

    conn.query_drop(
        "CREATE PROCEDURE log_sensor_deleted(IN p_sensor_id TEXT)
        BEGIN
            INSERT INTO sensor_log (operation, sensor_id)
            VALUES ('DELETE', p_sensor_id);
        END",
    )
    .await
    .expect("Failed to create log_sensor_deleted procedure");

    drop(conn);
}

#[derive(Debug, Clone, PartialEq)]
struct SensorLogEntry {
    operation: String,
    sensor_id: String,
    old_temperature: Option<f64>,
    new_temperature: Option<f64>,
}

async fn get_log_entries(config: &MysqlConfig) -> Vec<SensorLogEntry> {
    use mysql_async::prelude::Queryable;
    let opts = config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect to database");

    let rows: Vec<(String, String, Option<f64>, Option<f64>)> = conn
        .query(
            "SELECT operation, sensor_id, old_temperature, new_temperature \
             FROM sensor_log ORDER BY id",
        )
        .await
        .expect("Failed to query sensor_log");

    drop(conn);
    rows.into_iter()
        .map(
            |(operation, sensor_id, old_temperature, new_temperature)| SensorLogEntry {
                operation,
                sensor_id,
                old_temperature,
                new_temperature,
            },
        )
        .collect()
}

async fn get_log_count(config: &MysqlConfig) -> i64 {
    use mysql_async::prelude::Queryable;
    let opts = config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect to database");

    let count: i64 = conn
        .query_first("SELECT COUNT(*) FROM sensor_log")
        .await
        .expect("Failed to query log count")
        .unwrap_or(0);

    drop(conn);
    count
}

/// Wait until the sensor_log row count reaches `expected`, panicking if the
/// count is not reached within `timeout_ms` so tests fail on a missing write
/// instead of silently passing.
async fn wait_for_log_count(config: &MysqlConfig, expected: i64, timeout_ms: u64) {
    let step = 100;
    let mut waited = 0;
    let mut last_count = 0;
    while waited < timeout_ms {
        last_count = get_log_count(config).await;
        if last_count >= expected {
            return;
        }
        sleep(Duration::from_millis(step)).await;
        waited += step;
    }
    panic!(
        "Timed out after {timeout_ms}ms waiting for sensor_log count to reach {expected}; \
         last observed count was {last_count}"
    );
}

fn make_query_result(query_id: &str, diffs: Vec<ResultDiff>) -> QueryResult {
    QueryResult::new(query_id.to_string(), 0, Utc::now(), diffs, HashMap::new())
}

// ============================================================================
// Unit tests — configuration
// ============================================================================

#[test]
fn test_config_default_values() {
    let config = MySqlStoredProcReactionConfig::default();
    // DevSkim: ignore DS137138
    assert_eq!(config.hostname, "localhost");
    assert_eq!(config.get_port(), 3306);
    assert_eq!(config.command_timeout_ms, 30000);
    assert_eq!(config.retry_attempts, 3);
    assert!(config.routes.is_empty());
    assert!(config.default_template.is_none());
}

#[test]
fn test_config_custom_port() {
    let config = MySqlStoredProcReactionConfig {
        port: Some(3307),
        ..Default::default()
    };
    assert_eq!(config.get_port(), 3307);
}

#[test]
fn test_config_validation_no_commands() {
    let config = MySqlStoredProcReactionConfig {
        user: "root".to_string(),
        database: "mydb".to_string(),
        ..Default::default()
    };
    assert!(config.validate().is_err());
}

#[test]
fn test_config_validation_empty_user_without_identity_provider() {
    let config = MySqlStoredProcReactionConfig {
        database: "mydb".to_string(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL test()")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("user is required"));
}

#[test]
fn test_config_validation_empty_database() {
    let config = MySqlStoredProcReactionConfig {
        user: "root".to_string(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL test()")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("Database name is required"));
}

#[test]
fn test_config_validation_valid_with_default_template() {
    let config = MySqlStoredProcReactionConfig {
        user: "root".to_string(),
        database: "mydb".to_string(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_item({{param after.id}})")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };
    assert!(config.validate().is_ok());
}

#[test]
fn test_config_validation_valid_with_routes() {
    let mut routes = HashMap::new();
    routes.insert(
        "item-query".to_string(),
        QueryConfig {
            added: None,
            updated: Some(TemplateSpec::new("CALL update_item({{param after.id}})")),
            deleted: None,
        },
    );
    let config = MySqlStoredProcReactionConfig {
        user: "root".to_string(),
        database: "mydb".to_string(),
        routes,
        ..Default::default()
    };
    assert!(config.validate().is_ok());
}

#[test]
fn test_config_validation_rejects_invalid_template() {
    let config = MySqlStoredProcReactionConfig {
        user: "root".to_string(),
        database: "mydb".to_string(),
        default_template: Some(QueryConfig {
            // Unterminated helper — should fail to compile.
            added: Some(TemplateSpec::new("CALL add_item({{param after.id}")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };
    assert!(config.validate().is_err());
}

#[test]
fn test_config_validation_ok_with_user_and_no_identity_provider() {
    // A user is only required when there is no identity provider. We can't
    // easily construct an IdentityProvider here, so this asserts the inverse
    // path via validate(): with a user set and no provider, validation passes.
    let config = MySqlStoredProcReactionConfig {
        user: "svc".to_string(),
        database: "mydb".to_string(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_item({{param after.id}})")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };
    assert!(config.validate().is_ok());
}

#[test]
fn test_config_serialization_roundtrip() {
    let config = MySqlStoredProcReactionConfig {
        hostname: "db.example.com".to_string(),
        port: Some(3307),
        user: "root".to_string(),
        password: "secret".to_string(),
        database: "mydb".to_string(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL add_user({{param after.id}}, {{param after.name}})",
            )),
            updated: None,
            deleted: Some(TemplateSpec::new("CALL delete_user({{param before.id}})")),
        }),
        ..Default::default()
    };
    let serialized = serde_json::to_string(&config).unwrap();
    let deserialized: MySqlStoredProcReactionConfig = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.hostname, "db.example.com");
    assert_eq!(deserialized.get_port(), 3307);
    assert_eq!(deserialized.database, "mydb");
    assert!(deserialized.default_template.is_some());
}

// ============================================================================
// Unit tests — template resolution & route validation
// ============================================================================

#[test]
fn test_resolve_prefers_route_over_default() {
    let mut routes = HashMap::new();
    routes.insert(
        "user-query".to_string(),
        QueryConfig {
            added: Some(TemplateSpec::new("CALL route_add({{param after.id}})")),
            updated: None,
            deleted: None,
        },
    );
    let config = MySqlStoredProcReactionConfig {
        routes,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL default_add({{param after.id}})")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };
    assert_eq!(
        config.resolve_command_template("user-query", OperationType::Add),
        Some("CALL route_add({{param after.id}})".to_string())
    );
    assert_eq!(
        config.resolve_command_template("other-query", OperationType::Add),
        Some("CALL default_add({{param after.id}})".to_string())
    );
}

#[test]
fn test_resolve_matches_last_dotted_segment() {
    let mut routes = HashMap::new();
    routes.insert(
        "my_query".to_string(),
        QueryConfig {
            added: Some(TemplateSpec::new("CALL seg_add({{param after.id}})")),
            updated: None,
            deleted: None,
        },
    );
    let config = MySqlStoredProcReactionConfig {
        routes,
        ..Default::default()
    };
    assert_eq!(
        config.resolve_command_template("source.my_query", OperationType::Add),
        Some("CALL seg_add({{param after.id}})".to_string())
    );
}

#[test]
fn test_resolve_returns_none_when_unconfigured() {
    let config = MySqlStoredProcReactionConfig {
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL add({{param after.id}})")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };
    assert!(config
        .resolve_command_template("q", OperationType::Delete)
        .is_none());
}

#[tokio::test]
async fn test_builder_rejects_unknown_route_key() {
    let mut routes = HashMap::new();
    routes.insert(
        "not-subscribed".to_string(),
        QueryConfig {
            added: Some(TemplateSpec::new("CALL add({{param after.id}})")),
            updated: None,
            deleted: None,
        },
    );
    let config = MySqlStoredProcReactionConfig {
        user: "root".to_string(),
        database: "mydb".to_string(),
        routes,
        ..Default::default()
    };
    let result = MySqlStoredProcReaction::builder("test")
        .with_query("subscribed-query")
        .with_config(config)
        .build()
        .await;
    let err = result.expect_err("build should fail").to_string();
    assert!(err.contains("route key 'not-subscribed'"));
}

#[tokio::test]
async fn test_builder_accepts_route_key_matching_segment() {
    let mut routes = HashMap::new();
    routes.insert(
        "my_query".to_string(),
        QueryConfig {
            added: Some(TemplateSpec::new("CALL add({{param after.id}})")),
            updated: None,
            deleted: None,
        },
    );
    let config = MySqlStoredProcReactionConfig {
        user: "root".to_string(),
        database: "mydb".to_string(),
        routes,
        ..Default::default()
    };
    let result = MySqlStoredProcReaction::builder("test")
        .with_query("source.my_query")
        .with_config(config)
        .build()
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_reaction_trait_surface() {
    let reaction = MySqlStoredProcReaction::builder("mysql-sp")
        .with_connection("localhost", 3306, "testdb", "testuser", "testpass")
        .with_query("q1")
        .with_stored_procedure("handle_change")
        .build()
        .await
        .unwrap();

    assert_eq!(reaction.id(), "mysql-sp");
    assert_eq!(reaction.type_name(), "storedproc-mysql");
    assert_eq!(reaction.query_ids(), vec!["q1".to_string()]);
    assert!(reaction.auto_start());
    assert!(!reaction.properties().is_empty());
}

// ============================================================================
// End-to-end tests (require Docker / testcontainers)
// ============================================================================

fn sensor_default_template() -> QueryConfig {
    QueryConfig {
        added: Some(TemplateSpec::new(
            "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
        )),
        updated: Some(TemplateSpec::new(
            "CALL log_sensor_updated({{param after.sensor_id}}, \
             {{param before.temperature}}, {{param after.temperature}})",
        )),
        deleted: Some(TemplateSpec::new(
            "CALL log_sensor_deleted({{param before.sensor_id}})",
        )),
    }
}

async fn build_started_reaction(
    config: &MysqlConfig,
    template: QueryConfig,
) -> MySqlStoredProcReaction {
    let reaction = MySqlStoredProcReaction::builder("mysql-e2e")
        .with_connection(
            &config.host,
            config.port,
            &config.database,
            &config.user,
            &config.password,
        )
        .with_query("sensor-query")
        .with_default_template(template)
        .build()
        .await
        .expect("build reaction");
    reaction.start().await.expect("start reaction");
    reaction
}

#[tokio::test]
#[serial]
async fn test_e2e_add_binds_parameters() {
    let mysql = setup_mysql().await;
    let config = mysql.config().clone();
    setup_stored_procedures(&config).await;

    let reaction = build_started_reaction(&config, sensor_default_template()).await;

    let qr = make_query_result(
        "sensor-query",
        vec![ResultDiff::Add {
            data: json!({"sensor_id": "sensor-1", "temperature": 21.5}),
            row_signature: 0,
        }],
    );
    reaction.enqueue_query_result(qr).await.expect("enqueue");
    wait_for_log_count(&config, 1, 5000).await;
    reaction.stop().await.expect("stop");

    let entries = get_log_entries(&config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].operation, "ADD");
    assert_eq!(entries[0].sensor_id, "sensor-1");
    assert_eq!(entries[0].new_temperature, Some(21.5));

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_e2e_update_binds_before_and_after_independently() {
    // Regression: UPDATE must bind the diff's real `before` and `after` rows,
    // not derive both from a single payload.
    let mysql = setup_mysql().await;
    let config = mysql.config().clone();
    setup_stored_procedures(&config).await;

    let reaction = build_started_reaction(&config, sensor_default_template()).await;

    let qr = make_query_result(
        "sensor-query",
        vec![ResultDiff::Update {
            data: json!({}),
            before: json!({"sensor_id": "sensor-2", "temperature": 10.0}),
            after: json!({"sensor_id": "sensor-2", "temperature": 42.0}),
            grouping_keys: None,
            row_signature: 0,
        }],
    );
    reaction.enqueue_query_result(qr).await.expect("enqueue");
    wait_for_log_count(&config, 1, 5000).await;
    reaction.stop().await.expect("stop");

    let entries = get_log_entries(&config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].operation, "UPDATE");
    assert_eq!(entries[0].sensor_id, "sensor-2");
    assert_eq!(entries[0].old_temperature, Some(10.0));
    assert_eq!(entries[0].new_temperature, Some(42.0));

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_e2e_aggregation_binds_before_and_after() {
    // ResultDiff::Aggregation maps to the UPDATE operation and must bind the
    // aggregation's `before` and `after` rows through the `updated` template.
    let mysql = setup_mysql().await;
    let config = mysql.config().clone();
    setup_stored_procedures(&config).await;

    let reaction = build_started_reaction(&config, sensor_default_template()).await;

    let qr = make_query_result(
        "sensor-query",
        vec![ResultDiff::Aggregation {
            before: Some(json!({"sensor_id": "sensor-agg", "temperature": 1.0})),
            after: json!({"sensor_id": "sensor-agg", "temperature": 2.0}),
            row_signature: 0,
        }],
    );
    reaction.enqueue_query_result(qr).await.expect("enqueue");
    wait_for_log_count(&config, 1, 5000).await;
    reaction.stop().await.expect("stop");

    let entries = get_log_entries(&config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].operation, "UPDATE");
    assert_eq!(entries[0].sensor_id, "sensor-agg");
    assert_eq!(entries[0].old_temperature, Some(1.0));
    assert_eq!(entries[0].new_temperature, Some(2.0));

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_e2e_aggregation_without_before_binds_after_only() {
    // An aggregation with `before: None` must not crash: build_context passes
    // `before.as_ref()` (None), and an after-only template renders and executes
    // normally.
    let mysql = setup_mysql().await;
    let config = mysql.config().clone();
    setup_stored_procedures(&config).await;

    // `updated` references only `after`, so a missing `before` does not error.
    let template = QueryConfig {
        added: None,
        updated: Some(TemplateSpec::new(
            "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
        )),
        deleted: None,
    };
    let reaction = build_started_reaction(&config, template).await;

    let qr = make_query_result(
        "sensor-query",
        vec![ResultDiff::Aggregation {
            before: None,
            after: json!({"sensor_id": "sensor-agg-none", "temperature": 7.5}),
            row_signature: 0,
        }],
    );
    reaction.enqueue_query_result(qr).await.expect("enqueue");
    wait_for_log_count(&config, 1, 5000).await;
    reaction.stop().await.expect("stop");

    let entries = get_log_entries(&config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].sensor_id, "sensor-agg-none");
    assert_eq!(entries[0].new_temperature, Some(7.5));

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_e2e_delete_uses_before_row() {
    let mysql = setup_mysql().await;
    let config = mysql.config().clone();
    setup_stored_procedures(&config).await;

    let reaction = build_started_reaction(&config, sensor_default_template()).await;

    let qr = make_query_result(
        "sensor-query",
        vec![ResultDiff::Delete {
            data: json!({"sensor_id": "sensor-3", "temperature": 5.0}),
            row_signature: 0,
        }],
    );
    reaction.enqueue_query_result(qr).await.expect("enqueue");
    wait_for_log_count(&config, 1, 5000).await;
    reaction.stop().await.expect("stop");

    let entries = get_log_entries(&config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].operation, "DELETE");
    assert_eq!(entries[0].sensor_id, "sensor-3");

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_e2e_render_failure_skips_event_and_continues() {
    // A template referencing a missing field must skip that event (not crash
    // the loop), and subsequent well-formed events must still be processed.
    let mysql = setup_mysql().await;
    let config = mysql.config().clone();
    setup_stored_procedures(&config).await;

    let template = QueryConfig {
        added: Some(TemplateSpec::new(
            "CALL log_sensor_added({{param after.sensor_id}}, {{param after.missing}})",
        )),
        updated: None,
        deleted: Some(TemplateSpec::new(
            "CALL log_sensor_deleted({{param before.sensor_id}})",
        )),
    };
    let reaction = build_started_reaction(&config, template).await;

    // This ADD references `after.missing` and should be skipped.
    reaction
        .enqueue_query_result(make_query_result(
            "sensor-query",
            vec![ResultDiff::Add {
                data: json!({"sensor_id": "bad", "temperature": 1.0}),
                row_signature: 0,
            }],
        ))
        .await
        .expect("enqueue");

    // This DELETE is well-formed and must still be processed.
    reaction
        .enqueue_query_result(make_query_result(
            "sensor-query",
            vec![ResultDiff::Delete {
                data: json!({"sensor_id": "good"}),
                row_signature: 0,
            }],
        ))
        .await
        .expect("enqueue");

    wait_for_log_count(&config, 1, 5000).await;
    reaction.stop().await.expect("stop");

    let entries = get_log_entries(&config).await;
    assert_eq!(
        entries.len(),
        1,
        "only the well-formed event should be logged"
    );
    assert_eq!(entries[0].operation, "DELETE");
    assert_eq!(entries[0].sensor_id, "good");

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_e2e_all_operations_in_sequence() {
    let mysql = setup_mysql().await;
    let config = mysql.config().clone();
    setup_stored_procedures(&config).await;

    let reaction = build_started_reaction(&config, sensor_default_template()).await;

    reaction
        .enqueue_query_result(make_query_result(
            "sensor-query",
            vec![
                ResultDiff::Add {
                    data: json!({"sensor_id": "s", "temperature": 1.0}),
                    row_signature: 0,
                },
                ResultDiff::Update {
                    data: json!({}),
                    before: json!({"sensor_id": "s", "temperature": 1.0}),
                    after: json!({"sensor_id": "s", "temperature": 2.0}),
                    grouping_keys: None,
                    row_signature: 0,
                },
                ResultDiff::Delete {
                    data: json!({"sensor_id": "s", "temperature": 2.0}),
                    row_signature: 0,
                },
                ResultDiff::Noop,
            ],
        ))
        .await
        .expect("enqueue");

    wait_for_log_count(&config, 3, 5000).await;
    reaction.stop().await.expect("stop");

    let entries = get_log_entries(&config).await;
    let ops: Vec<String> = entries.iter().map(|e| e.operation.clone()).collect();
    assert_eq!(ops, vec!["ADD", "UPDATE", "DELETE"]);

    mysql.cleanup().await;
}
