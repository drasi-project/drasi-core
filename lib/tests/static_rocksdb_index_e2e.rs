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

//! End-to-end tests for the "statically-linked index backend" path.
//!
//! This exercises exactly how a host such as `drasi-server` is expected to use a
//! statically-compiled index backend crate (here `drasi-index-rocksdb` built with
//! the `plugin-descriptor` feature):
//!
//!   1. Read a `kind` + DTO storage-backend config (the standard plugin format).
//!   2. Look up the matching `IndexBackendPluginDescriptor` by `kind`.
//!   3. Call `create_index_backend(config)` to build an `Arc<dyn IndexBackendPlugin>`
//!      — this is where env-var/secret resolution of fields like `path` happens.
//!   4. Inject it as a *named* provider via `with_index_provider(name, provider)`.
//!   5. Reference it from a query via `StorageBackendRef::Named(name)`.
//!
//! Because there is no FFI boundary when the backend crate is statically linked,
//! `Arc<dyn IndexBackendPlugin>` (defined in `drasi-core`) is shared directly
//! between the backend crate and `drasi-lib`.

mod mock_source;

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

use drasi_index_rocksdb::RocksDbIndexDescriptor;
use drasi_lib::{ComponentStatus, DrasiLib, Query, StorageBackendRef};
use drasi_plugin_sdk::IndexBackendPluginDescriptor;
use mock_source::{MockSource, MockSourceHandle, PropertyMapBuilder};
use serde_json::json;
use tempfile::TempDir;

/// Build a RocksDB-backed index provider the way `drasi-server` would: from a
/// JSON storage-backend config, via the statically-linked descriptor.
async fn build_provider_from_config(
    config: &serde_json::Value,
) -> Arc<dyn drasi_core::interface::IndexBackendPlugin> {
    // A real host dispatches on `config["kind"]` to pick the descriptor. We assert
    // the configured kind matches the statically-linked descriptor, then strip the
    // discriminator before handing the DTO body to the factory method.
    let descriptor = RocksDbIndexDescriptor;
    assert_eq!(
        config["kind"].as_str(),
        Some(descriptor.kind()),
        "config kind must match the statically-linked descriptor"
    );

    let mut dto = config.clone();
    dto.as_object_mut()
        .expect("storage backend config must be an object")
        .remove("kind");

    descriptor
        .create_index_backend(&dto)
        .await
        .expect("descriptor should build a RocksDB index backend from config")
}

async fn insert_person(handle: &MockSourceHandle, id: &str, name: &str, age: i64) -> Result<()> {
    let props = PropertyMapBuilder::new()
        .with_string("name", name)
        .with_integer("age", age)
        .build();
    handle.send_node_insert(id, vec!["Person"], props).await
}

fn person_query(storage_backend: &str) -> drasi_lib::config::QueryConfig {
    Query::cypher("people")
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_source("people-src")
        .auto_start(true)
        .enable_bootstrap(true)
        .with_storage_backend(StorageBackendRef::Named(storage_backend.to_string()))
        .build()
}

