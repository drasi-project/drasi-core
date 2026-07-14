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

//! Tests for the MSSQL Stored Procedure Reaction
//!
//! These tests validate the end-to-end behavior of the MSSQL-specific
//! stored procedure reaction using testcontainers to provide a real MSSQL
//! database. Each integration test builds the reaction, starts it, enqueues
//! query results, and asserts the procedures ran against the real backend.
//!
//! **Note**: These tests are skipped on ARM64 (Apple Silicon) platforms due to
//! known stability issues with Azure SQL Edge containers crashing on startup.
//! The tests will run successfully on AMD64 platforms and in CI environments.

mod mssql_helpers;

use std::collections::HashMap;
use std::time::Duration;

use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::reactions::Reaction;
use drasi_reaction_storedproc_mssql::{
    MsSqlStoredProcReaction, MsSqlStoredProcReactionConfig, QueryConfig, TemplateSpec,
};
use mssql_helpers::{execute_sql, setup_mssql, MssqlConfig};
use serde_json::json;
use serial_test::serial;
use tokio::time::sleep;

// ============================================================================
// Test Helper Functions
// ============================================================================

/// Helper function to setup stored procedures in the test database
async fn setup_stored_procedures(config: &MssqlConfig) {
    let mut client = config
        .connect()
        .await
        .expect("Failed to connect to database");

    // Create a table to log stored procedure calls
    execute_sql(
        &mut client,
        "CREATE TABLE sensor_log (
            id INT IDENTITY(1,1) PRIMARY KEY,
            operation NVARCHAR(10) NOT NULL,
            sensor_id NVARCHAR(MAX) NOT NULL,
            temperature FLOAT,
            logged_at DATETIME2 DEFAULT GETDATE()
        )",
    )
    .await
    .expect("Failed to create sensor_log table");

    // Drop and create stored procedure for ADD operation
    // Use separate connections to ensure batch separation
    drop(client);

    let mut client = config
        .connect()
        .await
        .expect("Failed to connect to database");
    if let Ok(stream) = client
        .simple_query(
            "IF OBJECT_ID('log_sensor_added', 'P') IS NOT NULL DROP PROCEDURE log_sensor_added",
        )
        .await
    {
        let _ = stream.into_results().await;
    }

    drop(client);

    let mut client = config
        .connect()
        .await
        .expect("Failed to connect to database");
    let stream = client
        .simple_query(
            "CREATE PROCEDURE log_sensor_added
                @p_sensor_id NVARCHAR(MAX),
                @p_temperature FLOAT
            AS
            BEGIN
                INSERT INTO sensor_log (operation, sensor_id, temperature)
                VALUES ('ADD', @p_sensor_id, @p_temperature);
            END",
        )
        .await
        .expect("Failed to create log_sensor_added procedure");
    let _ = stream.into_results().await;

    // Drop and create stored procedure for UPDATE operation
    drop(client);

    let mut client = config
        .connect()
        .await
        .expect("Failed to connect to database");
    if let Ok(stream) = client
        .simple_query(
            "IF OBJECT_ID('log_sensor_updated', 'P') IS NOT NULL DROP PROCEDURE log_sensor_updated",
        )
        .await
    {
        let _ = stream.into_results().await;
    }

    drop(client);

    let mut client = config
        .connect()
        .await
        .expect("Failed to connect to database");
    let stream = client
        .simple_query(
            "CREATE PROCEDURE log_sensor_updated
                @p_sensor_id NVARCHAR(MAX),
                @p_temperature FLOAT
            AS
            BEGIN
                INSERT INTO sensor_log (operation, sensor_id, temperature)
                VALUES ('UPDATE', @p_sensor_id, @p_temperature);
            END",
        )
        .await
        .expect("Failed to create log_sensor_updated procedure");
    let _ = stream.into_results().await;

    // Drop and create stored procedure for DELETE operation
    drop(client);

    let mut client = config
        .connect()
        .await
        .expect("Failed to connect to database");
    if let Ok(stream) = client
        .simple_query(
            "IF OBJECT_ID('log_sensor_deleted', 'P') IS NOT NULL DROP PROCEDURE log_sensor_deleted",
        )
        .await
    {
        let _ = stream.into_results().await;
    }

    drop(client);

    let mut client = config
        .connect()
        .await
        .expect("Failed to connect to database");
    let stream = client
        .simple_query(
            "CREATE PROCEDURE log_sensor_deleted
                @p_sensor_id NVARCHAR(MAX)
            AS
            BEGIN
                INSERT INTO sensor_log (operation, sensor_id, temperature)
                VALUES ('DELETE', @p_sensor_id, NULL);
            END",
        )
        .await
        .expect("Failed to create log_sensor_deleted procedure");
    let _ = stream.into_results().await;
}

