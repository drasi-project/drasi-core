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

//! Tests for the PostgreSQL Stored Procedure Reaction
//!
//! These tests validate the end-to-end behavior of the PostgreSQL-specific
//! stored procedure reaction using testcontainers to provide a real PostgreSQL database.

mod postgres_helpers;

use chrono::Utc;
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::reactions::common;
use drasi_lib::reactions::common::TemplateRouting;
use drasi_lib::Reaction;
use drasi_reaction_storedproc_postgres::config::PostgresStoredProcReactionConfig;
use drasi_reaction_storedproc_postgres::executor::PostgresExecutor;
use drasi_reaction_storedproc_postgres::{PostgresStoredProcReaction, QueryConfig, TemplateSpec};
use postgres_helpers::{setup_postgres, PostgresConfig};
use serde_json::json;
use serial_test::serial;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;

// ============================================================================
// Test Helper Functions
// ============================================================================

/// Helper function to setup stored procedures in the test database
async fn setup_stored_procedures(config: &PostgresConfig) {
    let (client, connection) =
        tokio_postgres::connect(&config.connection_string(), tokio_postgres::NoTls)
            .await
            .expect("Failed to connect to database");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    // Create a table to log stored procedure calls
    client
        .execute(
            "CREATE TABLE IF NOT EXISTS sensor_log (
                id SERIAL PRIMARY KEY,
                operation VARCHAR(10) NOT NULL,
                sensor_id TEXT NOT NULL,
                temperature DOUBLE PRECISION,
                logged_at TIMESTAMPTZ DEFAULT NOW()
            )",
            &[],
        )
        .await
        .expect("Failed to create sensor_log table");

    // Create stored procedure for ADD operation
    client
        .execute(
            "CREATE OR REPLACE PROCEDURE log_sensor_added(
                p_sensor_id TEXT,
                p_temperature DOUBLE PRECISION
            )
            LANGUAGE plpgsql
            AS $$
            BEGIN
                INSERT INTO sensor_log (operation, sensor_id, temperature)
                VALUES ('ADD', p_sensor_id, p_temperature);
            END;
            $$",
            &[],
        )
        .await
        .expect("Failed to create log_sensor_added procedure");

    // Create stored procedure for UPDATE operation
    client
        .execute(
            "CREATE OR REPLACE PROCEDURE log_sensor_updated(
                p_sensor_id TEXT,
                p_temperature DOUBLE PRECISION
            )
            LANGUAGE plpgsql
            AS $$
            BEGIN
                INSERT INTO sensor_log (operation, sensor_id, temperature)
                VALUES ('UPDATE', p_sensor_id, p_temperature);
            END;
            $$",
            &[],
        )
        .await
        .expect("Failed to create log_sensor_updated procedure");

    // Create stored procedure for DELETE operation
    client
        .execute(
            "CREATE OR REPLACE PROCEDURE log_sensor_deleted(
                p_sensor_id TEXT
            )
            LANGUAGE plpgsql
            AS $$
            BEGIN
                INSERT INTO sensor_log (operation, sensor_id)
                VALUES ('DELETE', p_sensor_id);
            END;
            $$",
            &[],
        )
        .await
        .expect("Failed to create log_sensor_deleted procedure");
}

/// Helper function to get log entries from the database
async fn get_log_entries(config: &PostgresConfig) -> Vec<(String, String)> {
    let (client, connection) =
        tokio_postgres::connect(&config.connection_string(), tokio_postgres::NoTls)
            .await
            .expect("Failed to connect to database");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    let rows = client
        .query(
            "SELECT operation, sensor_id FROM sensor_log ORDER BY logged_at",
            &[],
        )
        .await
        .expect("Failed to query sensor_log");

    rows.iter().map(|row| (row.get(0), row.get(1))).collect()
}

/// Helper function to get log count
async fn get_log_count(config: &PostgresConfig) -> i64 {
    let (client, connection) =
        tokio_postgres::connect(&config.connection_string(), tokio_postgres::NoTls)
            .await
            .expect("Failed to connect to database");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    let row = client
        .query_one("SELECT COUNT(*) FROM sensor_log", &[])
        .await
        .expect("Failed to query log count");

    row.get(0)
}

// ============================================================================
// Unit Tests for Config Module
// ============================================================================

