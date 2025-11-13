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

//! Integration tests for storage backend configuration

use drasi_server_core::{DrasiServerCore, Query, Source, StorageBackendConfig, StorageBackendSpec};
use std::time::Duration;
use tokio::time::sleep;

/// Test query with inline memory backend configuration
#[tokio::test]
async fn test_inline_memory_backend() {
    let core = DrasiServerCore::builder()
        .with_id("test_inline_memory")
        .add_source(Source::application("events").auto_start(true).build())
        .add_query(
            Query::cypher("test_query")
                .query("MATCH (n:Event) RETURN n")
                .from_source("events")
                .with_inline_storage(StorageBackendSpec::Memory {
                    enable_archive: true,
                })
                .auto_start(true)
                .build(),
        )
        .build()
        .await
        .expect("Failed to build server");

    core.start().await.expect("Failed to start server");

    // Verify query is running
    let status = core
        .get_query_status("test_query")
        .await
        .expect("Failed to get query status");
    assert!(matches!(
        status,
        drasi_server_core::ComponentStatus::Running
    ));

    core.stop().await.expect("Failed to stop server");
}

/// Test query with named memory backend
#[tokio::test]
async fn test_named_memory_backend() {
    let core = DrasiServerCore::builder()
        .with_id("test_named_memory")
        .add_storage_backend(StorageBackendConfig {
            id: "memory_backend".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        })
        .add_source(Source::application("events").auto_start(true).build())
        .add_query(
            Query::cypher("test_query")
                .query("MATCH (n:Event) RETURN n")
                .from_source("events")
                .with_storage_backend("memory_backend")
                .auto_start(true)
                .build(),
        )
        .build()
        .await
        .expect("Failed to build server");

    core.start().await.expect("Failed to start server");

    // Verify query is running
    let status = core
        .get_query_status("test_query")
        .await
        .expect("Failed to get query status");
    assert!(matches!(
        status,
        drasi_server_core::ComponentStatus::Running
    ));

    core.stop().await.expect("Failed to stop server");
}

/// Test query with RocksDB backend (creates temp directory)
#[tokio::test]
async fn test_rocksdb_backend() {
    // Create temporary directory for RocksDB
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("test_db");

    let core = DrasiServerCore::builder()
        .with_id("test_rocksdb")
        .add_storage_backend(StorageBackendConfig {
            id: "rocks_backend".to_string(),
            spec: StorageBackendSpec::RocksDb {
                path: db_path.to_string_lossy().to_string(),
                enable_archive: true,
                direct_io: false,
            },
        })
        .add_source(Source::application("events").auto_start(true).build())
        .add_query(
            Query::cypher("test_query")
                .query("MATCH (n:Event) RETURN n")
                .from_source("events")
                .with_storage_backend("rocks_backend")
                .auto_start(true)
                .build(),
        )
        .build()
        .await
        .expect("Failed to build server");

    core.start().await.expect("Failed to start server");

    // Give query time to start
    sleep(Duration::from_millis(100)).await;

    // Verify query is running
    let status = core
        .get_query_status("test_query")
        .await
        .expect("Failed to get query status");
    assert!(matches!(
        status,
        drasi_server_core::ComponentStatus::Running
    ));

    core.stop().await.expect("Failed to stop server");

    // Verify RocksDB directory was created
    assert!(db_path.exists(), "RocksDB directory should exist");
}

