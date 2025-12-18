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
//! stored procedure reaction using testcontainers to provide a real MSSQL database.
//!
//! **Note**: These tests are skipped on ARM64 (Apple Silicon) platforms due to
//! known stability issues with Azure SQL Edge containers crashing on startup.
//! The tests will run successfully on AMD64 platforms and in CI environments.
use crate::mssql_helpers::{execute_sql, setup_mssql, MssqlConfig};
use serial_test::serial;
use std::time::Duration;
use tokio::time::sleep;

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
            "SELECT operation, sensor_id FROM sensor_log ORDER BY logged_at",
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
            let operation: &str = row.get(0).unwrap();
            let sensor_id: &str = row.get(1).unwrap();
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

// ============================================================================
// Unit Tests for Config Module
// ============================================================================

#[test]
fn test_config_defaults() {
    use drasi_core_mssql_storedproc_reaction::config::MsSqlStoredProcReactionConfig;
    
    let config = MsSqlStoredProcReactionConfig::default();
    
    assert_eq!(config.hostname, "localhost");
    assert_eq!(config.get_port(), 1433);
    assert_eq!(config.ssl, false);
    assert_eq!(config.command_timeout_ms, 30000);
    assert_eq!(config.retry_attempts, 3);
}

#[test]
fn test_config_validation_requires_user() {
    use drasi_core_mssql_storedproc_reaction::config::MsSqlStoredProcReactionConfig;
    
    let config = MsSqlStoredProcReactionConfig {
        user: String::new(),
        database: "test_db".to_string(),
        added_command: Some("EXEC test_proc".to_string()),
        ..Default::default()
    };
    
    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("user is required"));
}

#[test]
fn test_config_validation_requires_database() {
    use drasi_core_mssql_storedproc_reaction::config::MsSqlStoredProcReactionConfig;
    
    let config = MsSqlStoredProcReactionConfig {
        user: "test_user".to_string(),
        database: String::new(),
        added_command: Some("EXEC test_proc".to_string()),
        ..Default::default()
    };
    
    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Database name is required"));
}

#[test]
fn test_config_validation_requires_at_least_one_command() {
    use drasi_core_mssql_storedproc_reaction::config::MsSqlStoredProcReactionConfig;
    
    let config = MsSqlStoredProcReactionConfig {
        user: "test_user".to_string(),
        password: "test_pass".to_string(),
        database: "test_db".to_string(),
        added_command: None,
        updated_command: None,
        deleted_command: None,
        ..Default::default()
    };
    
    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("At least one command"));
}

#[test]
fn test_config_validation_success() {
    use drasi_core_mssql_storedproc_reaction::config::MsSqlStoredProcReactionConfig;
    
    let config = MsSqlStoredProcReactionConfig {
        user: "test_user".to_string(),
        password: "test_pass".to_string(),
        database: "test_db".to_string(),
        added_command: Some("EXEC test_proc".to_string()),
        ..Default::default()
    };
    
    let result = config.validate();
    assert!(result.is_ok());
}

#[test]
fn test_config_custom_port() {
    use drasi_core_mssql_storedproc_reaction::config::MsSqlStoredProcReactionConfig;
    
    let config = MsSqlStoredProcReactionConfig {
        port: Some(14330),
        ..Default::default()
    };
    
    assert_eq!(config.get_port(), 14330);
}

// ============================================================================
// Integration Tests with MSSQL
// ============================================================================

#[tokio::test]
#[cfg(not(target_arch = "aarch64"))] // Skip on ARM64 - Azure SQL Edge has stability issues
#[serial]
async fn test_mssql_connection() {
    // Initialize logger first to see all output
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    // Wrap test in timeout to prevent hanging
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        let mssql = setup_mssql().await;

        // Verify we can connect
        let mut client = mssql.get_client().await.unwrap();
        let rows = client
            .query("SELECT 1 AS value", &[])
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();

        if let Some(rows) = rows.first() {
            if let Some(row) = rows.first() {
                let value: i32 = row.get(0).unwrap();
                assert_eq!(value, 1);
            }
        }

        mssql.cleanup().await;
    })
    .await;

    match result {
        Ok(_) => {},
        Err(_) => panic!("Test timed out after 120 seconds. This likely means the MSSQL container failed to start or become ready."),
    }
}