#[test]
fn test_config_default_values() {
    let config = PostgresStoredProcReactionConfig::default();

    assert_eq!(config.hostname, "localhost"); // DevSkim: ignore DS137138
    assert_eq!(config.port, None);
    assert_eq!(config.get_port(), 5432); // Default port
    assert_eq!(config.user, "");
    assert_eq!(config.password, "");
    assert_eq!(config.database, "");
    assert!(!config.ssl);
    assert!(config.routes.is_empty());
    assert_eq!(config.default_template, None);
    assert_eq!(config.command_timeout_ms, 30000);
    assert_eq!(config.retry_attempts, 3);
}

#[test]
fn test_config_custom_port() {
    let config = PostgresStoredProcReactionConfig {
        port: Some(5433),
        ..Default::default()
    };

    assert_eq!(config.get_port(), 5433);
}

#[test]
fn test_config_validation_no_commands() {
    let config = PostgresStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        ..Default::default()
    };

    let result = config.validate(&[]);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("At least one command"));
}

#[test]
fn test_config_validation_empty_user() {
    let config = PostgresStoredProcReactionConfig {
        user: "".to_string(),
        database: "testdb".to_string(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL test()")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };

    let result = config.validate(&[]);
    assert!(result.is_err());
}

#[test]
fn test_config_validation_empty_database() {
    let config = PostgresStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "".to_string(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL test()")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };

    let result = config.validate(&[]);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Database name is required"));
}

#[test]
fn test_config_validation_valid_with_added_template() {
    let config = PostgresStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_item({{param after.id}})")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };

    assert!(config.validate(&[]).is_ok());
}

#[test]
fn test_config_validation_valid_with_updated_template() {
    let config = PostgresStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        default_template: Some(QueryConfig {
            added: None,
            updated: Some(TemplateSpec::new("CALL update_item({{param after.id}})")),
            deleted: None,
        }),
        ..Default::default()
    };

    assert!(config.validate(&[]).is_ok());
}

#[test]
fn test_config_validation_valid_with_deleted_template() {
    let config = PostgresStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        default_template: Some(QueryConfig {
            added: None,
            updated: None,
            deleted: Some(TemplateSpec::new("CALL delete_item({{param before.id}})")),
        }),
        ..Default::default()
    };

    assert!(config.validate(&[]).is_ok());
}

#[test]
fn test_config_validation_valid_with_all_templates() {
    let config = PostgresStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_item({{param after.id}})")),
            updated: Some(TemplateSpec::new("CALL update_item({{param after.id}})")),
            deleted: Some(TemplateSpec::new("CALL delete_item({{param before.id}})")),
        }),
        ..Default::default()
    };

    assert!(config.validate(&[]).is_ok());
}

#[test]
fn test_config_serialization() {
    let config = PostgresStoredProcReactionConfig {
        hostname: "db.example.com".to_string(),
        port: Some(5433),
        identity_provider: None,
        user: "admin".to_string(),
        password: "secret".to_string(),
        database: "mydb".to_string(),
        ssl: true,
        routes: std::collections::HashMap::new(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL add_user({{param after.id}}, {{param after.name}})",
            )),
            updated: Some(TemplateSpec::new(
                "CALL update_user({{param after.id}}, {{param after.name}})",
            )),
            deleted: Some(TemplateSpec::new("CALL delete_user({{param before.id}})")),
        }),
        command_timeout_ms: 10000,
        retry_attempts: 5,
    };

    let json = serde_json::to_string(&config).unwrap();
    let deserialized: PostgresStoredProcReactionConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.hostname, "db.example.com");
    assert_eq!(deserialized.port, Some(5433));
    assert_eq!(deserialized.user, "admin");
    assert_eq!(deserialized.password, "secret");
    assert_eq!(deserialized.database, "mydb");
    assert!(deserialized.ssl);
    assert_eq!(deserialized.command_timeout_ms, 10000);
    assert_eq!(deserialized.retry_attempts, 5);
}

