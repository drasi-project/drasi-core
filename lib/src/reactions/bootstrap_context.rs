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

//! Bootstrap context provided to reactions during startup recovery.
//!
//! The host constructs a `BootstrapContext` and passes it to
//! `Reaction::bootstrap()` when the reaction needs to (re-)synchronize
//! with a query's output state.

use std::sync::Arc;

use async_trait::async_trait;

use crate::queries::output_state::{FetchError, OutboxStream, SnapshotStream};
use crate::queries::Query;
use crate::reactions::checkpoint::ReactionCheckpoint;
use crate::state_store::StateStoreProvider;

/// Backend trait for `BootstrapContext` operations.
///
/// This allows `BootstrapContext` to be backed by either an in-process query
/// (normal case) or FFI callback trampolines (plugin case).
#[async_trait]
#[doc(hidden)]
pub trait BootstrapBackend: Send + Sync {
    async fn fetch_snapshot(&self) -> Result<SnapshotStream, FetchError>;
    async fn fetch_outbox(&self, after_sequence: u64) -> Result<OutboxStream, FetchError>;
    async fn read_checkpoint(&self) -> anyhow::Result<Option<ReactionCheckpoint>>;
    async fn write_checkpoint(&self, checkpoint: &ReactionCheckpoint) -> anyhow::Result<()>;
}

/// In-process backend: wraps a query instance and state store.
struct InProcessBackend {
    query: Arc<dyn Query>,
    reaction_id: String,
    query_id: String,
    state_store: Option<Arc<dyn StateStoreProvider>>,
}

#[async_trait]
impl BootstrapBackend for InProcessBackend {
    async fn fetch_snapshot(&self) -> Result<SnapshotStream, FetchError> {
        let snapshot = self.query.fetch_snapshot().await?;
        Ok(SnapshotStream::from_snapshot(snapshot))
    }

    async fn fetch_outbox(&self, after_sequence: u64) -> Result<OutboxStream, FetchError> {
        let outbox = self.query.fetch_outbox(after_sequence).await?;
        Ok(OutboxStream::from_outbox(outbox))
    }

    async fn read_checkpoint(&self) -> anyhow::Result<Option<ReactionCheckpoint>> {
        match self.state_store.as_ref() {
            Some(store) => {
                crate::reactions::checkpoint::read_checkpoint(
                    store.as_ref(),
                    &self.reaction_id,
                    &self.query_id,
                )
                .await
            }
            None => Ok(None),
        }
    }

    async fn write_checkpoint(&self, checkpoint: &ReactionCheckpoint) -> anyhow::Result<()> {
        let store = self
            .state_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No state store configured — cannot write checkpoint"))?;
        crate::reactions::checkpoint::write_checkpoint(
            store.as_ref(),
            &self.reaction_id,
            &self.query_id,
            checkpoint,
        )
        .await
    }
}

/// Context provided to `Reaction::bootstrap()` during startup recovery.
///
/// Exposes the query's `fetch_snapshot()` and `fetch_outbox()` APIs so the
/// reaction can synchronize its state, plus checkpoint read/write helpers.
///
/// # Contract
///
/// **This context is valid only for the duration of the `bootstrap()` call.**
/// Implementations MUST NOT store the `BootstrapContext` beyond the `bootstrap()`
/// method's return. The underlying backend may contain raw pointers (e.g., FFI
/// callback trampolines) that are invalidated when the call returns. Storing and
/// later using this context would cause undefined behavior.
///
/// Future versions may enforce this constraint with lifetime parameters.
pub struct BootstrapContext {
    /// The query ID this bootstrap is for.
    pub query_id: String,
    /// `true` when the reaction previously had a checkpoint that is being
    /// discarded (e.g., `AutoReset` recovery or config-hash mismatch).
    /// `false` on a fresh start when no prior checkpoint existed.
    pub is_reset: bool,
    /// The backend that provides the actual implementations.
    backend: Box<dyn BootstrapBackend>,
}

impl BootstrapContext {
    /// Create a new `BootstrapContext` backed by an in-process query.
    pub fn new(
        query_id: String,
        is_reset: bool,
        query: Arc<dyn Query>,
        reaction_id: String,
        state_store: Option<Arc<dyn StateStoreProvider>>,
    ) -> Self {
        let backend = InProcessBackend {
            query,
            reaction_id,
            query_id: query_id.clone(),
            state_store,
        };
        Self {
            query_id,
            is_reset,
            backend: Box::new(backend),
        }
    }

    /// Create a `BootstrapContext` backed by a custom backend.
    ///
    /// This is used by the FFI layer to provide callback-based implementations.
    #[doc(hidden)]
    pub fn from_backend(
        query_id: String,
        is_reset: bool,
        backend: Box<dyn BootstrapBackend>,
    ) -> Self {
        Self {
            query_id,
            is_reset,
            backend,
        }
    }

    /// Fetch a streaming snapshot of the query's live result set.
    ///
    /// Returns a `SnapshotStream` that yields rows one at a time.
    pub async fn fetch_snapshot(&self) -> Result<SnapshotStream, FetchError> {
        self.backend.fetch_snapshot().await
    }

    /// Fetch a streaming outbox after the given sequence number.
    ///
    /// Returns an `OutboxStream` that yields `QueryResult` entries one at a time.
    pub async fn fetch_outbox(&self, after_sequence: u64) -> Result<OutboxStream, FetchError> {
        self.backend.fetch_outbox(after_sequence).await
    }

    /// Read the persisted checkpoint for this query subscription.
    ///
    /// Returns `Ok(None)` if no checkpoint exists (fresh start) or no state
    /// store is configured.
    pub async fn read_checkpoint(&self) -> anyhow::Result<Option<ReactionCheckpoint>> {
        self.backend.read_checkpoint().await
    }

    /// Persist a checkpoint for this query subscription.
    pub async fn write_checkpoint(
        &self,
        checkpoint: &ReactionCheckpoint,
    ) -> anyhow::Result<()> {
        self.backend.write_checkpoint(checkpoint).await
    }
}