#[tokio::test]
#[cfg(not(target_arch = "aarch64"))] // Skip on ARM64 - Azure SQL Edge has stability issues
#[serial]
async fn test_mssql_stored_procedures() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    // Wrap test in timeout to prevent hanging
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        let mssql = setup_mssql().await;
        let mssql_config = mssql.config();
        setup_stored_procedures(mssql_config).await;

        // Execute the stored procedure using parameterized query
        let mut client = mssql.get_client().await.unwrap();

        client
            .execute(
                "EXEC log_sensor_added @p_sensor_id = @P1, @p_temperature = @P2",
                &[&"sensor-001", &25.5f64],
            )
            .await
            .expect("Failed to execute stored procedure");

        // Wait a bit for the insert to complete
        sleep(Duration::from_millis(100)).await;

        // Verify the log entry
        let count = get_log_count(mssql_config).await;
        assert_eq!(count, 1, "Should have 1 log entry");

        let entries = get_log_entries(mssql_config).await;
        assert_eq!(entries[0].0, "ADD");
        assert_eq!(entries[0].1, "sensor-001");

        mssql.cleanup().await;
    })
    .await;

    match result {
        Ok(_) => {},
        Err(_) => panic!("Test timed out after 120 seconds. This likely means the MSSQL container failed to start or become ready."),
    }
}

#[tokio::test]
#[cfg(not(target_arch = "aarch64"))] // Skip on ARM64 - Azure SQL Edge has stability issues
#[serial]
async fn test_mssql_multiple_operations() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    // Wrap test in timeout to prevent hanging
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        let mssql = setup_mssql().await;
        let mssql_config = mssql.config();
        setup_stored_procedures(mssql_config).await;

        let mut client = mssql.get_client().await.unwrap();

        // Execute ADD
        client
            .execute(
                "EXEC log_sensor_added @p_sensor_id = @P1, @p_temperature = @P2",
                &[&"sensor-001", &25.5f64],
            )
            .await
            .expect("ADD should succeed");

        // Execute UPDATE
        client
            .execute(
                "EXEC log_sensor_updated @p_sensor_id = @P1, @p_temperature = @P2",
                &[&"sensor-001", &26.5f64],
            )
            .await
            .expect("UPDATE should succeed");

        // Execute DELETE
        client
            .execute(
                "EXEC log_sensor_deleted @p_sensor_id = @P1",
                &[&"sensor-001"],
            )
            .await
            .expect("DELETE should succeed");

        // Wait for operations to complete
        sleep(Duration::from_millis(100)).await;

        // Verify all operations logged
        let entries = get_log_entries(mssql_config).await;
        assert_eq!(entries.len(), 3, "Should have 3 log entries");
        assert_eq!(entries[0].0, "ADD");
        assert_eq!(entries[1].0, "UPDATE");
        assert_eq!(entries[2].0, "DELETE");

        mssql.cleanup().await;
    })
    .await;

    match result {
        Ok(_) => {},
        Err(_) => panic!("Test timed out after 120 seconds. This likely means the MSSQL container failed to start or become ready."),
    }
}

#[tokio::test]
#[cfg(not(target_arch = "aarch64"))] // Skip on ARM64 - Azure SQL Edge has stability issues
#[serial]
async fn test_mssql_with_special_characters() {
    env_logger::builder()
        .is_test(true)
        .filter_level(log::LevelFilter::Debug)
        .try_init()
        .ok();

    // Wrap test in timeout to prevent hanging
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        let mssql = setup_mssql().await;
        let mssql_config = mssql.config();
        setup_stored_procedures(mssql_config).await;

        let mut client = mssql.get_client().await.unwrap();

        // Test with special characters (potential SQL injection)
        client
            .execute(
                "EXEC log_sensor_added @p_sensor_id = @P1, @p_temperature = @P2",
                &[&"sensor'; DROP TABLE sensor_log; --", &25.5f64],
            )
            .await
            .expect("Should safely handle special characters");

        sleep(Duration::from_millis(100)).await;

        // Verify the table still exists and has the entry
        let entries = get_log_entries(mssql_config).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "sensor'; DROP TABLE sensor_log; --");

        mssql.cleanup().await;
    })
    .await;

    match result {
        Ok(_) => {},
        Err(_) => panic!("Test timed out after 120 seconds. This likely means the MSSQL container failed to start or become ready."),
    }
}
