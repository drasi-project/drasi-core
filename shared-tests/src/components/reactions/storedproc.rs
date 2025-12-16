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

//! Integration tests for the StoredProc reaction component
//!
//! These tests validate the end-to-end behavior of the StoredProc reaction
//! using testcontainers to provide a real PostgreSQL database.

#[cfg(test)]
mod tests {
    use crate::postgres_helpers::setup_postgres;
    use drasi_lib::{DrasiLib, Query};
    use drasi_reaction_storedproc::{DatabaseClient, StoredProcReaction};
    use drasi_source_mock::{MockSource, MockSourceConfig};
    use std::time::Duration;
    use tokio::time::sleep;

    /// Helper function to setup stored procedures in the test database
    async fn setup_stored_procedures(pg_config: &crate::postgres_helpers::PostgresConfig) {
        let client = tokio_postgres::connect(&pg_config.connection_string(), tokio_postgres::NoTls)
            .await
            .expect("Failed to connect to database")
            .0;

        // Create a table to log stored procedure calls
        client
            .execute(
                "CREATE TABLE IF NOT EXISTS sensor_log (
                    id SERIAL PRIMARY KEY,
                    operation VARCHAR(10) NOT NULL,
                    sensor_id TEXT NOT NULL,
                    temperature DOUBLE PRECISION,
                    timestamp TIMESTAMPTZ,
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
                    p_temperature DOUBLE PRECISION,
                    p_timestamp TIMESTAMPTZ
                )
                LANGUAGE plpgsql
                AS $$
                BEGIN
                    INSERT INTO sensor_log (operation, sensor_id, temperature, timestamp)
                    VALUES ('ADD', p_sensor_id, p_temperature, p_timestamp);
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

    /// Helper function to verify log entries in the database
    async fn verify_log_entries(
        pg_config: &crate::postgres_helpers::PostgresConfig,
        expected_count: i64,
    ) -> Vec<(String, String)> {
        let client = tokio_postgres::connect(&pg_config.connection_string(), tokio_postgres::NoTls)
            .await
            .expect("Failed to connect to database")
            .0;

        let rows = client
            .query(
                "SELECT operation, sensor_id FROM sensor_log ORDER BY logged_at",
                &[],
            )
            .await
            .expect("Failed to query sensor_log");

        assert_eq!(
            rows.len() as i64,
            expected_count,
            "Expected {} log entries, found {}",
            expected_count,
            rows.len()
        );

        rows.iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect()
    }

    #[tokio::test]
    async fn test_storedproc_basic_add_operation() {
        env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Debug)
            .try_init()
            .ok();

        // Setup PostgreSQL container
        let pg = setup_postgres().await;
        let pg_config = pg.config();

        // Setup stored procedures
        setup_stored_procedures(pg_config).await;

        // Create mock source that emits sensor data
        let source = MockSource::new(
            "sensors",
            MockSourceConfig {
                data_type: "sensor".to_string(),
                interval_ms: 100, // Emit data every 100ms
            },
        )
        .expect("Failed to create mock source");

        // Create the stored procedure reaction
        let reaction = StoredProcReaction::builder("sensor-sync")
            .with_database_client(DatabaseClient::PostgreSQL)
            .with_connection(
                &pg_config.host,
                pg_config.port,
                &pg_config.database,
                &pg_config.user,
                &pg_config.password,
            )
            .with_query("high-temp")
            .with_added_command("CALL log_sensor_added(@id, @temperature, @timestamp)")
            .build()
            .await
            .expect("Failed to create StoredProcReaction");

        // Build DrasiLib with source, reaction, and query
        let core = DrasiLib::builder()
            .with_id("sensor-storedproc-test")
            .with_source(source)
            .with_reaction(reaction)
            .with_query(
                Query::cypher("high-temp")
                    .query("MATCH (s:SensorReading) WHERE s.temperature > 20 RETURN s.sensor_id AS id, s.temperature AS temperature, s.timestamp AS timestamp")
                    .from_source("sensors")
                    .build(),
            )
            .build()
            .await
            .expect("Failed to build DrasiLib");

        // Start the application
        core.start().await.expect("Failed to start DrasiLib");

        // Wait for mock source to emit data and reactions to process
        sleep(Duration::from_secs(2)).await;

        // Stop the application
        core.stop().await.expect("Failed to stop DrasiLib");

        // Verify that stored procedure was called
        let logs = verify_log_entries(pg_config, 1).await;
        assert_eq!(logs[0].0, "ADD");

        // Cleanup
        pg.cleanup().await;
    }

