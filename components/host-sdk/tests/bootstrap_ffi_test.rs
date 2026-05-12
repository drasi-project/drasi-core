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
use std::sync::Arc;

use async_trait::async_trait;
use drasi_host_sdk::callbacks;
use drasi_host_sdk::loader::load_plugin_from_path;
use drasi_lib::queries::output_state::{
    FetchError, OutboxStream, SnapshotResponse, SnapshotStream,
};
use drasi_lib::reactions::bootstrap_context::{BootstrapBackend, BootstrapContext};
use drasi_lib::reactions::checkpoint::ReactionCheckpoint;
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

    async fn fetch_outbox(
        &self,
        _after_sequence: u64,
    ) -> Result<OutboxStream, FetchError> {
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
        panic!(
            "SKIP: snapshot-test plugin not found at {}. Build with: \
             cargo build --lib -p drasi-reaction-snapshot-test",
            plugin_path.display()
        );
    }

    let plugin = load_plugin_from_path(
        &plugin_path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )
    .expect("Failed to load snapshot-test plugin");

    assert_eq!(plugin.reaction_plugins.len(), 1, "Expected 1 reaction descriptor");

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

    let ctx = BootstrapContext::from_backend(
        "test-query".to_string(),
        false,
        Box::new(mock_backend),
    );

    // --- Call bootstrap() through FFI — exercises the full streaming path ---
    reaction
        .bootstrap(ctx)
        .await
        .expect("bootstrap() failed");

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
        panic!(
            "SKIP: snapshot-test plugin not found at {}",
            plugin_path.display()
        );
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
    let snapshot_rows = vec![
        serde_json::json!({"x": 1}),
        serde_json::json!({"x": 2}),
    ];

    let outbox_entries: Vec<Arc<drasi_lib::channels::QueryResult>> = vec![
        Arc::new(drasi_lib::channels::QueryResult::new(
            "test-query".to_string(),
            43,
            chrono::Utc::now(),
            vec![drasi_lib::channels::ResultDiff::Add {
                data: serde_json::json!({"x": 3}),
                row_signature: 300,
            }],
            std::collections::HashMap::new(),
        )),
    ];

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

    let ctx = BootstrapContext::from_backend(
        "test-query".to_string(),
        false,
        Box::new(mock_backend),
    );

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