#[test]
fn test_config_deserialization_with_defaults() {
    let json = r#"{
        "user": "testuser",
        "password": "testpass",
        "database": "testdb",
        "default_template": {
            "added": {
                "template": "CALL test()"
            }
        }
    }"#;

    let config: PostgresStoredProcReactionConfig = serde_json::from_str(json).unwrap();

    assert_eq!(config.hostname, "localhost"); // DevSkim: ignore DS137138
    assert_eq!(config.port, None);
    assert_eq!(config.get_port(), 5432); // default port
    assert!(!config.ssl); // default
    assert_eq!(config.command_timeout_ms, 30000); // default
    assert_eq!(config.retry_attempts, 3); // default
    assert!(config.default_template.is_some());
    assert_eq!(
        config
            .default_template
            .as_ref()
            .unwrap()
            .added
            .as_ref()
            .unwrap()
            .template,
        "CALL test()"
    );
}

// ============================================================================
// Integration Tests with PostgreSQL
// ============================================================================

#[tokio::test]
async fn test_postgres_config_validation() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    // Valid config
    let config = PostgresStoredProcReactionConfig {
        hostname: "localhost".to_string(), // DevSkim: ignore DS137138
        port: Some(5432),
        user: "testuser".to_string(),
        password: "password".to_string(),
        database: "testdb".to_string(),
        ssl: false,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_user({{param after.id}})")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };

    assert!(config.validate(&[]).is_ok());

    // Invalid: empty user
    let mut invalid = config.clone();
    invalid.user = String::new();
    assert!(invalid.validate(&[]).is_err());

    // Invalid: empty database
    let mut invalid = config.clone();
    invalid.database = String::new();
    assert!(invalid.validate(&[]).is_err());

    // Invalid: no commands
    let mut invalid = config.clone();
    invalid.default_template = None;
    assert!(invalid.validate(&[]).is_err());
}

#[tokio::test]
async fn test_postgres_executor_connection() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL test()")),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
        ..Default::default()
    };

    let executor = PostgresExecutor::new(&config, None).await;
    assert!(executor.is_ok(), "Should create executor successfully");

    let executor = executor.unwrap();
    let result = executor.test_connection().await;
    assert!(result.is_ok(), "Connection test should pass");

    pg.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_postgres_executor_procedure_execution() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
        ..Default::default()
    };

    let executor = PostgresExecutor::new(&config, None).await.unwrap();

    // Execute the rendered command with positional bind parameters
    let params = vec![json!("sensor-001"), json!(25.5)];

    let result = executor
        .execute_command("CALL log_sensor_added($1, $2)", params)
        .await;
    assert!(
        result.is_ok(),
        "Command execution should succeed: {:?}",
        result.err()
    );

    // Wait a bit for the insert to complete
    sleep(Duration::from_millis(100)).await;

    // Verify the log entry
    let count = get_log_count(pg_config).await;
    assert_eq!(count, 1, "Should have 1 log entry");

    let entries = get_log_entries(pg_config).await;
    assert_eq!(entries[0].0, "ADD");
    assert_eq!(entries[0].1, "sensor-001");

    pg.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_postgres_executor_multiple_operations() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: Some(TemplateSpec::new(
                "CALL log_sensor_updated({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            deleted: Some(TemplateSpec::new(
                "CALL log_sensor_deleted({{param before.sensor_id}})",
            )),
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
        ..Default::default()
    };

    let executor = PostgresExecutor::new(&config, None).await.unwrap();

    // Execute ADD
    executor
        .execute_command(
            "CALL log_sensor_added($1, $2)",
            vec![json!("sensor-001"), json!(25.5)],
        )
        .await
        .expect("ADD should succeed");

    // Execute UPDATE
    executor
        .execute_command(
            "CALL log_sensor_updated($1, $2)",
            vec![json!("sensor-001"), json!(26.5)],
        )
        .await
        .expect("UPDATE should succeed");

    // Execute DELETE
    executor
        .execute_command("CALL log_sensor_deleted($1)", vec![json!("sensor-001")])
        .await
        .expect("DELETE should succeed");

    // Wait for operations to complete
    sleep(Duration::from_millis(100)).await;

    // Verify all operations logged
    let entries = get_log_entries(pg_config).await;
    assert_eq!(entries.len(), 3, "Should have 3 log entries");
    assert_eq!(entries[0].0, "ADD");
    assert_eq!(entries[1].0, "UPDATE");
    assert_eq!(entries[2].0, "DELETE");

    pg.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_postgres_execute_command_with_executor() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        identity_provider: None,
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config, None).await.unwrap();

    // Execute the rendered command with positional bind parameters
    let result = executor
        .execute_command(
            "CALL log_sensor_added($1, $2)",
            vec![json!("sensor-002"), json!(27.3)],
        )
        .await;
    assert!(result.is_ok());

    sleep(Duration::from_millis(100)).await;

    let entries = get_log_entries(pg_config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, "sensor-002");

    pg.cleanup().await;
}