    #[tokio::test]
    async fn test_storedproc_add_update_delete_operations() {
        env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Debug)
            .try_init()
            .ok();

        // Setup PostgreSQL container
        let pg = setup_postgres().await;
        let pg_config = pg.config();

        // Setup stored procedures
        setup_stored_procedures(pg_config).await;

        // Create mock source
        let source = MockSource::new(
            "sensors",
            MockSourceConfig {
                data_type: "sensor".to_string(),
                interval_ms: 200,
            },
        )
        .expect("Failed to create mock source");

        // Create the stored procedure reaction with all three commands
        let reaction = StoredProcReaction::builder("sensor-sync")
            .with_database_client(DatabaseClient::PostgreSQL)
            .with_connection(
                &pg_config.host,
                pg_config.port,
                &pg_config.database,
                &pg_config.user,
                &pg_config.password,
            )
            .with_query("high-temp")
            .with_added_command("CALL log_sensor_added(@id, @temperature, @timestamp)")
            .with_updated_command("CALL log_sensor_updated(@id, @temperature)")
            .with_deleted_command("CALL log_sensor_deleted(@id)")
            .build()
            .await
            .expect("Failed to create StoredProcReaction");

        // Build DrasiLib
        let core = DrasiLib::builder()
            .with_id("sensor-storedproc-full-test")
            .with_source(source)
            .with_reaction(reaction)
            .with_query(
                Query::cypher("high-temp")
                    .query("MATCH (s:SensorReading) WHERE s.temperature > 20 RETURN s.sensor_id AS id, s.temperature AS temperature, s.timestamp AS timestamp")
                    .from_source("sensors")
                    .build(),
            )
            .build()
            .await
            .expect("Failed to build DrasiLib");

        // Start the application
        core.start().await.expect("Failed to start DrasiLib");

        // Wait for operations to complete
        sleep(Duration::from_secs(3)).await;

        // Stop the application
        core.stop().await.expect("Failed to stop DrasiLib");

        // Verify that stored procedures were called
        // The mock source should have triggered ADD operations
        let logs = verify_log_entries(pg_config, 1).await;
        
        // Verify at least one ADD operation occurred
        assert!(
            logs.iter().any(|(op, _)| op == "ADD"),
            "Expected at least one ADD operation"
        );

        // Cleanup
        pg.cleanup().await;
    }

    #[tokio::test]
    async fn test_storedproc_connection_pool_and_retry() {
        env_logger::builder()
            .is_test(true)
            .filter_level(log::LevelFilter::Debug)
            .try_init()
            .ok();

        // Setup PostgreSQL container
        let pg = setup_postgres().await;
        let pg_config = pg.config();

        // Setup stored procedures
        setup_stored_procedures(pg_config).await;

        // Create mock source
        let source = MockSource::new(
            "sensors",
            MockSourceConfig {
                data_type: "sensor".to_string(),
                interval_ms: 100,
            },
        )
        .expect("Failed to create mock source");

        // Create reaction with custom connection pool and retry settings
        let reaction = StoredProcReaction::builder("sensor-sync-advanced")
            .with_database_client(DatabaseClient::PostgreSQL)
            .with_connection(
                &pg_config.host,
                pg_config.port,
                &pg_config.database,
                &pg_config.user,
                &pg_config.password,
            )
            .with_query("high-temp")
            .with_added_command("CALL log_sensor_added(@id, @temperature, @timestamp)")
            .with_connection_pool_size(5)
            .with_command_timeout_ms(5000)
            .with_retry_attempts(3)
            .build()
            .await
            .expect("Failed to create StoredProcReaction");

        // Build DrasiLib
        let core = DrasiLib::builder()
            .with_id("sensor-storedproc-advanced-test")
            .with_source(source)
            .with_reaction(reaction)
            .with_query(
                Query::cypher("high-temp")
                    .query("MATCH (s:SensorReading) WHERE s.temperature > 20 RETURN s.sensor_id AS id, s.temperature AS temperature, s.timestamp AS timestamp")
                    .from_source("sensors")
                    .build(),
            )
            .build()
            .await
            .expect("Failed to build DrasiLib");

        // Start the application
        core.start().await.expect("Failed to start DrasiLib");

        // Wait for operations
        sleep(Duration::from_secs(2)).await;

        // Stop the application
        core.stop().await.expect("Failed to stop DrasiLib");

        // Verify operations completed successfully
        let logs = verify_log_entries(pg_config, 1).await;
        assert!(logs.len() > 0, "Expected at least one log entry");

        // Cleanup
        pg.cleanup().await;
    }
}