/// Helper function to get log entries from the database
async fn get_log_entries(config: &MssqlConfig) -> Vec<(String, String)> {
    let mut client = config
        .connect()
        .await
        .expect("Failed to connect to database");

    let rows = client
        .query(
            "SELECT operation, sensor_id FROM sensor_log ORDER BY logged_at, id",
            &[],
        )
        .await
        .expect("Failed to query sensor_log")
        .into_results()
        .await
        .expect("Failed to get results");

    let mut entries = Vec::new();
    if let Some(rows) = rows.first() {
        for row in rows {
            let operation: &str = row.get(0).expect("Failed to get operation from row");
            let sensor_id: &str = row.get(1).expect("Failed to get sensor_id from row");
            entries.push((operation.to_string(), sensor_id.to_string()));
        }
    }

    entries
}

/// Helper function to get log count
async fn get_log_count(config: &MssqlConfig) -> i64 {
    let mut client = config
        .connect()
        .await
        .expect("Failed to connect to database");

    let rows = client
        .query("SELECT COUNT(*) as count FROM sensor_log", &[])
        .await
        .expect("Failed to query log count")
        .into_results()
        .await
        .expect("Failed to get results");

    if let Some(rows) = rows.first() {
        if let Some(row) = rows.first() {
            let count: i32 = row.get(0).unwrap_or(0);
            return count as i64;
        }
    }

    0
}

/// Poll `sensor_log` until it contains `expected` rows or the timeout elapses.
async fn wait_for_log_count(config: &MssqlConfig, expected: i64, timeout: Duration) -> i64 {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let count = get_log_count(config).await;
        if count >= expected || tokio::time::Instant::now() >= deadline {
            return count;
        }
        sleep(Duration::from_millis(100)).await;
    }
}

/// Build a reaction pointed at the test container that logs sensor changes via
/// stored procedures, binding row fields with the `{{param}}` helper.
async fn build_reaction(config: &MssqlConfig, query_id: &str) -> MsSqlStoredProcReaction {
    MsSqlStoredProcReaction::builder("sensor-sync")
        .with_connection(
            config.host.clone(),
            config.port,
            config.database.clone(),
            config.user.clone(),
            config.password.clone(),
        )
        .with_query(query_id)
        .with_added_command(
            "EXEC log_sensor_added {{param after.sensor_id}}, {{param after.temperature}}",
        )
        .with_updated_command(
            "EXEC log_sensor_updated {{param after.sensor_id}}, {{param after.temperature}}",
        )
        .with_deleted_command("EXEC log_sensor_deleted {{param before.sensor_id}}")
        .with_auto_start(false)
        .build()
        .await
        .expect("Failed to build reaction")
}

fn add_result(query_id: &str, sequence: u64, sensor_id: &str, temperature: f64) -> QueryResult {
    QueryResult::new(
        query_id.to_string(),
        sequence,
        chrono::Utc::now(),
        vec![ResultDiff::Add {
            data: json!({ "sensor_id": sensor_id, "temperature": temperature }),
            row_signature: 0,
        }],
        HashMap::new(),
    )
}

fn update_result(
    query_id: &str,
    sequence: u64,
    sensor_id: &str,
    before_temp: f64,
    after_temp: f64,
) -> QueryResult {
    QueryResult::new(
        query_id.to_string(),
        sequence,
        chrono::Utc::now(),
        vec![ResultDiff::Update {
            data: json!({}),
            before: json!({ "sensor_id": sensor_id, "temperature": before_temp }),
            after: json!({ "sensor_id": sensor_id, "temperature": after_temp }),
            grouping_keys: None,
            row_signature: 0,
        }],
        HashMap::new(),
    )
}

