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

//! Integration tests for the FFI streaming bootstrap protocol.
//!
//! These tests load the `drasi-reaction-snapshot-test` cdylib and exercise the
//! full FFI bootstrap round-trip: host creates snapshot/outbox iterators,
//! the plugin streams rows one-at-a-time via `next_fn` callbacks, and verifies
//! data integrity.
//!
//! **Prerequisites**: Build the snapshot-test reaction before running:
//!
//! ```sh
//! cargo build --lib -p drasi-reaction-snapshot-test
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use drasi_host_sdk::callbacks;
use drasi_host_sdk::loader::load_plugin_from_path;
use drasi_lib::queries::output_state::{
    FetchError, OutboxStream, SnapshotResponse, SnapshotStream,
};
use drasi_lib::reactions::bootstrap_context::{BootstrapBackend, BootstrapContext};
use drasi_lib::reactions::checkpoint::ReactionCheckpoint;
use drasi_lib::reactions::SnapshotFetcher;
use drasi_plugin_sdk::descriptor::ReactionPluginDescriptor;
use tokio::sync::Mutex;

/// Locate the workspace target/debug directory.
fn target_debug_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("Cannot find workspace root");
    workspace_root.join("target").join("debug")
}

/// Get the platform-specific .so/.dylib filename.
fn plugin_filename(crate_name: &str) -> String {
    let lib_name = crate_name.replace('-', "_");
    if cfg!(target_os = "macos") {
        format!("lib{lib_name}.dylib")
    } else {
        format!("lib{lib_name}.so")
    }
}

/// Mock bootstrap backend with canned snapshot and outbox data.
struct MockBootstrapBackend {
    snapshot_rows: Vec<serde_json::Value>,
    snapshot_sequence: u64,
    snapshot_config_hash: u64,
    outbox_entries: Vec<Arc<drasi_lib::channels::QueryResult>>,
    outbox_latest_sequence: u64,
    outbox_config_hash: u64,
    written_checkpoint: Arc<Mutex<Option<ReactionCheckpoint>>>,
}

#[async_trait]
impl BootstrapBackend for MockBootstrapBackend {
    async fn fetch_snapshot(&self) -> Result<SnapshotStream, FetchError> {
        // Construct a SnapshotResponse from canned data, then convert to stream.
        let mut results = im::HashMap::new();
        for (i, v) in self.snapshot_rows.iter().enumerate() {
            results.insert(i as u64, v.clone());
        }
        let snapshot =
            SnapshotResponse::new(results, self.snapshot_sequence, self.snapshot_config_hash);
        Ok(SnapshotStream::from_snapshot(snapshot))
    }

    async fn fetch_outbox(&self, _after_sequence: u64) -> Result<OutboxStream, FetchError> {
        Ok(OutboxStream::from_outbox(
            drasi_lib::queries::output_state::OutboxResponse {
                results: self.outbox_entries.clone(),
                latest_sequence: self.outbox_latest_sequence,
                config_hash: self.outbox_config_hash,
            },
        ))
    }

    async fn read_checkpoint(&self) -> anyhow::Result<Option<ReactionCheckpoint>> {
        Ok(None) // No prior checkpoint
    }