#[tokio::test]
async fn test_postgres_reaction_creation() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        identity_provider: None,
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let reaction =
        PostgresStoredProcReaction::new("test-reaction", vec!["test-query".to_string()], config)
            .await;

    assert!(reaction.is_ok(), "Should create reaction successfully");

    let reaction = reaction.unwrap();
    assert_eq!(reaction.id(), "test-reaction");
    assert_eq!(reaction.type_name(), "storedproc-postgres");
    assert_eq!(reaction.query_ids(), vec!["test-query"]);

    pg.cleanup().await;
}

#[tokio::test]
async fn test_postgres_reaction_builder() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    let reaction = PostgresStoredProcReaction::builder("test-builder")
        .with_hostname(&pg_config.host)
        .with_port(pg_config.port)
        .with_database(&pg_config.database)
        .with_user(&pg_config.user)
        .with_password(&pg_config.password)
        .with_ssl(false)
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: None,
            deleted: None,
        })
        .with_query("test-query")
        .with_command_timeout_ms(5000)
        .with_retry_attempts(3)
        .build()
        .await;

    assert!(reaction.is_ok(), "Builder should create reaction");

    let reaction = reaction.unwrap();
    assert_eq!(reaction.id(), "test-builder");
    assert_eq!(reaction.query_ids(), vec!["test-query"]);

    pg.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_postgres_executor_with_special_characters() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        identity_provider: None,
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config, None).await.unwrap();

    // Test with special characters (potential SQL injection)
    let params = vec![json!("sensor'; DROP TABLE sensor_log; --"), json!(25.5)];

    let result = executor
        .execute_command("CALL log_sensor_added($1, $2)", params)
        .await;
    assert!(
        result.is_ok(),
        "Should safely handle special characters via parameterization"
    );

    sleep(Duration::from_millis(100)).await;

    // Verify the table still exists and has the entry
    let entries = get_log_entries(pg_config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, "sensor'; DROP TABLE sensor_log; --");

    pg.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_postgres_reaction_lifecycle() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    // Create the PostgreSQL stored procedure reaction
    let reaction = PostgresStoredProcReaction::builder("postgres-reaction")
        .with_hostname(&pg_config.host)
        .with_port(pg_config.port)
        .with_database(&pg_config.database)
        .with_user(&pg_config.user)
        .with_password(&pg_config.password)
        .with_ssl(false)
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: Some(TemplateSpec::new(
                "CALL log_sensor_updated({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            deleted: Some(TemplateSpec::new(
                "CALL log_sensor_deleted({{param before.sensor_id}})",
            )),
        })
        .with_query("sensor-query")
        .with_auto_start(false) // Don't auto-start for this test
        .build()
        .await
        .expect("Should create reaction");

    // Verify reaction properties
    assert_eq!(reaction.id(), "postgres-reaction");
    assert_eq!(reaction.type_name(), "storedproc-postgres");
    assert_eq!(reaction.query_ids(), vec!["sensor-query"]);
    assert!(!reaction.auto_start(), "Should not auto-start");

    // Verify initial status
    let status = reaction.status().await;
    assert_eq!(
        status,
        drasi_lib::channels::ComponentStatus::Stopped,
        "Reaction should start in Stopped status"
    );

    // Verify properties can be retrieved
    let properties = reaction.properties();
    assert_eq!(
        properties.get("hostname").and_then(|v| v.as_str()),
        Some(pg_config.host.as_str()),
        "Hostname should match"
    );
    assert_eq!(
        properties.get("database").and_then(|v| v.as_str()),
        Some(pg_config.database.as_str()),
        "Database should match config"
    );
    assert_eq!(
        properties.get("ssl").and_then(|v| v.as_bool()),
        Some(false),
        "SSL should be false"
    );

    pg.cleanup().await;
}

