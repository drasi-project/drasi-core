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

//! Tests for the MySQL Stored Procedure Reaction
//!
//! These tests validate the end-to-end behavior of the MySQL-specific
//! stored procedure reaction using testcontainers to provide a real MySQL database.
use crate::mysql_helpers::{setup_mysql, MysqlConfig};
use drasi_lib::plugin_core::Reaction;
use drasi_reaction_storedproc_mysql::config::MySqlStoredProcReactionConfig;
use drasi_reaction_storedproc_mysql::executor::MySqlExecutor;
use drasi_reaction_storedproc_mysql::parser::ParameterParser;
use drasi_reaction_storedproc_mysql::MySqlStoredProcReaction;
use mysql_async::prelude::*;
use serde_json::json;
use serial_test::serial;
use std::time::Duration;
use tokio::time::sleep;

/// Helper function to setup stored procedures in the test database
async fn setup_stored_procedures(config: &MysqlConfig) {
    let opts = config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect to database");

    // Create a table to log stored procedure calls
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS sensor_log (
            id INT AUTO_INCREMENT PRIMARY KEY,
            operation VARCHAR(10) NOT NULL,
            sensor_id TEXT NOT NULL,
            temperature DOUBLE,
            logged_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .await
    .expect("Failed to create sensor_log table");

    // Drop existing procedures if they exist
    let _ = conn
        .query_drop("DROP PROCEDURE IF EXISTS log_sensor_added")
        .await;
    let _ = conn
        .query_drop("DROP PROCEDURE IF EXISTS log_sensor_updated")
        .await;
    let _ = conn
        .query_drop("DROP PROCEDURE IF EXISTS log_sensor_deleted")
        .await;

    // Create stored procedure for ADD operation
    conn.query_drop(
        "CREATE PROCEDURE log_sensor_added(
            IN p_sensor_id TEXT,
            IN p_temperature DOUBLE
        )
        BEGIN
            INSERT INTO sensor_log (operation, sensor_id, temperature)
            VALUES ('ADD', p_sensor_id, p_temperature);
        END",
    )
    .await
    .expect("Failed to create log_sensor_added procedure");

    // Create stored procedure for UPDATE operation
    conn.query_drop(
        "CREATE PROCEDURE log_sensor_updated(
            IN p_sensor_id TEXT,
            IN p_temperature DOUBLE
        )
        BEGIN
            INSERT INTO sensor_log (operation, sensor_id, temperature)
            VALUES ('UPDATE', p_sensor_id, p_temperature);
        END",
    )
    .await
    .expect("Failed to create log_sensor_updated procedure");

    // Create stored procedure for DELETE operation
    conn.query_drop(
        "CREATE PROCEDURE log_sensor_deleted(
            IN p_sensor_id TEXT
        )
        BEGIN
            INSERT INTO sensor_log (operation, sensor_id)
            VALUES ('DELETE', p_sensor_id);
        END",
    )
    .await
    .expect("Failed to create log_sensor_deleted procedure");

    // Close connection
    drop(conn);
}

/// Helper function to get log entries from the database
async fn get_log_entries(config: &MysqlConfig) -> Vec<(String, String)> {
    let opts = config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect to database");

    let rows: Vec<(String, String)> = conn
        .query("SELECT operation, sensor_id FROM sensor_log ORDER BY logged_at")
        .await
        .expect("Failed to query sensor_log");

    drop(conn);
    rows
}

/// Helper function to get log count
async fn get_log_count(config: &MysqlConfig) -> i64 {
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

// ============================================================================
// Unit Tests for Config Module
// ============================================================================

#[test]
fn test_config_default_values() {
    let config = MySqlStoredProcReactionConfig::default();

    // DevSkim: ignore DS137138
    assert_eq!(config.hostname, "localhost");
    assert_eq!(config.port, None);
    assert_eq!(config.get_port(), 3306); // Default MySQL port
    assert_eq!(config.user, "");
    assert_eq!(config.password, "");
    assert_eq!(config.database, "");
    assert!(!config.ssl);
    assert_eq!(config.added_command, None);
    assert_eq!(config.updated_command, None);
    assert_eq!(config.deleted_command, None);
    assert_eq!(config.command_timeout_ms, 30000);
    assert_eq!(config.retry_attempts, 3);
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
    let config = MySqlStoredProcReactionConfig {
        user: "".to_string(),
        database: "testdb".to_string(),
        added_command: Some("CALL test()".to_string()),
        ..Default::default()
    };

    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("user is required"));
}

