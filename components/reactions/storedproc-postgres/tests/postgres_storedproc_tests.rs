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

use drasi_lib::plugin_core::Reaction;
use drasi_lib::reactions::common;
use drasi_reaction_storedproc_postgres::config::PostgresStoredProcReactionConfig;
use drasi_reaction_storedproc_postgres::executor::PostgresExecutor;
use drasi_reaction_storedproc_postgres::parser::ParameterParser;
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

    let result = config.validate();
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

    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("user is required"));
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

    let result = config.validate();
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
            added: Some(TemplateSpec::new("CALL add_item(@after.id)")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };

    assert!(config.validate().is_ok());
}

#[test]
fn test_config_validation_valid_with_updated_template() {
    let config = PostgresStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        default_template: Some(QueryConfig {
            added: None,
            updated: Some(TemplateSpec::new("CALL update_item(@after.id)")),
            deleted: None,
        }),
        ..Default::default()
    };

    assert!(config.validate().is_ok());
}

#[test]
fn test_config_validation_valid_with_deleted_template() {
    let config = PostgresStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        default_template: Some(QueryConfig {
            added: None,
            updated: None,
            deleted: Some(TemplateSpec::new("CALL delete_item(@before.id)")),
        }),
        ..Default::default()
    };

    assert!(config.validate().is_ok());
}

#[test]
fn test_config_validation_valid_with_all_templates() {
    let config = PostgresStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_item(@after.id)")),
            updated: Some(TemplateSpec::new("CALL update_item(@after.id)")),
            deleted: Some(TemplateSpec::new("CALL delete_item(@before.id)")),
        }),
        ..Default::default()
    };

    assert!(config.validate().is_ok());
}