#[tokio::test]
async fn test_postgres_executor_retry_on_failure() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        identity_provider: None,
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL non_existent()")),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 1000,
        retry_attempts: 2,
    };

    let executor = PostgresExecutor::new(&config, None).await.unwrap();

    // Try to execute non-existent procedure (should fail after retries)
    let result = executor
        .execute_command("CALL non_existent_proc()", vec![])
        .await;

    assert!(
        result.is_err(),
        "Should fail after exhausting retry attempts"
    );

    pg.cleanup().await;
}

// ============================================================================
// Template and Route Tests
// ============================================================================

#[tokio::test]
#[serial]
async fn test_default_template_applies_to_all_queries() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    // Setup additional stored procedures for testing
    let (client, connection) = tokio_postgres::connect(
        &format!(
            "host={} port={} user={} password={} dbname={}",
            pg_config.host, pg_config.port, pg_config.user, pg_config.password, pg_config.database
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    client
        .execute(
            "CREATE OR REPLACE PROCEDURE log_query_event(
                p_query_name TEXT,
                p_sensor_id TEXT
            )
            LANGUAGE plpgsql
            AS $$
            BEGIN
                INSERT INTO sensor_log (operation, sensor_id, temperature)
                VALUES (p_query_name, p_sensor_id, 0);
            END;
            $$;",
            &[],
        )
        .await
        .unwrap();

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        identity_provider: None,
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(), // No routes specified
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_query_event({{param query_name}}, {{param after.sensor_id}})",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config, None).await.unwrap();

    // Execute the default-template command for query1
    executor
        .execute_command(
            "CALL log_query_event($1, $2)",
            vec![json!("query1"), json!("sensor-001")],
        )
        .await
        .expect("Should execute");

    // Execute the default-template command for query2
    executor
        .execute_command(
            "CALL log_query_event($1, $2)",
            vec![json!("query2"), json!("sensor-002")],
        )
        .await
        .expect("Should execute");

    sleep(Duration::from_millis(100)).await;

    // Verify both queries used the default template
    let entries = get_log_entries(pg_config).await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].1, "sensor-001");
    assert_eq!(entries[1].1, "sensor-002");

    pg.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_route_overrides_default_template() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    // Setup additional stored procedure
    let (client, connection) = tokio_postgres::connect(
        &format!(
            "host={} port={} user={} password={} dbname={}",
            pg_config.host, pg_config.port, pg_config.user, pg_config.password, pg_config.database
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    client
        .execute(
            "CREATE OR REPLACE PROCEDURE log_special_event(
                p_sensor_id TEXT,
                p_temperature DOUBLE PRECISION
            )
            LANGUAGE plpgsql
            AS $$
            BEGIN
                INSERT INTO sensor_log (operation, sensor_id, temperature)
                VALUES ('SPECIAL', p_sensor_id, p_temperature);
            END;
            $$;",
            &[],
        )
        .await
        .unwrap();

    let mut routes = HashMap::new();
    routes.insert(
        "special-query".to_string(),
        QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_special_event({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: None,
            deleted: None,
        },
    );

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        identity_provider: None,
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config, None).await.unwrap();

    // Execute with the route-specific rendered command
    executor
        .execute_command(
            "CALL log_special_event($1, $2)",
            vec![json!("sensor-special"), json!(99.9)],
        )
        .await
        .expect("Should execute");

    sleep(Duration::from_millis(100)).await;

    // Verify it used the special route (SPECIAL operation vs ADD)
    let entries = get_log_entries(pg_config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "SPECIAL");
    assert_eq!(entries[0].1, "sensor-special");

    pg.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_route_with_none_falls_back_to_default() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    let mut routes = HashMap::new();
    routes.insert(
        "partial-query".to_string(),
        QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: None, // Will fall back to default
            deleted: None,
        },
    );

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        identity_provider: None,
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes,
        default_template: Some(QueryConfig {
            added: None,
            updated: Some(TemplateSpec::new(
                "CALL log_sensor_updated({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    // Get the UPDATE template for "partial-query"
    let spec = config.get_template_spec("partial-query", common::OperationType::Update);

    // Should fall back to default template
    assert!(spec.is_some());
    assert_eq!(
        spec.unwrap().template,
        "CALL log_sensor_updated({{param after.sensor_id}}, {{param after.temperature}})"
    );

    pg.cleanup().await;
}

// ============================================================================
// Data Type Tests
// ============================================================================

#[tokio::test]
#[serial]
async fn test_executor_with_various_data_types() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();

    // Create a comprehensive test table
    let (client, connection) = tokio_postgres::connect(
        &format!(
            "host={} port={} user={} password={} dbname={}",
            pg_config.host, pg_config.port, pg_config.user, pg_config.password, pg_config.database
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    client
        .execute(
            "CREATE TABLE IF NOT EXISTS data_type_test (
                id SERIAL PRIMARY KEY,
                text_col TEXT,
                int_col INTEGER,
                float_col DOUBLE PRECISION,
                bool_col BOOLEAN
            )",
            &[],
        )
        .await
        .unwrap();

    client
        .execute(
            "CREATE OR REPLACE PROCEDURE test_data_types(
                p_text TEXT,
                p_int INTEGER,
                p_float DOUBLE PRECISION,
                p_bool BOOLEAN
            )
            LANGUAGE plpgsql
            AS $$
            BEGIN
                INSERT INTO data_type_test (text_col, int_col, float_col, bool_col)
                VALUES (p_text, p_int, p_float, p_bool);
            END;
            $$;",
            &[],
        )
        .await
        .unwrap();

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        identity_provider: None,
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config, None).await.unwrap();

    // Execute the rendered command with positional bind parameters covering
    // several JSON value types
    let result = executor
        .execute_command(
            "CALL test_data_types($1, $2, $3, $4)",
            vec![
                json!("test-string"),
                json!(42),
                json!(std::f64::consts::PI),
                json!(true),
            ],
        )
        .await;
    assert!(
        result.is_ok(),
        "Should handle various data types: {:?}",
        result.err()
    );

    sleep(Duration::from_millis(100)).await;

    // Verify the data was inserted correctly
    let rows = client
        .query(
            "SELECT text_col, int_col, float_col, bool_col FROM data_type_test",
            &[],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.get::<_, String>(0), "test-string");
    assert_eq!(row.get::<_, i32>(1), 42);
    assert!((row.get::<_, f64>(2) - std::f64::consts::PI).abs() < 0.00001);
    assert!(row.get::<_, bool>(3));

    pg.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_executor_with_string_numbers() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();

    let (client, connection) = tokio_postgres::connect(
        &format!(
            "host={} port={} user={} password={} dbname={}",
            pg_config.host, pg_config.port, pg_config.user, pg_config.password, pg_config.database
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    client
        .execute(
            "CREATE TABLE IF NOT EXISTS string_number_test (
                id SERIAL PRIMARY KEY,
                numeric_value DOUBLE PRECISION
            )",
            &[],
        )
        .await
        .unwrap();

    client
        .execute(
            "CREATE OR REPLACE PROCEDURE test_string_number(
                p_value DOUBLE PRECISION
            )
            LANGUAGE plpgsql
            AS $$
            BEGIN
                INSERT INTO string_number_test (numeric_value) VALUES (p_value);
            END;
            $$;",
            &[],
        )
        .await
        .unwrap();

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        identity_provider: None,
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config, None).await.unwrap();

    // Test with string that looks like a number (simulating MockSource behavior)
    let params = vec![json!("25.789")];

    let result = executor
        .execute_command("CALL test_string_number($1)", params)
        .await;
    assert!(
        result.is_ok(),
        "Should parse string numbers: {:?}",
        result.err()
    );

    sleep(Duration::from_millis(100)).await;

    // Verify it was stored as a number
    let rows = client
        .query("SELECT numeric_value FROM string_number_test", &[])
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let value = rows[0].get::<_, f64>(0);
    assert!((value - 25.789).abs() < 0.00001);

    pg.cleanup().await;
}

