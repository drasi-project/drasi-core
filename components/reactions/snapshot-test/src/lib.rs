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

//! Test reaction that exercises the streaming FFI bootstrap and snapshot
//! fetcher vtable paths.
//!
//! This crate compiles as a cdylib and exercises:
//! - All 4 `BootstrapContext` callbacks through the FFI boundary
//! - The `SnapshotFetcher` vtable path (injected via `ReactionRuntimeContext`)
//!
//! During `start()`, if a `SnapshotFetcher` was provided at `initialize()`,
//! the reaction fetches a snapshot for the first query ID and streams all rows.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::QueryResult;
use drasi_lib::reactions::bootstrap_context::BootstrapContext;
use drasi_lib::reactions::{Reaction, SnapshotFetcher};
use drasi_lib::{ComponentStatus, ReactionRuntimeContext};
use drasi_plugin_sdk::descriptor::ReactionPluginDescriptor;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_stream::StreamExt;
use utoipa::ToSchema;

mod descriptor;

/// Captured bootstrap results for test verification.
#[derive(Debug, Default)]
pub struct BootstrapReport {
    pub snapshot_rows: Vec<serde_json::Value>,
    pub snapshot_sequence: u64,
    pub snapshot_config_hash: u64,
    pub outbox_entries: Vec<Arc<QueryResult>>,
    pub outbox_latest_sequence: u64,
    pub outbox_config_hash: u64,
    pub checkpoint_found: bool,
    pub checkpoint_written: bool,
}

pub struct SnapshotTestReaction {
    id: String,
    query_ids: Vec<String>,
    status: Mutex<ComponentStatus>,
    report: Arc<Mutex<BootstrapReport>>,
    snapshot_fetcher: Mutex<Option<Arc<dyn SnapshotFetcher>>>,
}

impl SnapshotTestReaction {
    pub fn new(id: &str, query_ids: Vec<String>) -> (Self, Arc<Mutex<BootstrapReport>>) {
        let report = Arc::new(Mutex::new(BootstrapReport::default()));
        let reaction = Self {
            id: id.to_string(),
            query_ids,
            status: Mutex::new(ComponentStatus::Added),
            report: report.clone(),
            snapshot_fetcher: Mutex::new(None),
        };
        (reaction, report)
    }
}

#[async_trait]
impl Reaction for SnapshotTestReaction {
    fn id(&self) -> &str {
        &self.id
    }

    fn type_name(&self) -> &str {
        "snapshot-test"
    }

    fn query_ids(&self) -> Vec<String> {
        self.query_ids.clone()
    }

    fn auto_start(&self) -> bool {
        false
    }

    fn is_durable(&self) -> bool {
        true
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        true
    }

    fn default_recovery_policy(&self) -> drasi_lib::recovery::ReactionRecoveryPolicy {
        drasi_lib::recovery::ReactionRecoveryPolicy::AutoReset
    }

    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        std::collections::HashMap::new()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        // Store the snapshot fetcher (if provided) for use during start().
        let mut guard = self.snapshot_fetcher.lock().await;
        *guard = context.snapshot_fetcher;
    }

    async fn start(&self) -> Result<()> {
        // If a SnapshotFetcher was injected, exercise it by fetching and
        // streaming all rows. This validates the vtable FFI path (Site 1).
        let fetcher = self.snapshot_fetcher.lock().await.clone();
        if let Some(fetcher) = fetcher {
            if let Some(query_id) = self.query_ids.first() {
                let snapshot = fetcher
                    .fetch_snapshot(query_id)
                    .await
                    .map_err(|e| anyhow::anyhow!("vtable fetch_snapshot failed: {e}"))?;

                let mut report = self.report.lock().await;
                report.snapshot_sequence = snapshot.as_of_sequence;
                report.snapshot_config_hash = snapshot.config_hash;

                let mut stream = Box::pin(snapshot);
                while let Some(row) = stream.next().await {
                    report.snapshot_rows.push(row);
                }
            }
        }

        *self.status.lock().await = ComponentStatus::Running;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        *self.status.lock().await = ComponentStatus::Stopped;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        *self.status.lock().await
    }

    async fn enqueue_query_result(&self, _result: QueryResult) -> Result<()> {
        Ok(())
    }

    async fn bootstrap(&self, ctx: BootstrapContext) -> Result<()> {
        let mut report = self.report.lock().await;

        // 1. Read checkpoint
        match ctx.read_checkpoint().await {
            Ok(Some(_cp)) => report.checkpoint_found = true,
            Ok(None) => report.checkpoint_found = false,
            Err(e) => log::error!("read_checkpoint failed: {e}"),
        }

        // 2. Fetch snapshot — stream row by row
        let snapshot = ctx
            .fetch_snapshot()
            .await
            .map_err(|e| anyhow::anyhow!("fetch_snapshot failed: {e}"))?;
        report.snapshot_sequence = snapshot.as_of_sequence;
        report.snapshot_config_hash = snapshot.config_hash;

        // Pin the stream and consume it
        let mut stream = Box::pin(snapshot);
        while let Some(row) = stream.next().await {
            report.snapshot_rows.push(row);
        }

        // 3. Fetch outbox — stream entry by entry
        let outbox = ctx
            .fetch_outbox(0)
            .await
            .map_err(|e| anyhow::anyhow!("fetch_outbox failed: {e}"))?;
        report.outbox_latest_sequence = outbox.latest_sequence;
        report.outbox_config_hash = outbox.config_hash;

        let mut stream = Box::pin(outbox);
        while let Some(entry) = stream.next().await {
            report.outbox_entries.push(entry);
        }

        // 4. Write checkpoint
        let cp = drasi_lib::reactions::checkpoint::ReactionCheckpoint {
            sequence: report.snapshot_sequence,
            config_hash: report.snapshot_config_hash,
        };
        ctx.write_checkpoint(&cp).await?;
        report.checkpoint_written = true;

        Ok(())
    }
}

/// Empty config DTO.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SnapshotTestConfig {}

/// Plugin descriptor for the snapshot-test reaction.
pub use descriptor::SnapshotTestReactionDescriptor;

// Export the plugin via FFI when compiled as cdylib.
drasi_plugin_sdk::export_plugin!(
    plugin_id = "snapshot-test-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [SnapshotTestReactionDescriptor],
    bootstrap_descriptors = [],
);