fn delete_result(query_id: &str, sequence: u64, sensor_id: &str) -> QueryResult {
    QueryResult::new(
        query_id.to_string(),
        sequence,
        chrono::Utc::now(),
        vec![ResultDiff::Delete {
            data: json!({ "sensor_id": sensor_id, "temperature": 0.0 }),
            row_signature: 0,
        }],
        HashMap::new(),
    )
}

// ============================================================================
// Unit Tests for Config Module
// ============================================================================

fn template(command: &str) -> QueryConfig {
    QueryConfig {
        added: Some(TemplateSpec::new(command)),
        updated: None,
        deleted: None,
    }
}

#[test]
fn test_config_defaults() {
    let config = MsSqlStoredProcReactionConfig::default();

    // DevSkim: ignore DS137138
    assert_eq!(config.hostname, "localhost");
    assert_eq!(config.get_port(), 1433);
    assert!(!config.ssl);
    assert_eq!(config.command_timeout_ms, 30000);
    assert_eq!(config.retry_attempts, 3);
}

#[test]
fn test_config_validation_requires_user() {
    let config = MsSqlStoredProcReactionConfig {
        user: String::new(),
        database: "test_db".to_string(),
        default_template: Some(template("EXEC test_proc")),
        ..Default::default()
    };

    let result = config.validate(&[]);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("user is required"));
}

#[test]
fn test_config_validation_requires_database() {
    let config = MsSqlStoredProcReactionConfig {
        user: "test_user".to_string(),
        database: String::new(),
        default_template: Some(template("EXEC test_proc")),
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
fn test_config_validation_requires_at_least_one_template() {
    let config = MsSqlStoredProcReactionConfig {
        user: "test_user".to_string(),
        password: "test_pass".to_string(),
        database: "test_db".to_string(),
        ..Default::default()
    };

    let result = config.validate(&[]);
    assert!(result.is_err());
}

#[test]
fn test_config_validation_success() {
    let config = MsSqlStoredProcReactionConfig {
        user: "test_user".to_string(),
        password: "test_pass".to_string(),
        database: "test_db".to_string(),
        default_template: Some(template(
            "EXEC test_proc {{param after.id}}, {{param after.name}}",
        )),
        ..Default::default()
    };

    let result = config.validate(&[]);
    assert!(result.is_ok());
}

#[test]
fn test_config_validation_rejects_malformed_template() {
    let config = MsSqlStoredProcReactionConfig {
        user: "test_user".to_string(),
        database: "test_db".to_string(),
        // Unterminated Handlebars expression.
        default_template: Some(template("EXEC test_proc {{param after.id}")),
        ..Default::default()
    };

    let result = config.validate(&[]);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("invalid default template"));
}

#[test]
fn test_config_validation_rejects_unknown_route_key() {
    let mut routes = HashMap::new();
    routes.insert(
        "not-subscribed".to_string(),
        template("EXEC p {{param after.id}}"),
    );

    let config = MsSqlStoredProcReactionConfig {
        user: "test_user".to_string(),
        database: "test_db".to_string(),
        routes,
        ..Default::default()
    };

    let result = config.validate(&["some-query".to_string()]);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("does not match any subscribed query id"));
}

#[test]
fn test_config_validation_accepts_route_by_last_segment() {
    let mut routes = HashMap::new();
    routes.insert(
        "my-query".to_string(),
        template("EXEC p {{param after.id}}"),
    );

    let config = MsSqlStoredProcReactionConfig {
        user: "test_user".to_string(),
        database: "test_db".to_string(),
        routes,
        ..Default::default()
    };

    // Wire id is `source.my-query`; route keyed by the last segment must match.
    let result = config.validate(&["source.my-query".to_string()]);
    assert!(result.is_ok());
}

#[test]
fn test_config_custom_port() {
    let config = MsSqlStoredProcReactionConfig {
        port: Some(14330),
        ..Default::default()
    };

    assert_eq!(config.get_port(), 14330);
}

// ============================================================================
// Integration Tests with MSSQL (exercise the reaction end-to-end)
// ============================================================================