// ============================================================================
// End-to-End Pipeline Tests
//
// Unlike the executor tests above (which pass pre-rendered SQL), these push
// `QueryResult`s through `enqueue_query_result` so the reaction's processing
// loop performs the real Handlebars `{{param}}` render → positional-bind →
// execute pipeline against a live PostgreSQL instance.
// ============================================================================

/// Run a single SQL statement against the test database.
async fn exec_sql(config: &PostgresConfig, sql: &str) {
    let (client, connection) =
        tokio_postgres::connect(&config.connection_string(), tokio_postgres::NoTls)
            .await
            .expect("connect");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });
    client.execute(sql, &[]).await.expect("exec_sql");
}

/// Poll `count_sql` (which must return a single `bigint`) until it reaches
/// `expected` or the deadline elapses.
async fn wait_for_count(config: &PostgresConfig, count_sql: &str, expected: i64, timeout_ms: u64) {
    let (client, connection) =
        tokio_postgres::connect(&config.connection_string(), tokio_postgres::NoTls)
            .await
            .expect("connect");
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let count: i64 = client
            .query_one(count_sql, &[])
            .await
            .expect("count")
            .get(0);
        if count >= expected {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for count {expected} from `{count_sql}` (last={count})");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn add_diff(data: serde_json::Value) -> ResultDiff {
    ResultDiff::Add {
        data,
        row_signature: 0,
    }
}

/// Drives the full `{{param}}` render→execute pipeline for ADD, UPDATE and
/// DELETE through the reaction's processing loop against a real database.
#[tokio::test]
#[serial]
async fn test_end_to_end_param_pipeline_all_operations() {
    let pg = setup_postgres().await;
    let pg_config = pg.config();
    setup_stored_procedures(pg_config).await;

    let reaction = PostgresStoredProcReaction::builder("e2e-pipeline")
        .with_hostname(&pg_config.host)
        .with_port(pg_config.port)
        .with_database(&pg_config.database)
        .with_user(&pg_config.user)
        .with_password(&pg_config.password)
        .with_ssl(false)
        .with_query("sensor-query")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            updated: Some(TemplateSpec::new(
                "CALL log_sensor_updated({{param after.sensor_id}}, {{param after.temperature}})",
            )),
            deleted: Some(TemplateSpec::new(
                "CALL log_sensor_deleted({{param before.sensor_id}})",
            )),
        })
        .with_auto_start(false)
        .build()
        .await
        .expect("build reaction");

    reaction.start().await.expect("start");

    // ADD
    reaction
        .enqueue_query_result(QueryResult::new(
            "sensor-query".to_string(),
            1,
            Utc::now(),
            vec![add_diff(
                json!({"sensor_id": "sensor-e2e", "temperature": 21.5}),
            )],
            HashMap::new(),
        ))
        .await
        .expect("enqueue add");

    // UPDATE
    reaction
        .enqueue_query_result(QueryResult::new(
            "sensor-query".to_string(),
            2,
            Utc::now(),
            vec![ResultDiff::Update {
                data: json!({}),
                before: json!({"sensor_id": "sensor-e2e", "temperature": 21.5}),
                after: json!({"sensor_id": "sensor-e2e", "temperature": 22.7}),
                grouping_keys: None,
                row_signature: 0,
            }],
            HashMap::new(),
        ))
        .await
        .expect("enqueue update");

    // DELETE
    reaction
        .enqueue_query_result(QueryResult::new(
            "sensor-query".to_string(),
            3,
            Utc::now(),
            vec![ResultDiff::Delete {
                data: json!({"sensor_id": "sensor-e2e", "temperature": 22.7}),
                row_signature: 0,
            }],
            HashMap::new(),
        ))
        .await
        .expect("enqueue delete");

    wait_for_count(pg_config, "SELECT COUNT(*) FROM sensor_log", 3, 5000).await;
    reaction.stop().await.expect("stop");

    // Verify each operation rendered and executed the right procedure.
    let entries = get_log_entries(pg_config).await;
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0], ("ADD".to_string(), "sensor-e2e".to_string()));
    assert_eq!(entries[1], ("UPDATE".to_string(), "sensor-e2e".to_string()));
    assert_eq!(entries[2], ("DELETE".to_string(), "sensor-e2e".to_string()));

    pg.cleanup().await;
}

