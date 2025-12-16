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

//! Integration tests for the PostgreSQL Stored Procedure Reaction
//!
//! These tests validate the end-to-end behavior of the PostgreSQL-specific
//! stored procedure reaction using testcontainers to provide a real PostgreSQL database.

use drasi_lib::plugin_core::Reaction;
use drasi_reaction_storedproc_postgres::config::PostgresStoredProcReactionConfig;
use drasi_reaction_storedproc_postgres::executor::PostgresExecutor;
use drasi_reaction_storedproc_postgres::parser::ParameterParser;
use drasi_reaction_storedproc_postgres::PostgresStoredProcReaction;
use serde_json::json;
use serial_test::serial;
use shared_tests::postgres_helpers::{setup_postgres, PostgresConfig};
use std::time::Duration;
use tokio::time::sleep;

/// Helper function to setup stored procedures in the test database
async fn setup_stored_procedures(config: &PostgresConfig) {
    let (client, connection) = tokio_postgres::connect(&config.connection_string(), tokio_postgres::NoTls)
        .await
        .expect("Failed to connect to database");

    // Spawn the connection to run in the background
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
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
    let (client, connection) = tokio_postgres::connect(&config.connection_string(), tokio_postgres::NoTls)
        .await
        .expect("Failed to connect to database");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    let rows = client
        .query(
            "SELECT operation, sensor_id FROM sensor_log ORDER BY logged_at",
            &[],
        )
        .await
        .expect("Failed to query sensor_log");

    rows.iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

/// Helper function to get log count
async fn get_log_count(config: &PostgresConfig) -> i64 {
    let (client, connection) = tokio_postgres::connect(&config.connection_string(), tokio_postgres::NoTls)
        .await
        .expect("Failed to connect to database");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    let row = client
        .query_one("SELECT COUNT(*) FROM sensor_log", &[])
        .await
        .expect("Failed to query log count");

    row.get(0)
}

#[tokio::test]
async fn test_postgres_config_validation() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    // Valid config
    let config = PostgresStoredProcReactionConfig {
        hostname: "localhost".to_string(),
        port: Some(5432),
        user: "testuser".to_string(),
        password: "password".to_string(),
        database: "testdb".to_string(),
        ssl: false,
        added_command: Some("CALL add_user(@id)".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 30000,
        retry_attempts: 3,
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
    invalid.added_command = None;
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
        added_command: Some("CALL test()".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
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
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Execute the stored procedure
    let params = vec![
        json!("sensor-001"),
        json!(25.5),
    ];

    let result = executor.execute_procedure("log_sensor_added", params).await;
    assert!(result.is_ok(), "Procedure execution should succeed: {:?}", result.err());

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
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: Some("CALL log_sensor_updated(@sensor_id, @temperature)".to_string()),
        deleted_command: Some("CALL log_sensor_deleted(@sensor_id)".to_string()),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Execute ADD
    executor
        .execute_procedure(
            "log_sensor_added",
            vec![
                json!("sensor-001"),
                json!(25.5),
            ],
        )
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
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: None,
        deleted_command: None,
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
        .parse_command(
            "CALL log_sensor_added(@sensor_id, @temperature)",
            &data,
        )
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
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let reaction = PostgresStoredProcReaction::new(
        "test-reaction",
        vec!["test-query".to_string()],
        config,
    )
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
        .with_added_command("CALL log_sensor_added(@sensor_id, @temperature)")
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
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Test with special characters (potential SQL injection)
    let params = vec![
        json!("sensor'; DROP TABLE sensor_log; --"),
        json!(25.5),
    ];

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
async fn test_postgres_executor_with_unicode() {
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
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = PostgresExecutor::new(&config).await.unwrap();

    // Test with Unicode characters
    let params = vec![
        json!("传感器-001"),
        json!(25.5),
    ];

    let result = executor.execute_procedure("log_sensor_added", params).await;
    assert!(result.is_ok(), "Should handle Unicode characters");

    sleep(Duration::from_millis(100)).await;

    let entries = get_log_entries(pg_config).await;
    assert_eq!(entries[0].1, "传感器-001");

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
        .with_added_command("CALL log_sensor_added(@sensor_id, @temperature)")
        .with_updated_command("CALL log_sensor_updated(@sensor_id, @temperature)")
        .with_deleted_command("CALL log_sensor_deleted(@sensor_id)")
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
        added_command: Some("CALL non_existent()".to_string()),
        updated_command: None,
        deleted_command: None,
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