#[tokio::test]
#[cfg(not(target_arch = "aarch64"))] // Skip on ARM64 - Azure SQL Edge has stability issues
#[serial]
async fn test_reaction_executes_added_procedure() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let result = tokio::time::timeout(Duration::from_secs(180), async {
        let mssql = setup_mssql().await;
        let mssql_config = mssql.config().clone();
        setup_stored_procedures(&mssql_config).await;

        let query_id = "sensor-query";
        let reaction = build_reaction(&mssql_config, query_id).await;
        reaction.start().await.expect("reaction should start");

        reaction
            .enqueue_query_result(add_result(query_id, 1, "sensor-001", 25.5))
            .await
            .expect("enqueue should succeed");

        let count = wait_for_log_count(&mssql_config, 1, Duration::from_secs(15)).await;
        assert_eq!(count, 1, "reaction should have executed the ADD procedure");

        let entries = get_log_entries(&mssql_config).await;
        assert_eq!(entries[0].0, "ADD");
        assert_eq!(entries[0].1, "sensor-001");

        reaction.stop().await.expect("reaction should stop");
        mssql.cleanup().await;
    })
    .await;

    if result.is_err() {
        panic!("Test timed out. The MSSQL container likely failed to start or become ready.");
    }
}

#[tokio::test]
#[cfg(not(target_arch = "aarch64"))] // Skip on ARM64 - Azure SQL Edge has stability issues
#[serial]
async fn test_reaction_executes_all_operations() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let result = tokio::time::timeout(Duration::from_secs(180), async {
        let mssql = setup_mssql().await;
        let mssql_config = mssql.config().clone();
        setup_stored_procedures(&mssql_config).await;

        let query_id = "sensor-query";
        let reaction = build_reaction(&mssql_config, query_id).await;
        reaction.start().await.expect("reaction should start");

        reaction
            .enqueue_query_result(add_result(query_id, 1, "sensor-001", 25.5))
            .await
            .expect("ADD enqueue should succeed");
        reaction
            .enqueue_query_result(update_result(query_id, 2, "sensor-001", 25.5, 26.5))
            .await
            .expect("UPDATE enqueue should succeed");
        reaction
            .enqueue_query_result(delete_result(query_id, 3, "sensor-001"))
            .await
            .expect("DELETE enqueue should succeed");

        let count = wait_for_log_count(&mssql_config, 3, Duration::from_secs(20)).await;
        assert_eq!(
            count, 3,
            "reaction should have executed all three procedures"
        );

        let entries = get_log_entries(&mssql_config).await;
        assert_eq!(entries[0].0, "ADD");
        assert_eq!(entries[1].0, "UPDATE");
        assert_eq!(entries[2].0, "DELETE");
        assert!(entries.iter().all(|(_, sensor)| sensor == "sensor-001"));

        reaction.stop().await.expect("reaction should stop");
        mssql.cleanup().await;
    })
    .await;

    if result.is_err() {
        panic!("Test timed out. The MSSQL container likely failed to start or become ready.");
    }
}

/// Regression test for the historically broken `{{param}}` binding: a value
/// containing SQL metacharacters must be bound as a parameter (never
/// interpolated), so the payload is stored verbatim and cannot alter the batch.
#[tokio::test]
#[cfg(not(target_arch = "aarch64"))] // Skip on ARM64 - Azure SQL Edge has stability issues
#[serial]
async fn test_reaction_binds_special_characters_safely() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    let result = tokio::time::timeout(Duration::from_secs(180), async {
        let mssql = setup_mssql().await;
        let mssql_config = mssql.config().clone();
        setup_stored_procedures(&mssql_config).await;

        let query_id = "sensor-query";
        let reaction = build_reaction(&mssql_config, query_id).await;
        reaction.start().await.expect("reaction should start");

        let malicious = "sensor'; DROP TABLE sensor_log; --";
        reaction
            .enqueue_query_result(add_result(query_id, 1, malicious, 25.5))
            .await
            .expect("enqueue should succeed");

        let count = wait_for_log_count(&mssql_config, 1, Duration::from_secs(15)).await;
        assert_eq!(count, 1, "table must still exist and hold the row");

        let entries = get_log_entries(&mssql_config).await;
        assert_eq!(entries[0].1, malicious, "value must be stored verbatim");

        reaction.stop().await.expect("reaction should stop");
        mssql.cleanup().await;
    })
    .await;

    if result.is_err() {
        panic!("Test timed out. The MSSQL container likely failed to start or become ready.");
    }
}