#[test]
fn test_config_serialization() {
    let config = PostgresStoredProcReactionConfig {
        hostname: "db.example.com".to_string(),
        port: Some(5433),
        user: "admin".to_string(),
        password: "secret".to_string(),
        database: "mydb".to_string(),
        ssl: true,
        routes: std::collections::HashMap::new(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_user(@after.id, @after.name)")),
            updated: Some(TemplateSpec::new(
                "CALL update_user(@after.id, @after.name)",
            )),
            deleted: Some(TemplateSpec::new("CALL delete_user(@before.id)")),
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
// Unit Tests for Parser Module
// ============================================================================

#[test]
fn test_parser_invalid_procedure_name_with_semicolon() {
    let parser = ParameterParser::new();
    let data = json!({"id": 1});

    // Parser should extract the procedure name but executor will handle injection
    let result = parser.parse_command("CALL test(); DROP TABLE users;--", &data);
    assert!(result.is_ok());
    let (proc_name, _) = result.unwrap();
    assert_eq!(proc_name, "test"); // Stops at first parenthesis
}

#[test]
fn test_parser_parameter_with_underscores() {
    let parser = ParameterParser::new();
    let command = "CALL test(@user_id, @user_name)";
    let data = json!({"user_id": 42, "user_name": "Alice"});

    let (_, params) = parser.parse_command(command, &data).unwrap();
    assert_eq!(params.len(), 2);
    assert_eq!(params[0], json!(42));
    assert_eq!(params[1], json!("Alice"));
}

#[test]
fn test_parser_large_numbers() {
    let parser = ParameterParser::new();
    let command = "CALL test(@big_int, @big_float)";
    let data = json!({
        "big_int": 9223372036854775807i64,
        "big_float": 1.7976931348623157e308
    });

    let (_, params) = parser.parse_command(command, &data).unwrap();
    assert_eq!(params[0], json!(9223372036854775807i64));
    assert_eq!(params[1], json!(1.7976931348623157e308));
}

#[test]
fn test_parser_empty_strings() {
    let parser = ParameterParser::new();
    let command = "CALL test(@name, @description)";
    let data = json!({"name": "", "description": ""});

    let (_, params) = parser.parse_command(command, &data).unwrap();
    assert_eq!(params[0], json!(""));
    assert_eq!(params[1], json!(""));
}

#[test]
fn test_parser_mixed_case_field_names() {
    let parser = ParameterParser::new();
    let command = "CALL test(@userId, @UserName, @USER_EMAIL)";
    let data = json!({
        "userId": 1,
        "UserName": "Alice",
        "USER_EMAIL": "alice@example.com"
    });

    let (_, params) = parser.parse_command(command, &data).unwrap();
    assert_eq!(params.len(), 3);
}

#[test]
fn test_parser_nested_array_field() {
    let parser = ParameterParser::new();
    let command = "CALL test(@items)";
    let data = json!({
        "items": [
            {"id": 1, "name": "Item 1"},
            {"id": 2, "name": "Item 2"}
        ]
    });

    let (_, params) = parser.parse_command(command, &data).unwrap();
    assert_eq!(
        params[0],
        json!([
            {"id": 1, "name": "Item 1"},
            {"id": 2, "name": "Item 2"}
        ])
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
            added: Some(TemplateSpec::new("CALL add_user(@after.id)")),
            updated: None,
            deleted: None,
        }),
        ..Default::default()
    };

    assert!(config.validate().is_ok());

    // Invalid: empty user
    let mut invalid = config.clone();
    invalid.user = String::new();
    assert!(invalid.validate().is_err());

    // Invalid: empty database
    let mut invalid = config.clone();
    invalid.database = String::new();
    assert!(invalid.validate().is_err());

    // Invalid: no commands
    let mut invalid = config.clone();
    invalid.default_template = None;
    assert!(invalid.validate().is_err());
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

    let executor = PostgresExecutor::new(&config).await;
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
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
        ..Default::default()
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Execute the stored procedure
    let params = vec![json!("sensor-001"), json!(25.5)];

    let result = executor.execute_procedure("log_sensor_added", params).await;
    assert!(
        result.is_ok(),
        "Procedure execution should succeed: {:?}",
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
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
            )),
            updated: Some(TemplateSpec::new(
                "CALL log_sensor_updated(@after.sensor_id, @after.temperature)",
            )),
            deleted: Some(TemplateSpec::new(
                "CALL log_sensor_deleted(@before.sensor_id)",
            )),
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
        ..Default::default()
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Execute ADD
    executor
        .execute_procedure("log_sensor_added", vec![json!("sensor-001"), json!(25.5)])
        .await
        .expect("ADD should succeed");

    // Execute UPDATE
    executor
        .execute_procedure("log_sensor_updated", vec![json!("sensor-001"), json!(26.5)])
        .await
        .expect("UPDATE should succeed");

    // Execute DELETE
    executor
        .execute_procedure("log_sensor_deleted", vec![json!("sensor-001")])
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
async fn test_postgres_parser_with_executor() {
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
        routes: HashMap::new(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();
    let parser = ParameterParser::new();

    // Parse command with data
    let data = json!({
        "sensor_id": "sensor-002",
        "temperature": 27.3
    });

    let (proc_name, params) = parser
        .parse_command("CALL log_sensor_added(@sensor_id, @temperature)", &data)
        .expect("Parser should succeed");

    assert_eq!(proc_name, "log_sensor_added");
    assert_eq!(params.len(), 2);

    // Execute the parsed procedure
    let result = executor.execute_procedure(&proc_name, params).await;
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
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
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
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
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
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Test with special characters (potential SQL injection)
    let params = vec![json!("sensor'; DROP TABLE sensor_log; --"), json!(25.5)];

    let result = executor.execute_procedure("log_sensor_added", params).await;
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
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
            )),
            updated: Some(TemplateSpec::new(
                "CALL log_sensor_updated(@after.sensor_id, @after.temperature)",
            )),
            deleted: Some(TemplateSpec::new(
                "CALL log_sensor_deleted(@before.sensor_id)",
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
        Some("PostgreSQL"),
        "Database type should be PostgreSQL"
    );
    assert_eq!(
        properties.get("database_name").and_then(|v| v.as_str()),
        Some(pg_config.database.as_str()),
        "Database name should match"
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

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Try to execute non-existent procedure (should fail after retries)
    let result = executor
        .execute_procedure("non_existent_proc", vec![])
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
    let client = tokio_postgres::connect(
        &format!(
            "host={} port={} user={} password={} dbname={}",
            pg_config.host, pg_config.port, pg_config.user, pg_config.password, pg_config.database
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap()
    .0;

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
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(), // No routes specified
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_query_event(@query_name, @after.sensor_id)",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();
    let parser = ParameterParser::new();

    // Test that default template is used for query1
    let data = json!({
        "query_name": "query1",
        "sensor_id": "sensor-001"
    });

    let (proc_name, params) = parser
        .parse_command("CALL log_query_event(@query_name, @sensor_id)", &data)
        .expect("Should parse");

    executor
        .execute_procedure(&proc_name, params)
        .await
        .expect("Should execute");

    // Test that default template is used for query2
    let data = json!({
        "query_name": "query2",
        "sensor_id": "sensor-002"
    });

    let (proc_name, params) = parser
        .parse_command("CALL log_query_event(@query_name, @sensor_id)", &data)
        .expect("Should parse");

    executor
        .execute_procedure(&proc_name, params)
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
    let client = tokio_postgres::connect(
        &format!(
            "host={} port={} user={} password={} dbname={}",
            pg_config.host, pg_config.port, pg_config.user, pg_config.password, pg_config.database
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap()
    .0;

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
                "CALL log_special_event(@after.sensor_id, @after.temperature)",
            )),
            updated: None,
            deleted: None,
        },
    );

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
            )),
            updated: None,
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();
    let parser = ParameterParser::new();

    // Execute with route-specific template
    let data = json!({
        "sensor_id": "sensor-special",
        "temperature": 99.9
    });

    let (proc_name, params) = parser
        .parse_command("CALL log_special_event(@sensor_id, @temperature)", &data)
        .expect("Should parse");

    executor
        .execute_procedure(&proc_name, params)
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
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
            )),
            updated: None, // Will fall back to default
            deleted: None,
        },
    );

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes,
        default_template: Some(QueryConfig {
            added: None,
            updated: Some(TemplateSpec::new(
                "CALL log_sensor_updated(@after.sensor_id, @after.temperature)",
            )),
            deleted: None,
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    // Get the UPDATE template for "partial-query"
    let template = config.get_command_template("partial-query", common::OperationType::Update);

    // Should fall back to default template
    assert!(template.is_some());
    assert_eq!(
        template.unwrap(),
        "CALL log_sensor_updated(@after.sensor_id, @after.temperature)"
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
    let client = tokio_postgres::connect(
        &format!(
            "host={} port={} user={} password={} dbname={}",
            pg_config.host, pg_config.port, pg_config.user, pg_config.password, pg_config.database
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap()
    .0;

    client
        .execute(
            "CREATE TABLE IF NOT EXISTS data_type_test (
                id SERIAL PRIMARY KEY,
                text_col TEXT,
                int_col INTEGER,
                float_col DOUBLE PRECISION,
                bool_col BOOLEAN,
                json_col JSONB
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
                p_bool BOOLEAN,
                p_json JSONB
            )
            LANGUAGE plpgsql
            AS $$
            BEGIN
                INSERT INTO data_type_test (text_col, int_col, float_col, bool_col, json_col)
                VALUES (p_text, p_int, p_float, p_bool, p_json);
            END;
            $$;",
            &[],
        )
        .await
        .unwrap();

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Test with various data types
    let params = vec![
        json!("test string"),                  // TEXT
        json!(42),                             // INTEGER
        json!(std::f64::consts::PI),           // DOUBLE PRECISION
        json!(true),                           // BOOLEAN
        json!({"key": "value", "count": 123}), // JSONB
    ];

    let result = executor.execute_procedure("test_data_types", params).await;
    assert!(
        result.is_ok(),
        "Should handle various data types: {:?}",
        result.err()
    );

    sleep(Duration::from_millis(100)).await;

    // Verify the data was inserted correctly
    let rows = client
        .query(
            "SELECT text_col, int_col, float_col, bool_col, json_col FROM data_type_test",
            &[],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.get::<_, String>(0), "test string");
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

    let client = tokio_postgres::connect(
        &format!(
            "host={} port={} user={} password={} dbname={}",
            pg_config.host, pg_config.port, pg_config.user, pg_config.password, pg_config.database
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap()
    .0;

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
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Test with string that looks like a number (simulating MockSource behavior)
    let params = vec![json!("25.789")];

    let result = executor
        .execute_procedure("test_string_number", params)
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

#[tokio::test]
#[serial]
async fn test_executor_with_null_values() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let pg = setup_postgres().await;
    let pg_config = pg.config();

    let client = tokio_postgres::connect(
        &format!(
            "host={} port={} user={} password={} dbname={}",
            pg_config.host, pg_config.port, pg_config.user, pg_config.password, pg_config.database
        ),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap()
    .0;

    client
        .execute(
            "CREATE TABLE IF NOT EXISTS null_test (
                id SERIAL PRIMARY KEY,
                nullable_text TEXT,
                nullable_int INTEGER
            )",
            &[],
        )
        .await
        .unwrap();

    client
        .execute(
            "CREATE OR REPLACE PROCEDURE test_null_values(
                p_text TEXT,
                p_int INTEGER
            )
            LANGUAGE plpgsql
            AS $$
            BEGIN
                INSERT INTO null_test (nullable_text, nullable_int) VALUES (p_text, p_int);
            END;
            $$;",
            &[],
        )
        .await
        .unwrap();

    let config = PostgresStoredProcReactionConfig {
        hostname: pg_config.host.clone(),
        port: Some(pg_config.port),
        user: pg_config.user.clone(),
        password: pg_config.password.clone(),
        database: pg_config.database.clone(),
        ssl: false,
        routes: HashMap::new(),
        default_template: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Test with NULL values
    let params = vec![json!(null), json!(null)];

    let result = executor.execute_procedure("test_null_values", params).await;
    assert!(
        result.is_ok(),
        "Should handle NULL values: {:?}",
        result.err()
    );

    sleep(Duration::from_millis(100)).await;

    // Verify NULLs were stored
    let rows = client
        .query("SELECT nullable_text, nullable_int FROM null_test", &[])
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let text_val: Option<String> = rows[0].get(0);
    let int_val: Option<i32> = rows[0].get(1);
    assert!(text_val.is_none());
    assert!(int_val.is_none());

    pg.cleanup().await;
}