/// Test multiple queries with different backends
#[tokio::test]
async fn test_multiple_backends() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("multi_test_db");

    let core = DrasiServerCore::builder()
        .with_id("test_multiple_backends")
        .add_storage_backend(StorageBackendConfig {
            id: "memory_fast".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        })
        .add_storage_backend(StorageBackendConfig {
            id: "rocks_persistent".to_string(),
            spec: StorageBackendSpec::RocksDb {
                path: db_path.to_string_lossy().to_string(),
                enable_archive: true,
                direct_io: false,
            },
        })
        .add_source(Source::application("events").auto_start(true).build())
        .add_query(
            Query::cypher("fast_query")
                .query("MATCH (n:FastEvent) RETURN n")
                .from_source("events")
                .with_storage_backend("memory_fast")
                .auto_start(true)
                .build(),
        )
        .add_query(
            Query::cypher("persistent_query")
                .query("MATCH (n:ImportantEvent) RETURN n")
                .from_source("events")
                .with_storage_backend("rocks_persistent")
                .auto_start(true)
                .build(),
        )
        .add_query(
            Query::cypher("default_query")
                .query("MATCH (n:RegularEvent) RETURN n")
                .from_source("events")
                .auto_start(true) // No storage backend - uses default in-memory
                .build(),
        )
        .build()
        .await
        .expect("Failed to build server");

    core.start().await.expect("Failed to start server");

    // Give queries time to start
    sleep(Duration::from_millis(200)).await;

    // Verify all queries are running
    for query_id in &["fast_query", "persistent_query", "default_query"] {
        let status = core
            .get_query_status(query_id)
            .await
            .unwrap_or_else(|_| panic!("Failed to get status for {}", query_id));
        assert!(
            matches!(status, drasi_server_core::ComponentStatus::Running),
            "Query {} should be running",
            query_id
        );
    }

    core.stop().await.expect("Failed to stop server");
}

/// Test builder API for adding multiple backends at once
#[tokio::test]
async fn test_add_multiple_backends_api() {
    let backends = vec![
        StorageBackendConfig {
            id: "backend1".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        },
        StorageBackendConfig {
            id: "backend2".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: true,
            },
        },
    ];

    let core = DrasiServerCore::builder()
        .with_id("test_bulk_backends")
        .add_storage_backends(backends)
        .add_source(Source::application("events").auto_start(true).build())
        .build()
        .await
        .expect("Failed to build server");

    core.start().await.expect("Failed to start server");
    core.stop().await.expect("Failed to stop server");
}

/// Test error handling for invalid backend reference
#[tokio::test]
async fn test_invalid_backend_reference() {
    // Try to build server with query referencing nonexistent backend
    let result = DrasiServerCore::builder()
        .with_id("test_invalid_ref")
        .add_source(Source::application("events").auto_start(true).build())
        .add_query(
            Query::cypher("bad_query")
                .query("MATCH (n) RETURN n")
                .from_source("events")
                .with_storage_backend("nonexistent_backend") // Invalid reference
                .auto_start(false)
                .build(),
        )
        .build()
        .await;

    // Build should fail with validation error for unknown backend
    match result {
        Ok(_) => panic!("Building server with invalid backend reference should fail"),
        Err(err) => {
            let err_msg = err.to_string();
            assert!(
                err_msg.contains("unknown storage backend")
                    || err_msg.contains("nonexistent_backend"),
                "Error message should mention unknown backend, got: {}",
                err_msg
            );
        }
    }
}

/// Test query with inline RocksDB backend
#[tokio::test]
async fn test_inline_rocksdb_backend() {
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let db_path = temp_dir.path().join("inline_test_db");

    let core = DrasiServerCore::builder()
        .with_id("test_inline_rocksdb")
        .add_source(Source::application("events").auto_start(true).build())
        .add_query(
            Query::cypher("test_query")
                .query("MATCH (n:Event) RETURN n")
                .from_source("events")
                .with_inline_storage(StorageBackendSpec::RocksDb {
                    path: db_path.to_string_lossy().to_string(),
                    enable_archive: true,
                    direct_io: false,
                })
                .auto_start(true)
                .build(),
        )
        .build()
        .await
        .expect("Failed to build server");

    core.start().await.expect("Failed to start server");

    // Give query time to start
    sleep(Duration::from_millis(100)).await;

    // Verify query is running
    let status = core
        .get_query_status("test_query")
        .await
        .expect("Failed to get query status");
    assert!(matches!(
        status,
        drasi_server_core::ComponentStatus::Running
    ));

    core.stop().await.expect("Failed to stop server");

    // Verify RocksDB directory was created
    assert!(
        db_path.exists(),
        "RocksDB directory should exist for inline backend"
    );
}
