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

//! Template and Route Tests for MySQL Stored Procedure Reaction

mod mysql_helpers;

use drasi_lib::Reaction;
use drasi_reaction_storedproc_mysql::config::{
    MySqlStoredProcReactionConfig, QueryConfig, TemplateSpec,
};
use drasi_reaction_storedproc_mysql::executor::MySqlExecutor;
use drasi_reaction_storedproc_mysql::parser::ParameterParser;
use drasi_reaction_storedproc_mysql::MySqlStoredProcReaction;
use mysql_async::prelude::*;
use mysql_helpers::{setup_mysql, MysqlConfig};
use serde_json::json;
use serial_test::serial;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;

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

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    // Setup additional stored procedure for testing
    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    conn.query_drop("DROP PROCEDURE IF EXISTS log_query_event")
        .await
        .ok();

    conn.query_drop(
        "CREATE PROCEDURE log_query_event(
            IN p_query_name TEXT,
            IN p_sensor_id TEXT
        )
        BEGIN
            INSERT INTO sensor_log (operation, sensor_id)
            VALUES (p_query_name, p_sensor_id);
        END",
    )
    .await
    .expect("Failed to create log_query_event procedure");

    drop(conn);

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
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

    let executor = MySqlExecutor::new(&config).await.unwrap();
    let parser = ParameterParser::new();

    // Test that default template is used for query1
    let data = json!({
        "query_name": "query1",
        "after": {
            "sensor_id": "sensor-001"
        }
    });

    let (proc_name, params) = parser
        .parse_command("CALL log_query_event(@query_name, @after.sensor_id)", &data)
        .expect("Should parse");

    executor
        .execute_procedure(&proc_name, params)
        .await
        .expect("Should execute");

    // Test that default template is used for query2
    let data = json!({
        "query_name": "query2",
        "after": {
            "sensor_id": "sensor-002"
        }
    });

    let (proc_name, params) = parser
        .parse_command("CALL log_query_event(@query_name, @after.sensor_id)", &data)
        .expect("Should parse");

    executor
        .execute_procedure(&proc_name, params)
        .await
        .expect("Should execute");

    sleep(Duration::from_millis(100)).await;

    // Verify both queries used the default template
    let entries = get_log_entries(mysql_config).await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].1, "sensor-001");
    assert_eq!(entries[1].1, "sensor-002");

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_route_overrides_default_template() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    // Setup additional stored procedure
    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    conn.query_drop("DROP PROCEDURE IF EXISTS log_special_event")
        .await
        .ok();

    conn.query_drop(
        "CREATE PROCEDURE log_special_event(
            IN p_sensor_id TEXT,
            IN p_temperature DOUBLE
        )
        BEGIN
            INSERT INTO sensor_log (operation, sensor_id, temperature)
            VALUES ('SPECIAL', p_sensor_id, p_temperature);
        END",
    )
    .await
    .expect("Failed to create log_special_event procedure");

    drop(conn);

    let mut routes = HashMap::new();
    routes.insert(
        "special-query".to_string(),
        QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_special_event(@after.sensor_id, @after.temperature)",
            )),
            ..Default::default()
        },
    );

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
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

    let executor = MySqlExecutor::new(&config).await.unwrap();
    let parser = ParameterParser::new();

    // Execute with route-specific template
    let data = json!({
        "after": {
            "sensor_id": "sensor-special",
            "temperature": 99.9
        }
    });

    let (proc_name, params) = parser
        .parse_command(
            "CALL log_special_event(@after.sensor_id, @after.temperature)",
            &data,
        )
        .expect("Should parse");

    executor
        .execute_procedure(&proc_name, params)
        .await
        .expect("Should execute");

    sleep(Duration::from_millis(100)).await;

    // Verify it used the special route (SPECIAL operation vs ADD)
    let entries = get_log_entries(mysql_config).await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "SPECIAL");
    assert_eq!(entries[0].1, "sensor-special");

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_route_with_none_falls_back_to_default() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    let mut routes = HashMap::new();
    routes.insert(
        "partial-query".to_string(),
        QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
            )),
            updated: None, // Will fall back to default
            ..Default::default()
        },
    );

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
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
    let template = config.get_command_template(
        "partial-query",
        drasi_lib::reactions::common::OperationType::Update,
    );

    // Should fall back to default template
    assert!(template.is_some());
    assert_eq!(
        template.unwrap(),
        "CALL log_sensor_updated(@after.sensor_id, @after.temperature)"
    );

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_multiple_queries_with_different_routes() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    // Setup additional stored procedure
    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    conn.query_drop("DROP PROCEDURE IF EXISTS log_critical_sensor")
        .await
        .ok();

    conn.query_drop(
        "CREATE PROCEDURE log_critical_sensor(
            IN p_sensor_id TEXT,
            IN p_temperature DOUBLE
        )
        BEGIN
            INSERT INTO sensor_log (operation, sensor_id, temperature)
            VALUES ('CRITICAL', p_sensor_id, p_temperature);
        END",
    )
    .await
    .expect("Failed to create log_critical_sensor procedure");

    drop(conn);

    let mut routes = HashMap::new();

    // Route for critical temperature query
    routes.insert(
        "critical-temp".to_string(),
        QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_critical_sensor(@after.sensor_id, @after.temperature)",
            )),
            ..Default::default()
        },
    );

    // Route for normal temperature query uses default
    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
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

    let executor = MySqlExecutor::new(&config).await.unwrap();
    let parser = ParameterParser::new();

    // Execute normal query (uses default)
    let data = json!({
        "after": {
            "sensor_id": "sensor-normal",
            "temperature": 25.5
        }
    });

    let (proc_name, params) = parser
        .parse_command(
            "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
            &data,
        )
        .expect("Should parse");

    executor
        .execute_procedure(&proc_name, params)
        .await
        .expect("Should execute");

    // Execute critical query (uses route)
    let data = json!({
        "after": {
            "sensor_id": "sensor-critical",
            "temperature": 99.9
        }
    });

    let (proc_name, params) = parser
        .parse_command(
            "CALL log_critical_sensor(@after.sensor_id, @after.temperature)",
            &data,
        )
        .expect("Should parse");

    executor
        .execute_procedure(&proc_name, params)
        .await
        .expect("Should execute");

    sleep(Duration::from_millis(100)).await;

    // Verify both queries executed with different procedures
    let entries = get_log_entries(mysql_config).await;
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "ADD");
    assert_eq!(entries[0].1, "sensor-normal");
    assert_eq!(entries[1].0, "CRITICAL");
    assert_eq!(entries[1].1, "sensor-critical");

    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_builder_with_routes() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();
    setup_stored_procedures(mysql_config).await;

    let reaction = MySqlStoredProcReaction::builder("test-routes")
        .with_hostname(&mysql_config.host)
        .with_port(mysql_config.port)
        .with_database(&mysql_config.database)
        .with_user(&mysql_config.user)
        .with_password(&mysql_config.password)
        .with_ssl(false)
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL log_sensor_added(@after.sensor_id, @after.temperature)",
            )),
            ..Default::default()
        })
        .with_route(
            "special-query",
            QueryConfig {
                added: Some(TemplateSpec::new(
                    "CALL log_sensor_updated(@after.sensor_id, @after.temperature)",
                )),
                ..Default::default()
            },
        )
        .with_query("normal-query")
        .with_query("special-query")
        .with_command_timeout_ms(5000)
        .with_retry_attempts(3)
        .build()
        .await;

    assert!(
        reaction.is_ok(),
        "Builder should create reaction with routes"
    );

    let reaction = reaction.unwrap();
    assert_eq!(reaction.id(), "test-routes");
    assert_eq!(reaction.query_ids(), vec!["normal-query", "special-query"]);

    mysql.cleanup().await;
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

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();

    // Create a comprehensive test table
    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS data_type_test (
            id INT AUTO_INCREMENT PRIMARY KEY,
            text_col TEXT,
            int_col INT,
            float_col DOUBLE,
            bool_col BOOLEAN
        )",
    )
    .await
    .expect("Failed to create data_type_test table");

    conn.query_drop("DROP PROCEDURE IF EXISTS test_data_types")
        .await
        .ok();

    conn.query_drop(
        "CREATE PROCEDURE test_data_types(
            IN p_text TEXT,
            IN p_int INT,
            IN p_float DOUBLE,
            IN p_bool BOOLEAN
        )
        BEGIN
            INSERT INTO data_type_test (text_col, int_col, float_col, bool_col)
            VALUES (p_text, p_int, p_float, p_bool);
        END",
    )
    .await
    .expect("Failed to create test_data_types procedure");

    drop(conn);

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL test_data_types(@after.text_val, @after.int_val, @after.float_val, @after.bool_val)")),
            ..Default::default()
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
        ..Default::default()
    };

    let executor = MySqlExecutor::new(&config).await.unwrap();
    let parser = ParameterParser::new();

    // Test with various data types using ParameterParser
    let data = json!({
        "after": {
            "text_val": "test-string",
            "int_val": 42,
            "float_val": std::f64::consts::PI,
            "bool_val": true
        }
    });

    let (proc_name, params) = parser
        .parse_command(
            "CALL test_data_types(@after.text_val, @after.int_val, @after.float_val, @after.bool_val)",
            &data,
        )
        .expect("Should parse command");

    assert_eq!(proc_name, "test_data_types");

    let result = executor.execute_procedure(&proc_name, params).await;
    assert!(
        result.is_ok(),
        "Should handle various data types: {:?}",
        result.err()
    );

    sleep(Duration::from_millis(100)).await;

    // Verify the data was inserted correctly
    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    let rows: Vec<(String, i32, f64, bool)> = conn
        .query("SELECT text_col, int_col, float_col, bool_col FROM data_type_test")
        .await
        .expect("Failed to query data_type_test");

    assert_eq!(rows.len(), 1);
    let (text_col, int_col, float_col, bool_col) = &rows[0];
    assert_eq!(text_col, "test-string");
    assert_eq!(*int_col, 42);
    assert!((float_col - std::f64::consts::PI).abs() < 0.00001);
    assert!(*bool_col);

    drop(conn);
    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_executor_with_string_numbers() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();

    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS string_number_test (
            id INT AUTO_INCREMENT PRIMARY KEY,
            numeric_value DOUBLE
        )",
    )
    .await
    .expect("Failed to create string_number_test table");

    conn.query_drop("DROP PROCEDURE IF EXISTS test_string_number")
        .await
        .ok();

    conn.query_drop(
        "CREATE PROCEDURE test_string_number(
            IN p_value DOUBLE
        )
        BEGIN
            INSERT INTO string_number_test (numeric_value) VALUES (p_value);
        END",
    )
    .await
    .expect("Failed to create test_string_number procedure");

    drop(conn);

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL test_string_number(@after.value)")),
            ..Default::default()
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
        ..Default::default()
    };

    let executor = MySqlExecutor::new(&config).await.unwrap();

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
    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    let rows: Vec<f64> = conn
        .query("SELECT numeric_value FROM string_number_test")
        .await
        .expect("Failed to query string_number_test");

    assert_eq!(rows.len(), 1);
    let value = rows[0];
    assert!((value - 25.789).abs() < 0.00001);

    drop(conn);
    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_parser_and_executor_with_nested_parameters() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();

    // Create a comprehensive test table for nested parameter testing
    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS nested_param_test (
            id INT AUTO_INCREMENT PRIMARY KEY,
            operation VARCHAR(20),
            sensor_id TEXT,
            old_temp DOUBLE,
            new_temp DOUBLE,
            metadata TEXT,
            logged_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .await
    .expect("Failed to create nested_param_test table");

    conn.query_drop("DROP PROCEDURE IF EXISTS test_nested_add")
        .await
        .ok();

    conn.query_drop("DROP PROCEDURE IF EXISTS test_nested_update")
        .await
        .ok();

    conn.query_drop("DROP PROCEDURE IF EXISTS test_nested_delete")
        .await
        .ok();

    // Procedure for ADD with nested @after parameters
    conn.query_drop(
        "CREATE PROCEDURE test_nested_add(
            IN p_sensor_id TEXT,
            IN p_temperature DOUBLE,
            IN p_metadata TEXT
        )
        BEGIN
            INSERT INTO nested_param_test (operation, sensor_id, new_temp, metadata)
            VALUES ('ADD', p_sensor_id, p_temperature, p_metadata);
        END",
    )
    .await
    .expect("Failed to create test_nested_add procedure");

    // Procedure for UPDATE with both @before and @after parameters
    conn.query_drop(
        "CREATE PROCEDURE test_nested_update(
            IN p_sensor_id TEXT,
            IN p_old_temp DOUBLE,
            IN p_new_temp DOUBLE,
            IN p_metadata TEXT
        )
        BEGIN
            INSERT INTO nested_param_test (operation, sensor_id, old_temp, new_temp, metadata)
            VALUES ('UPDATE', p_sensor_id, p_old_temp, p_new_temp, p_metadata);
        END",
    )
    .await
    .expect("Failed to create test_nested_update procedure");

    // Procedure for DELETE with nested @before parameters
    conn.query_drop(
        "CREATE PROCEDURE test_nested_delete(
            IN p_sensor_id TEXT,
            IN p_temperature DOUBLE
        )
        BEGIN
            INSERT INTO nested_param_test (operation, sensor_id, old_temp)
            VALUES ('DELETE', p_sensor_id, p_temperature);
        END",
    )
    .await
    .expect("Failed to create test_nested_delete procedure");

    drop(conn);

    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new(
                "CALL test_nested_add(@after.sensor_id, @after.temperature, @after.metadata)",
            )),
            updated: Some(TemplateSpec::new(
                "CALL test_nested_update(@after.sensor_id, @before.temperature, @after.temperature, @after.metadata)",
            )),
            deleted: Some(TemplateSpec::new(
                "CALL test_nested_delete(@before.sensor_id, @before.temperature)",
            )),
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
        ..Default::default()
    };

    let executor = MySqlExecutor::new(&config).await.unwrap();
    let parser = ParameterParser::new();

    // Test ADD operation with @after.field syntax
    let add_data = json!({
        "after": {
            "sensor_id": "sensor-123",
            "temperature": 25.5,
            "metadata": "Room A"
        }
    });

    let (proc_name, params) = parser
        .parse_command(
            "CALL test_nested_add(@after.sensor_id, @after.temperature, @after.metadata)",
            &add_data,
        )
        .expect("Should parse ADD command");

    assert_eq!(proc_name, "test_nested_add");
    assert_eq!(params.len(), 3);
    assert_eq!(params[0], json!("sensor-123"));
    assert_eq!(params[1], json!(25.5));
    assert_eq!(params[2], json!("Room A"));

    executor
        .execute_procedure(&proc_name, params)
        .await
        .expect("Should execute ADD");

    // Test UPDATE operation with both @before and @after fields
    let update_data = json!({
        "before": {
            "sensor_id": "sensor-123",
            "temperature": 25.5,
            "metadata": "Room A"
        },
        "after": {
            "sensor_id": "sensor-123",
            "temperature": 27.8,
            "metadata": "Room A - Updated"
        }
    });

    let (proc_name, params) = parser
        .parse_command(
            "CALL test_nested_update(@after.sensor_id, @before.temperature, @after.temperature, @after.metadata)",
            &update_data,
        )
        .expect("Should parse UPDATE command");

    assert_eq!(proc_name, "test_nested_update");
    assert_eq!(params.len(), 4);
    assert_eq!(params[0], json!("sensor-123"));
    assert_eq!(params[1], json!(25.5)); // old temperature
    assert_eq!(params[2], json!(27.8)); // new temperature
    assert_eq!(params[3], json!("Room A - Updated"));

    executor
        .execute_procedure(&proc_name, params)
        .await
        .expect("Should execute UPDATE");

    // Test DELETE operation with @before.field syntax
    let delete_data = json!({
        "before": {
            "sensor_id": "sensor-123",
            "temperature": 27.8,
            "metadata": "Room A - Updated"
        }
    });

    let (proc_name, params) = parser
        .parse_command(
            "CALL test_nested_delete(@before.sensor_id, @before.temperature)",
            &delete_data,
        )
        .expect("Should parse DELETE command");

    assert_eq!(proc_name, "test_nested_delete");
    assert_eq!(params.len(), 2);
    assert_eq!(params[0], json!("sensor-123"));
    assert_eq!(params[1], json!(27.8));

    executor
        .execute_procedure(&proc_name, params)
        .await
        .expect("Should execute DELETE");

    sleep(Duration::from_millis(100)).await;

    // Verify all operations were logged correctly
    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    let rows: Vec<(String, String, Option<f64>, Option<f64>, Option<String>)> = conn
        .query("SELECT operation, sensor_id, old_temp, new_temp, metadata FROM nested_param_test ORDER BY id")
        .await
        .expect("Failed to query nested_param_test");

    assert_eq!(rows.len(), 3);

    // Verify ADD operation
    assert_eq!(rows[0].0, "ADD");
    assert_eq!(rows[0].1, "sensor-123");
    assert_eq!(rows[0].2, None); // old_temp should be NULL for ADD
    assert_eq!(rows[0].3, Some(25.5));
    assert_eq!(rows[0].4, Some("Room A".to_string()));

    // Verify UPDATE operation
    assert_eq!(rows[1].0, "UPDATE");
    assert_eq!(rows[1].1, "sensor-123");
    assert_eq!(rows[1].2, Some(25.5)); // old temperature
    assert_eq!(rows[1].3, Some(27.8)); // new temperature
    assert_eq!(rows[1].4, Some("Room A - Updated".to_string()));

    // Verify DELETE operation
    assert_eq!(rows[2].0, "DELETE");
    assert_eq!(rows[2].1, "sensor-123");
    assert_eq!(rows[2].2, Some(27.8)); // old_temp
    assert_eq!(rows[2].3, None); // new_temp should be NULL for DELETE

    drop(conn);
    mysql.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_parser_with_deeply_nested_fields() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let mysql = setup_mysql().await;
    let mysql_config = mysql.config();

    // Create test table and procedure
    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS deep_nested_test (
            id INT AUTO_INCREMENT PRIMARY KEY,
            device_name TEXT,
            location_building TEXT,
            location_floor INT,
            reading_value DOUBLE
        )",
    )
    .await
    .expect("Failed to create deep_nested_test table");

    conn.query_drop("DROP PROCEDURE IF EXISTS test_deep_nested")
        .await
        .ok();

    conn.query_drop(
        "CREATE PROCEDURE test_deep_nested(
            IN p_device_name TEXT,
            IN p_building TEXT,
            IN p_floor INT,
            IN p_value DOUBLE
        )
        BEGIN
            INSERT INTO deep_nested_test (device_name, location_building, location_floor, reading_value)
            VALUES (p_device_name, p_building, p_floor, p_value);
        END",
    )
    .await
    .expect("Failed to create test_deep_nested procedure");

    drop(conn);

    let parser = ParameterParser::new();

    // Test with deeply nested JSON structure
    let data = json!({
        "after": {
            "device": {
                "name": "TempSensor-001"
            },
            "location": {
                "building": "Building A",
                "floor": 3
            },
            "reading": {
                "value": 23.4
            }
        }
    });

    let (proc_name, params) = parser
        .parse_command(
            "CALL test_deep_nested(@after.device.name, @after.location.building, @after.location.floor, @after.reading.value)",
            &data,
        )
        .expect("Should parse deeply nested fields");

    assert_eq!(proc_name, "test_deep_nested");
    assert_eq!(params.len(), 4);
    assert_eq!(params[0], json!("TempSensor-001"));
    assert_eq!(params[1], json!("Building A"));
    assert_eq!(params[2], json!(3));
    assert_eq!(params[3], json!(23.4));

    // Execute with the parsed parameters
    let config = MySqlStoredProcReactionConfig {
        hostname: mysql_config.host.clone(),
        port: Some(mysql_config.port),
        user: mysql_config.user.clone(),
        password: mysql_config.password.clone(),
        database: mysql_config.database.clone(),
        ssl: false,
        default_template: Some(QueryConfig {
            added: Some(TemplateSpec::new("CALL test_deep_nested(@after.device.name, @after.location.building, @after.location.floor, @after.reading.value)")),
            ..Default::default()
        }),
        command_timeout_ms: 5000,
        retry_attempts: 3,
        ..Default::default()
    };

    let executor = MySqlExecutor::new(&config).await.unwrap();

    executor
        .execute_procedure(&proc_name, params)
        .await
        .expect("Should execute with deeply nested params");

    sleep(Duration::from_millis(100)).await;

    // Verify the data was inserted correctly
    let opts = mysql_config.connection_opts();
    let mut conn = mysql_async::Conn::new(opts)
        .await
        .expect("Failed to connect");

    let rows: Vec<(String, String, i32, f64)> = conn
        .query("SELECT device_name, location_building, location_floor, reading_value FROM deep_nested_test")
        .await
        .expect("Failed to query deep_nested_test");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "TempSensor-001");
    assert_eq!(rows[0].1, "Building A");
    assert_eq!(rows[0].2, 3);
    assert_eq!(rows[0].3, 23.4);

    drop(conn);
    mysql.cleanup().await;
}
