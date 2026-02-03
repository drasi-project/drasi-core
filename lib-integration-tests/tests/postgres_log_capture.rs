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

//! Integration tests for PostgreSQL source log capture.
//!
//! These tests verify that logs from the PostgreSQL source (including errors
//! from spawned tasks) are properly routed to the component log streaming
//! infrastructure and accessible via the DrasiLib public API.

use drasi_lib::{DrasiLib, LogLevel};
use drasi_source_postgres::PostgresReplicationSource;
use std::time::Duration;
use testcontainers::{runners::AsyncRunner, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tokio::time::timeout;

/// Test that logs from a successfully connected PostgreSQL source are captured.
/// This test requires Docker to be running.
#[tokio::test]
async fn test_postgres_source_logs_captured_on_success() {
    // Start a PostgreSQL container with logical replication enabled
    let container = Postgres::default()
        .with_cmd([
            "postgres",
            "-c",
            "wal_level=logical",
            "-c",
            "max_replication_slots=10",
            "-c",
            "max_wal_senders=10",
        ])
        .start()
        .await
        .expect("Failed to start PostgreSQL container");

    let host = container.get_host().await.expect("Failed to get host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("Failed to get port");

    // Set up the database with a publication
    let conn_str =
        format!("host={host} port={port} user=postgres password=postgres dbname=postgres");
    let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls)
        .await
        .expect("Failed to connect to PostgreSQL");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {e}");
        }
    });

    // Create a test table and publication
    client
        .execute(
            "CREATE TABLE test_items (id SERIAL PRIMARY KEY, name TEXT)",
            &[],
        )
        .await
        .expect("Failed to create table");

    client
        .execute("CREATE PUBLICATION drasi_pub FOR TABLE test_items", &[])
        .await
        .expect("Failed to create publication");

    // Create the PostgreSQL source
    let source = PostgresReplicationSource::builder("test-pg-source")
        .with_host(host.to_string())
        .with_port(port)
        .with_database("postgres")
        .with_user("postgres")
        .with_password("postgres")
        .with_tables(vec!["test_items".to_string()])
        .with_publication_name("drasi_pub")
        .with_slot_name("test_slot")
        .build()
        .expect("Failed to build PostgreSQL source");

    // Create DrasiLib and add the source
    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    drasi.start().await.expect("Failed to start DrasiLib");

    // Give the source time to start, connect, and emit debug logs
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Use the DrasiLib public API to get logs
    let (history, _receiver) = drasi
        .subscribe_source_logs("test-pg-source")
        .await
        .expect("Failed to subscribe to source logs");

    assert!(
        !history.is_empty(),
        "Expected logs to be captured for the PostgreSQL source"
    );

    println!(
        "All captured logs: {:?}",
        history
            .iter()
            .map(|l| format!("[{:?}] {}", l.level, l.message))
            .collect::<Vec<_>>()
    );

    // Check for lifecycle logs
    let has_lifecycle_log = history
        .iter()
        .any(|log| log.message.contains("replication") || log.message.contains("Starting"));
    assert!(
        has_lifecycle_log,
        "Expected lifecycle logs, got: {:?}",
        history.iter().map(|l| &l.message).collect::<Vec<_>>()
    );

    // Check for debug-level logs from internal operations (e.g., connection, replication protocol)
    let has_debug_log = history.iter().any(|log| {
        log.level == LogLevel::Debug
            || log.message.contains("handshake")
            || log.message.contains("Authentication")
            || log.message.contains("IDENTIFY_SYSTEM")
            || log.message.contains("slot")
            || log.message.contains("Received")
    });
    assert!(
        has_debug_log,
        "Expected debug-level logs from postgres source internals, got: {:?}",
        history
            .iter()
            .map(|l| format!("[{:?}] {}", l.level, l.message))
            .collect::<Vec<_>>()
    );

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test that error logs are captured when PostgreSQL source fails to connect.
/// Uses the DrasiLib public API to retrieve logs.
#[tokio::test]
async fn test_postgres_source_logs_captured_on_connection_failure() {
    // Create a source pointing to a non-existent database
    let source = PostgresReplicationSource::builder("failing-pg-source")
        .with_host("localhost")
        .with_port(59999) // Non-existent port
        .with_database("nonexistent_db")
        .with_user("postgres")
        .with_password("postgres")
        .with_tables(vec!["test_table".to_string()])
        .build()
        .expect("Failed to build PostgreSQL source");

    // Create DrasiLib and add the source
    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    drasi.start().await.expect("Failed to start DrasiLib");

    // Give the source time to attempt connection and fail
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Use the DrasiLib public API to get logs
    let (history, _receiver) = drasi
        .subscribe_source_logs("failing-pg-source")
        .await
        .expect("Failed to subscribe to source logs");

    // Should have at least lifecycle logs
    assert!(
        !history.is_empty(),
        "Expected logs to be captured for the failing PostgreSQL source"
    );

    // Look for error logs from the connection failure
    let has_error_log = history.iter().any(|log| {
        log.level == LogLevel::Error
            || log.message.to_lowercase().contains("error")
            || log.message.to_lowercase().contains("failed")
            || log.message.to_lowercase().contains("connection")
    });

    println!(
        "Captured logs via DrasiLib API: {:?}",
        history
            .iter()
            .map(|l| format!("[{:?}] {}", l.level, l.message))
            .collect::<Vec<_>>()
    );

    assert!(
        has_error_log,
        "Expected error logs from connection failure, got: {:?}",
        history
            .iter()
            .map(|l| format!("[{:?}] {}", l.level, l.message))
            .collect::<Vec<_>>()
    );

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test that log streaming works for PostgreSQL source via DrasiLib API.
#[tokio::test]
async fn test_postgres_source_log_streaming() {
    // Create a source pointing to a non-existent port (will fail quickly)
    let source = PostgresReplicationSource::builder("streaming-pg-source")
        .with_host("localhost")
        .with_port(59998) // Non-existent port
        .with_database("test_db")
        .with_user("postgres")
        .with_password("postgres")
        .with_tables(vec!["test".to_string()])
        .build()
        .expect("Failed to build PostgreSQL source");

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to logs BEFORE starting using the DrasiLib public API
    let (initial_history, mut log_stream) = drasi
        .subscribe_source_logs("streaming-pg-source")
        .await
        .expect("Failed to subscribe to source logs");

    // Initial history should be empty before start
    println!("Initial history before start: {initial_history:?}");

    drasi.start().await.expect("Failed to start DrasiLib");

    // Wait for logs to stream in
    let mut received_logs = Vec::new();
    let _collect_result = timeout(Duration::from_secs(5), async {
        while let Ok(log) = log_stream.recv().await {
            received_logs.push(log);
            // Collect a few logs then stop
            if received_logs.len() >= 2 {
                break;
            }
        }
    })
    .await;

    drasi.stop().await.expect("Failed to stop DrasiLib");

    // We should have received at least some logs via streaming
    assert!(
        !received_logs.is_empty(),
        "Expected to receive logs via streaming"
    );

    println!(
        "Streamed logs via DrasiLib API: {:?}",
        received_logs
            .iter()
            .map(|l| format!("[{:?}] {}", l.level, l.message))
            .collect::<Vec<_>>()
    );
}