/// End-to-end proof of the new standard context keys (`query_name`,
/// `operation`) and whole-object JSONB binding (`{{param after}}`).
#[tokio::test]
#[serial]
async fn test_end_to_end_context_keys_and_json_binding() {
    let pg = setup_postgres().await;
    let pg_config = pg.config();

    exec_sql(
        pg_config,
        "CREATE TABLE IF NOT EXISTS ctx_capture (
            id SERIAL PRIMARY KEY,
            query_name TEXT,
            operation TEXT,
            payload JSONB
        )",
    )
    .await;
    exec_sql(
        pg_config,
        "CREATE OR REPLACE PROCEDURE capture_ctx(
            p_query TEXT, p_op TEXT, p_payload JSONB
        ) LANGUAGE plpgsql AS $$
        BEGIN
            INSERT INTO ctx_capture (query_name, operation, payload)
            VALUES (p_query, p_op, p_payload);
        END; $$",
    )
    .await;

    let reaction = PostgresStoredProcReaction::builder("e2e-ctx")
        .with_hostname(&pg_config.host)
        .with_port(pg_config.port)
        .with_database(&pg_config.database)
        .with_user(&pg_config.user)
        .with_password(&pg_config.password)
        .with_ssl(false)
        .with_query("ctx-query")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL capture_ctx({{param query_name}}, {{param operation}}, {{param after}})",
            )),
            updated: None,
            deleted: None,
        })
        .with_auto_start(false)
        .build()
        .await
        .expect("build reaction");

    reaction.start().await.expect("start");
    reaction
        .enqueue_query_result(QueryResult::new(
            "ctx-query".to_string(),
            1,
            Utc::now(),
            vec![add_diff(json!({"sensor_id": "s1", "temperature": 30.0}))],
            HashMap::new(),
        ))
        .await
        .expect("enqueue add");

    wait_for_count(pg_config, "SELECT COUNT(*) FROM ctx_capture", 1, 5000).await;
    reaction.stop().await.expect("stop");

    let (client, connection) =
        tokio_postgres::connect(&pg_config.connection_string(), tokio_postgres::NoTls)
            .await
            .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let row = client
        .query_one(
            "SELECT query_name, operation, payload->>'sensor_id', (payload->>'temperature')::float8
             FROM ctx_capture",
            &[],
        )
        .await
        .expect("query ctx_capture");

    assert_eq!(row.get::<_, String>(0), "ctx-query"); // query_name key
    assert_eq!(row.get::<_, String>(1), "ADD"); // operation key
    assert_eq!(row.get::<_, String>(2), "s1"); // whole-object JSONB payload
    assert!((row.get::<_, f64>(3) - 30.0).abs() < 1e-9);

    pg.cleanup().await;
}