    async fn write_checkpoint(&self, checkpoint: &ReactionCheckpoint) -> anyhow::Result<()> {
        let mut guard = self.written_checkpoint.lock().await;
        *guard = Some(checkpoint.clone());
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_ffi_streaming_bootstrap() {
    // --- Load the snapshot-test cdylib ---
    let plugin_path = target_debug_dir().join(plugin_filename("drasi-reaction-snapshot-test"));
    if !plugin_path.exists() {
        eprintln!(
            "SKIP: snapshot-test plugin not found at {}. Build with: \
             cargo build --lib -p drasi-reaction-snapshot-test",
            plugin_path.display()
        );
        return;
    }

    let plugin = load_plugin_from_path(
        &plugin_path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Failed to load snapshot-test plugin");

    assert_eq!(
        plugin.reaction_plugins.len(),
        1,
        "Expected 1 reaction descriptor"
    );

    let descriptor = &plugin.reaction_plugins[0];
    assert_eq!(descriptor.kind(), "snapshot-test");

    // --- Create a reaction instance via the FFI descriptor ---
    let reaction = descriptor
        .create_reaction(
            "test-reaction-1",
            vec!["test-query".to_string()],
            &serde_json::json!({}),
            false,
        )
        .await
        .expect("Failed to create reaction via FFI");

    assert_eq!(reaction.id(), "test-reaction-1");
    assert_eq!(reaction.type_name(), "snapshot-test");
    assert!(reaction.is_durable());
    assert!(reaction.needs_snapshot_on_fresh_start());

    // --- Set up mock backend with canned data ---
    let snapshot_rows = vec![
        serde_json::json!({"name": "Alice", "age": 30}),
        serde_json::json!({"name": "Bob", "age": 25}),
        serde_json::json!({"name": "Charlie", "age": 35}),
    ];

    let written_checkpoint = Arc::new(Mutex::new(None));

    let mock_backend = MockBootstrapBackend {
        snapshot_rows: snapshot_rows.clone(),
        snapshot_sequence: 42,
        snapshot_config_hash: 12345,
        outbox_entries: vec![],
        outbox_latest_sequence: 42,
        outbox_config_hash: 12345,
        written_checkpoint: written_checkpoint.clone(),
    };

    let ctx =
        BootstrapContext::from_backend("test-query".to_string(), false, Box::new(mock_backend));

    // --- Call bootstrap() through FFI — exercises the full streaming path ---
    reaction.bootstrap(ctx).await.expect("bootstrap() failed");

    // --- Verify checkpoint was written ---
    let cp = written_checkpoint.lock().await;
    let cp = cp.as_ref().expect("Checkpoint should have been written");
    assert_eq!(cp.sequence, 42, "Checkpoint sequence mismatch");
    assert_eq!(cp.config_hash, 12345, "Checkpoint config_hash mismatch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_ffi_streaming_bootstrap_with_outbox() {
    // --- Load the snapshot-test cdylib ---
    let plugin_path = target_debug_dir().join(plugin_filename("drasi-reaction-snapshot-test"));
    if !plugin_path.exists() {
        eprintln!(
            "SKIP: snapshot-test plugin not found at {}",
            plugin_path.display()
        );
        return;
    }

    let plugin = load_plugin_from_path(
        &plugin_path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Failed to load snapshot-test plugin");

    let descriptor = &plugin.reaction_plugins[0];

    let reaction = descriptor
        .create_reaction(
            "test-reaction-2",
            vec!["test-query".to_string()],
            &serde_json::json!({}),
            false,
        )
        .await
        .expect("Failed to create reaction via FFI");

    // --- Set up mock backend with snapshot + outbox data ---
    let snapshot_rows = vec![serde_json::json!({"x": 1}), serde_json::json!({"x": 2})];

    let outbox_entries: Vec<Arc<drasi_lib::channels::QueryResult>> =
        vec![Arc::new(drasi_lib::channels::QueryResult::new(
            "test-query".to_string(),
            43,
            chrono::Utc::now(),
            vec![drasi_lib::channels::ResultDiff::Add {
                data: serde_json::json!({"x": 3}),
                row_signature: 300,
            }],
            std::collections::HashMap::new(),
        ))];

    let written_checkpoint = Arc::new(Mutex::new(None));

    let mock_backend = MockBootstrapBackend {
        snapshot_rows,
        snapshot_sequence: 42,
        snapshot_config_hash: 99999,
        outbox_entries,
        outbox_latest_sequence: 43,
        outbox_config_hash: 99999,
        written_checkpoint: written_checkpoint.clone(),
    };

    let ctx =
        BootstrapContext::from_backend("test-query".to_string(), false, Box::new(mock_backend));

    // --- Call bootstrap() — exercises both snapshot AND outbox streaming ---
    reaction
        .bootstrap(ctx)
        .await
        .expect("bootstrap() with outbox failed");

    // --- Verify checkpoint was written with snapshot sequence ---
    let cp = written_checkpoint.lock().await;
    let cp = cp.as_ref().expect("Checkpoint should have been written");
    assert_eq!(cp.sequence, 42, "Checkpoint should match snapshot sequence");
    assert_eq!(cp.config_hash, 99999, "Checkpoint config_hash mismatch");
}

// ============================================================================
// SnapshotFetcher vtable path tests (Site 1)
// ============================================================================

/// Mock SnapshotFetcher that lives on the host side and tracks calls.
struct MockSnapshotFetcher {
    rows: Vec<serde_json::Value>,
    sequence: u64,
    config_hash: u64,
    fetch_count: AtomicUsize,
    rows_yielded: Arc<AtomicUsize>,
    captured_query_id: Mutex<Option<String>>,
}

#[async_trait]
impl SnapshotFetcher for MockSnapshotFetcher {
    async fn fetch_snapshot(&self, query_id: &str) -> Result<SnapshotStream, FetchError> {
        self.fetch_count.fetch_add(1, Ordering::SeqCst);
        {
            let mut guard = self.captured_query_id.lock().await;
            *guard = Some(query_id.to_string());
        }

        let rows_yielded = self.rows_yielded.clone();
        let rows = self.rows.clone();
        let sequence = self.sequence;
        let config_hash = self.config_hash;

        let stream = async_stream::stream! {
            for row in rows {
                rows_yielded.fetch_add(1, Ordering::SeqCst);
                yield row;
            }
        };

        Ok(SnapshotStream::from_stream(stream, sequence, config_hash))
    }
}

/// Test the SnapshotFetcher vtable path end-to-end across a real cdylib FFI boundary.
///
/// This exercises Site 1 (snapshot_fetcher_bridge.rs): the host builds a
/// SnapshotFetcherVtable from the mock, passes it to the plugin during
/// initialize(), and the plugin calls fetch_snapshot() during start().
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_ffi_snapshot_fetcher_vtable() {
    // --- Load the snapshot-test cdylib ---
    let plugin_path = target_debug_dir().join(plugin_filename("drasi-reaction-snapshot-test"));
    if !plugin_path.exists() {
        eprintln!(
            "SKIP: snapshot-test plugin not found at {}",
            plugin_path.display()
        );
        return;
    }

    let plugin = load_plugin_from_path(
        &plugin_path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Failed to load snapshot-test plugin");

    let descriptor = &plugin.reaction_plugins[0];

    let reaction = descriptor
        .create_reaction(
            "vtable-test-1",
            vec!["test-query".to_string()],
            &serde_json::json!({}),
            false,
        )
        .await
        .expect("Failed to create reaction via FFI");

    // --- Set up mock fetcher with canned data ---
    let mock_fetcher = Arc::new(MockSnapshotFetcher {
        rows: vec![
            serde_json::json!({"id": 1, "name": "Alice"}),
            serde_json::json!({"id": 2, "name": "Bob"}),
            serde_json::json!({"id": 3, "name": "Charlie"}),
            serde_json::json!({"id": 4, "name": "Diana"}),
            serde_json::json!({"id": 5, "name": "Eve"}),
        ],
        sequence: 100,
        config_hash: 55555,
        fetch_count: AtomicUsize::new(0),
        rows_yielded: Arc::new(AtomicUsize::new(0)),
        captured_query_id: Mutex::new(None),
    });

    // --- Initialize with the mock fetcher ---
    let (update_tx, _update_rx) =
        tokio::sync::mpsc::channel::<drasi_lib::component_graph::ComponentUpdate>(16);
    let context = drasi_lib::ReactionRuntimeContext {
        instance_id: "vtable-test-instance".to_string(),
        reaction_id: "vtable-test-1".to_string(),
        update_tx,
        state_store: None,
        identity_provider: None,
        snapshot_fetcher: Some(mock_fetcher.clone()),
    };
    reaction.initialize(context).await;

    // --- Start the reaction — this triggers vtable fetch_snapshot ---
    reaction.start().await.expect("Reaction start() failed");

    // --- Verify host-side observables ---
    assert_eq!(
        mock_fetcher.fetch_count.load(Ordering::SeqCst),
        1,
        "fetch_snapshot should have been called exactly once"
    );
    assert_eq!(
        mock_fetcher.rows_yielded.load(Ordering::SeqCst),
        5,
        "All 5 rows should have been streamed through the FFI boundary"
    );
    let captured_qid = mock_fetcher.captured_query_id.lock().await;
    assert_eq!(
        captured_qid.as_deref(),
        Some("test-query"),
        "Plugin should have fetched snapshot for 'test-query'"
    );

    // Clean up
    reaction.stop().await.expect("Reaction stop() failed");
}