#[test]
fn test_config_validation_empty_database() {
    let config = MySqlStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "".to_string(),
        added_command: Some("CALL test()".to_string()),
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
fn test_config_validation_valid_with_added_command() {
    let config = MySqlStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        added_command: Some("CALL add_item(@id)".to_string()),
        ..Default::default()
    };

    assert!(config.validate().is_ok());
}

#[test]
fn test_config_validation_valid_with_updated_command() {
    let config = MySqlStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        updated_command: Some("CALL update_item(@id)".to_string()),
        ..Default::default()
    };

    assert!(config.validate().is_ok());
}

#[test]
fn test_config_validation_valid_with_deleted_command() {
    let config = MySqlStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        deleted_command: Some("CALL delete_item(@id)".to_string()),
        ..Default::default()
    };

    assert!(config.validate().is_ok());
}

#[test]
fn test_config_validation_valid_with_all_commands() {
    let config = MySqlStoredProcReactionConfig {
        user: "testuser".to_string(),
        database: "testdb".to_string(),
        added_command: Some("CALL add_item(@id)".to_string()),
        updated_command: Some("CALL update_item(@id)".to_string()),
        deleted_command: Some("CALL delete_item(@id)".to_string()),
        ..Default::default()
    };

    assert!(config.validate().is_ok());
}

#[test]
fn test_config_serialization() {
    let config = MySqlStoredProcReactionConfig {
        hostname: "db.example.com".to_string(),
        port: Some(3307),
        user: "admin".to_string(),
        password: "secret".to_string(),
        database: "mydb".to_string(),
        ssl: true,
        added_command: Some("CALL add_user(@id, @name)".to_string()),
        updated_command: Some("CALL update_user(@id, @name)".to_string()),
        deleted_command: Some("CALL delete_user(@id)".to_string()),
        command_timeout_ms: 10000,
        retry_attempts: 5,
    };

    let json = serde_json::to_string(&config).unwrap();
    let deserialized: MySqlStoredProcReactionConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.hostname, "db.example.com");
    assert_eq!(deserialized.port, Some(3307));
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
        "added_command": "CALL test()"
    }"#;

    let config: MySqlStoredProcReactionConfig = serde_json::from_str(json).unwrap();

    // DevSkim: ignore DS137138
    assert_eq!(config.hostname, "localhost"); // default
    assert_eq!(config.port, None);
    assert_eq!(config.get_port(), 3306); // default port
    assert!(!config.ssl); // default
    assert_eq!(config.command_timeout_ms, 30000); // default
    assert_eq!(config.retry_attempts, 3); // default
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
// Integration Tests with MySQL
// ============================================================================