/// End-to-end regression test for the string-binding fix: a numeric-looking
/// string bound to a TEXT column must be preserved verbatim (leading zeros
/// intact), rather than being coerced to a number.
#[tokio::test]
#[serial]
async fn test_end_to_end_numeric_string_preserved_in_text_column() {
    let pg = setup_postgres().await;
    let pg_config = pg.config();

    exec_sql(
        pg_config,
        "CREATE TABLE IF NOT EXISTS code_capture (id SERIAL PRIMARY KEY, code TEXT)",
    )
    .await;
    exec_sql(
        pg_config,
        "CREATE OR REPLACE PROCEDURE store_code(p_code TEXT)
         LANGUAGE plpgsql AS $$
         BEGIN
             INSERT INTO code_capture (code) VALUES (p_code);
         END; $$",
    )
    .await;

    let reaction = PostgresStoredProcReaction::builder("e2e-code")
        .with_hostname(&pg_config.host)
        .with_port(pg_config.port)
        .with_database(&pg_config.database)
        .with_user(&pg_config.user)
        .with_password(&pg_config.password)
        .with_ssl(false)
        .with_query("code-query")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("CALL store_code({{param after.code}})")),
            updated: None,
            deleted: None,
        })
        .with_auto_start(false)
        .build()
        .await
        .expect("build reaction");

    reaction.start().await.expect("start");
    reaction
        .enqueue_query_result(QueryResult::new(
            "code-query".to_string(),
            1,
            Utc::now(),
            // "007" parses as a number but must NOT be coerced for a TEXT column.
            vec![add_diff(json!({"code": "007"}))],
            HashMap::new(),
        ))
        .await
        .expect("enqueue add");

    wait_for_count(pg_config, "SELECT COUNT(*) FROM code_capture", 1, 5000).await;
    reaction.stop().await.expect("stop");

    let (client, connection) =
        tokio_postgres::connect(&pg_config.connection_string(), tokio_postgres::NoTls)
            .await
            .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let code: String = client
        .query_one("SELECT code FROM code_capture", &[])
        .await
        .expect("query code_capture")
        .get(0);

    assert_eq!(
        code, "007",
        "numeric-looking string must be preserved verbatim"
    );

    pg.cleanup().await;
}