/// Poll the query results until `expected` rows are present (or time out).
async fn wait_for_results(
    core: &DrasiLib,
    query_id: &str,
    expected: usize,
) -> Vec<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match core.get_query_results(query_id).await {
            Ok(results) if results.len() >= expected => return results,
            Ok(results) if tokio::time::Instant::now() >= deadline => return results,
            // Query may still be transitioning to Running, or results not yet
            // materialized; keep polling until the deadline.
            Ok(_) | Err(_) if tokio::time::Instant::now() >= deadline => return Vec::new(),
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Poll until the query reaches the expected status (or time out).
async fn wait_for_query_status(core: &DrasiLib, query_id: &str, expected: ComponentStatus) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(status) = core.get_query_status(query_id).await {
            if status == expected {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("query '{query_id}' did not reach {expected:?} in time");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Proves the full static-link path: descriptor DTO -> provider -> named
/// injection -> query evaluates against the RocksDB-backed index.
#[tokio::test]
async fn static_rocksdb_descriptor_drives_query() -> Result<()> {
    let data_dir = TempDir::new()?;

    // Standard `kind` + camelCase DTO storage-backend config, as it would appear
    // in a drasi-server configuration document.
    let backend_config = json!({
        "kind": "rocksdb",
        "path": data_dir.path().to_string_lossy(),
        "enableArchive": false,
    });

    let provider = build_provider_from_config(&backend_config).await;

    let (source, handle) = MockSource::new("people-src")?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("static-rocksdb")
            .with_index_provider("rocks-1", provider)
            .with_source(source)
            .with_query(person_query("rocks-1"))
            .build()
            .await?,
    );

    core.start().await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;

    let results = wait_for_results(&core, "people", 2).await;
    assert_eq!(
        results.len(),
        2,
        "query backed by RocksDB should return both rows, got: {results:?}"
    );
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(names.contains(&"Alice"), "missing Alice in {results:?}");
    assert!(names.contains(&"Bob"), "missing Bob in {results:?}");

    core.stop().await?;
    Ok(())
}

/// Proves the index data built by the statically-linked descriptor is genuinely
/// durable: after a query stop/start cycle, the prior results are recovered from
/// the persisted RocksDB index without re-ingesting any source events.
///
/// This uses the supported restart pattern — `stop_query`/`start_query` on the
/// same `DrasiLib` instance reusing the same provider. (A brand-new provider on
/// the same on-disk path within one process is not exercised here because
/// RocksDB holds a process-level exclusive lock on the data directory.)
#[tokio::test]
async fn static_rocksdb_index_persists_across_restart() -> Result<()> {
    let data_dir = TempDir::new()?;
    let backend_config = json!({
        "kind": "rocksdb",
        "path": data_dir.path().to_string_lossy(),
        "enableArchive": false,
    });

    let provider = build_provider_from_config(&backend_config).await;
    let (source, handle) = MockSource::new("people-src")?;
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("static-rocksdb")
            .with_index_provider("rocks-1", provider)
            .with_source(source)
            // Bootstrap disabled so that, after restart, the rows can only come
            // from the persisted RocksDB index — proving durability rather than
            // re-bootstrap.
            .with_query(
                Query::cypher("people")
                    .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
                    .from_source("people-src")
                    .auto_start(true)
                    .enable_bootstrap(false)
                    .with_storage_backend(StorageBackendRef::Named("rocks-1".to_string()))
                    .build(),
            )
            .build()
            .await?,
    );

    core.start().await?;

    // Ingest 3 rows as live source changes.
    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    insert_person(&handle, "p3", "Carol", 41).await?;
    let results = wait_for_results(&core, "people", 3).await;
    assert_eq!(results.len(), 3, "first run should ingest 3 rows");

    // Restart the query: in-memory state is discarded, the RocksDB index is
    // re-opened from disk.
    core.stop_query("people").await?;
    wait_for_query_status(&core, "people", ComponentStatus::Stopped).await;
    core.start_query("people").await?;

    // No re-ingestion: results must be recovered from the persisted index.
    let recovered = wait_for_results(&core, "people", 3).await;
    let names: Vec<&str> = recovered
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert_eq!(
        recovered.len(),
        3,
        "RocksDB-backed results should survive query restart without re-ingestion, got: {recovered:?}"
    );
    assert!(names.contains(&"Alice"), "missing Alice in {recovered:?}");
    assert!(names.contains(&"Bob"), "missing Bob in {recovered:?}");
    assert!(names.contains(&"Carol"), "missing Carol in {recovered:?}");

    core.stop().await?;
    Ok(())
}

/// Proves the fix for issue #627: after `DrasiLib::shutdown()`, the RocksDB
/// process-exclusive lock on the data directory is released, so a **brand-new**
/// `DrasiLib` + provider can reopen the **same** on-disk path **within the same
/// process** and recover the prior query state — without re-ingesting any source
/// events.
///
/// This is exactly the scenario the sibling `..._persists_across_restart` test
/// documents as previously unsupported: before the fix, the query runtime kept the
/// RocksDB handles alive for the whole `DrasiLib` lifetime, so reopening the path in
/// the same process failed with `LOCK: No locks available`.
///
/// The first engine is intentionally kept alive (held in an `Arc`) across the
/// reopen: the fix must free the lock on `shutdown()` itself, not rely on the whole
/// `DrasiLib` being dropped (internal Arcs can keep it alive past `shutdown()`).
#[tokio::test]
async fn static_rocksdb_shutdown_releases_lock_for_same_process_reopen() -> Result<()> {
    let data_dir = TempDir::new()?;
    let backend_config = json!({
        "kind": "rocksdb",
        "path": data_dir.path().to_string_lossy(),
        "enableArchive": false,
    });

    // ---- First engine: ingest rows, then shut down permanently. ----
    let provider1 = build_provider_from_config(&backend_config).await;
    let (source1, handle1) = MockSource::new("people-src")?;
    let core1 = Arc::new(
        DrasiLib::builder()
            .with_id("static-rocksdb-1")
            .with_index_provider("rocks-1", provider1)
            .with_source(source1)
            // Bootstrap disabled so that, after reopen, rows can only come from the
            // persisted RocksDB index — proving durability rather than re-bootstrap.
            .with_query(
                Query::cypher("people")
                    .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
                    .from_source("people-src")
                    .auto_start(true)
                    .enable_bootstrap(false)
                    .with_storage_backend(StorageBackendRef::Named("rocks-1".to_string()))
                    .build(),
            )
            .build()
            .await?,
    );

    core1.start().await?;

    insert_person(&handle1, "p1", "Alice", 30).await?;
    insert_person(&handle1, "p2", "Bob", 25).await?;
    insert_person(&handle1, "p3", "Carol", 41).await?;
    let results = wait_for_results(&core1, "people", 3).await;
    assert_eq!(results.len(), 3, "first engine should ingest 3 rows");

    // Permanent teardown — this must release the RocksDB lock even though `core1`
    // (and its provider) remain alive below.
    core1.shutdown().await?;

    // ---- Second engine: reopen the SAME path in the SAME process. ----
    // A fresh provider on the same on-disk path; this open acquires the RocksDB
    // LOCK, which only succeeds if the first engine's `shutdown()` released it.
    let provider2 = build_provider_from_config(&backend_config).await;
    let (source2, _handle2) = MockSource::new("people-src")?;
    let core2 = Arc::new(
        DrasiLib::builder()
            .with_id("static-rocksdb-2")
            .with_index_provider("rocks-1", provider2)
            .with_source(source2)
            .with_query(
                Query::cypher("people")
                    .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
                    .from_source("people-src")
                    .auto_start(true)
                    .enable_bootstrap(false)
                    .with_storage_backend(StorageBackendRef::Named("rocks-1".to_string()))
                    .build(),
            )
            .build()
            .await?,
    );

    // If the lock were still held, `start()` would fail while building the indexes
    // ("Failed to build indexes" / "No locks available").
    core2
        .start()
        .await
        .expect("second engine should open the same RocksDB path after shutdown released the lock");

    // No re-ingestion: results must be recovered from the persisted index.
    let recovered = wait_for_results(&core2, "people", 3).await;
    let names: Vec<&str> = recovered
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert_eq!(
        recovered.len(),
        3,
        "reopened engine should recover the persisted rows without re-ingestion, got: {recovered:?}"
    );
    assert!(names.contains(&"Alice"), "missing Alice in {recovered:?}");
    assert!(names.contains(&"Bob"), "missing Bob in {recovered:?}");
    assert!(names.contains(&"Carol"), "missing Carol in {recovered:?}");

    core2.shutdown().await?;

    // Keep the first engine alive until the very end so the test proves the lock was
    // freed by `shutdown()` rather than by dropping `core1`.
    drop(core1);
    Ok(())
}
/// instance-wide `persist_index` flag: a query with **no** `storage_backend` is
/// transparently backed by the default RocksDB provider (rather than falling back
/// to in-memory), and that data is durable across a query stop/start cycle.
#[tokio::test]
async fn static_rocksdb_default_provider_backs_unspecified_query() -> Result<()> {
    let data_dir = TempDir::new()?;
    let backend_config = json!({
        "kind": "rocksdb",
        "path": data_dir.path().to_string_lossy(),
        "enableArchive": false,
    });

    let provider = build_provider_from_config(&backend_config).await;
    let (source, handle) = MockSource::new("people-src")?;
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("static-rocksdb")
            // As drasi-server does when persist_index is enabled: register the
            // provider as the default for any query without an explicit backend.
            .with_default_index_provider("rocksdb", provider)
            .with_source(source)
            // Note: NO with_storage_backend — relies entirely on the default.
            .with_query(
                Query::cypher("people")
                    .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
                    .from_source("people-src")
                    .auto_start(true)
                    .enable_bootstrap(false)
                    .build(),
            )
            .build()
            .await?,
    );

    core.start().await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    let results = wait_for_results(&core, "people", 2).await;
    assert_eq!(
        results.len(),
        2,
        "query with no explicit backend should be served by the default RocksDB provider"
    );

    // Restart the query: with bootstrap disabled, recovered rows can only come
    // from the persisted RocksDB index — proving the default provider really is
    // persistent (not in-memory).
    core.stop_query("people").await?;
    wait_for_query_status(&core, "people", ComponentStatus::Stopped).await;
    core.start_query("people").await?;

    let recovered = wait_for_results(&core, "people", 2).await;
    let names: Vec<&str> = recovered
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert_eq!(
        recovered.len(),
        2,
        "default-backed results should survive restart without re-ingestion, got: {recovered:?}"
    );
    assert!(names.contains(&"Alice"), "missing Alice in {recovered:?}");
    assert!(names.contains(&"Bob"), "missing Bob in {recovered:?}");

    core.stop().await?;
    Ok(())
}