#[tokio::test]
async fn test_mysql_config_validation() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    // Valid config
    // DevSkim: ignore DS137138
    let config = MySqlStoredProcReactionConfig {
        hostname: "localhost".to_string(),
        port: Some(3306),
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
async fn test_mysql_executor_connection() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        added_command: Some("CALL test()".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = MySqlExecutor::new(&config).await;
    assert!(executor.is_ok(), "Should create executor successfully");

    let executor = executor.unwrap();
    let result = executor.test_connection().await;
    assert!(result.is_ok(), "Connection test should pass");

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_mysql_executor_procedure_execution() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = MySqlExecutor::new(&config).await.unwrap();

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
    let count = get_log_count(mysql_config).await;
    assert_eq!(count, 1, "Should have 1 log entry");

    let entries = get_log_entries(mysql_config).await;
    assert_eq!(entries[0].0, "ADD");
    assert_eq!(entries[0].1, "sensor-001");

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_mysql_executor_multiple_operations() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: Some("CALL log_sensor_updated(@sensor_id, @temperature)".to_string()),
        deleted_command: Some("CALL log_sensor_deleted(@sensor_id)".to_string()),
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = MySqlExecutor::new(&config).await.unwrap();

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
    let entries = get_log_entries(mysql_config).await;
    assert_eq!(entries.len(), 3, "Should have 3 log entries");
    assert_eq!(entries[0].0, "ADD");
    assert_eq!(entries[1].0, "UPDATE");
    assert_eq!(entries[2].0, "DELETE");

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_mysql_parser_with_executor() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = MySqlExecutor::new(&config).await.unwrap();
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

    let entries = get_log_entries(mysql_config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, "sensor-002");

    mysql.cleanup().await;
}

#[tokio::test]
async fn test_mysql_reaction_creation() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let reaction =
        MySqlStoredProcReaction::new("test-reaction", vec!["test-query".to_string()], config).await;

    assert!(reaction.is_ok(), "Should create reaction successfully");

    let reaction = reaction.unwrap();
    assert_eq!(reaction.id(), "test-reaction");
    assert_eq!(reaction.type_name(), "storedproc-mysql");
    assert_eq!(reaction.query_ids(), vec!["test-query"]);

    mysql.cleanup().await;
}

#[tokio::test]
async fn test_mysql_reaction_builder() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    let reaction = MySqlStoredProcReaction::builder("test-builder")
        .with_hostname(&mysql_config.host)
        .with_port(mysql_config.port)
        .with_database(&mysql_config.database)
        .with_user(&mysql_config.user)
        .with_password(&mysql_config.password)
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

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_mysql_executor_with_special_characters() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        added_command: Some("CALL log_sensor_added(@sensor_id, @temperature)".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 5000,
        retry_attempts: 3,
    };

    let executor = MySqlExecutor::new(&config).await.unwrap();

    // Test with special characters (potential SQL injection)
    let params = vec![json!("sensor'; DROP TABLE sensor_log; --"), json!(25.5)];

    let result = executor.execute_procedure("log_sensor_added", params).await;
    assert!(
        result.is_ok(),
        "Should safely handle special characters via parameterization"
    );

    sleep(Duration::from_millis(100)).await;

    // Verify the table still exists and has the entry
    let entries = get_log_entries(mysql_config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, "sensor'; DROP TABLE sensor_log; --");

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_mysql_reaction_lifecycle() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    // Create the MySQL stored procedure reaction
    let reaction = MySqlStoredProcReaction::builder("mysql-reaction")
        .with_hostname(&mysql_config.host)
        .with_port(mysql_config.port)
        .with_database(&mysql_config.database)
        .with_user(&mysql_config.user)
        .with_password(&mysql_config.password)
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
    assert_eq!(reaction.id(), "mysql-reaction");
    assert_eq!(reaction.type_name(), "storedproc-mysql");
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
        Some(mysql_config.host.as_str()),
        "Hostname should match"
    );
    assert_eq!(
        properties.get("database").and_then(|v| v.as_str()),
        Some("MySQL"),
        "Database type should be MySQL"
    );
    assert_eq!(
        properties.get("database_name").and_then(|v| v.as_str()),
        Some(mysql_config.database.as_str()),
        "Database name should match"
    );
    assert_eq!(
        properties.get("ssl").and_then(|v| v.as_bool()),
        Some(false),
        "SSL should be false"
    );

    mysql.cleanup().await;
}

#[tokio::test]
async fn test_mysql_executor_retry_on_failure() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        added_command: Some("CALL non_existent()".to_string()),
        updated_command: None,
        deleted_command: None,
        command_timeout_ms: 1000,
        retry_attempts: 2,
    };

    let executor = MySqlExecutor::new(&config).await.unwrap();

    // Try to execute non-existent procedure (should fail after retries)
    let result = executor
        .execute_procedure("non_existent_proc", vec![])
        .await;

    assert!(
        result.is_err(),
        "Should fail after exhausting retry attempts"
    );

    mysql.cleanup().await;
}
