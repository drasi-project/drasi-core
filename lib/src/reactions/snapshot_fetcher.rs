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

//! Runtime snapshot fetcher for reactions.
//!
//! This module provides the [`SnapshotFetcher`] trait, which allows reactions
//! to fetch query snapshots on-demand at any time during their lifecycle —
//! not just during bootstrap.
//!
//! # Architecture
//!
//! The trait is designed to be FFI-safe:
//!
//! - **In-process reactions** receive an [`InProcessSnapshotFetcher`] backed
//!   by the host's [`QueryProvider`].
//! - **cdylib plugin reactions** receive an FFI proxy (`FfiSnapshotFetcherProxy`)
//!   backed by a [`SnapshotFetcherVtable`] with `extern "C"` callbacks.
//!
//! Both sides use the same [`SnapshotStream`] return type, and the FFI layer
//! reuses the same streaming iterator protocol as bootstrap.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::channels::ComponentStatus;
use crate::queries::output_state::{FetchError, SnapshotStream};
use crate::reactions::QueryProvider;

/// Trait for fetching query snapshots on-demand at runtime.
///
/// Reactions receive an `Arc<dyn SnapshotFetcher>` via [`ReactionRuntimeContext`]
/// and can call `fetch_snapshot()` at any time to get the current result set
/// of a subscribed query.
///
/// # Scoping
///
/// The fetcher is scoped to the reaction's configured query IDs. Attempting
/// to fetch a snapshot for a query the reaction is not subscribed to returns
/// `FetchError::NotRunning`.
#[async_trait]
pub trait SnapshotFetcher: Send + Sync {
    /// Fetch the current result set of the given query as a streaming snapshot.
    ///
    /// Returns a [`SnapshotStream`] that yields rows one at a time, along with
    /// the `as_of_sequence` and `config_hash` metadata.
    ///
    /// # Errors
    ///
    /// - `FetchError::NotRunning` if the query is not running or not found
    /// - `FetchError::TimedOut` if the query's bootstrap hasn't completed in time
    async fn fetch_snapshot(&self, query_id: &str) -> Result<SnapshotStream, FetchError>;
}

/// In-process snapshot fetcher backed by a [`QueryProvider`].
///
/// Used for reactions running in the same process (not across FFI).
/// Wraps a lazily-resolved `QueryProvider` behind a `RwLock` so it can
/// be created at provision time (before `QueryProvider` is injected) and
/// resolved at call time.
pub(crate) struct InProcessSnapshotFetcher {
    query_provider: Arc<RwLock<Option<Arc<dyn QueryProvider>>>>,
    allowed_queries: HashSet<String>,
}

impl InProcessSnapshotFetcher {
    /// Create a new in-process snapshot fetcher.
    ///
    /// The `query_provider` may be `None` at construction time and resolved
    /// lazily when `fetch_snapshot` is called (after `inject_query_provider`).
    pub fn new(
        query_provider: Arc<RwLock<Option<Arc<dyn QueryProvider>>>>,
        allowed_queries: Vec<String>,
    ) -> Self {
        Self {
            query_provider,
            allowed_queries: allowed_queries.into_iter().collect(),
        }
    }
}

#[async_trait]
impl SnapshotFetcher for InProcessSnapshotFetcher {
    async fn fetch_snapshot(&self, query_id: &str) -> Result<SnapshotStream, FetchError> {
        if !self.allowed_queries.contains(query_id) {
            return Err(FetchError::NotRunning {
                status: ComponentStatus::Error,
            });
        }

        let provider = self.query_provider.read().await;
        let provider = provider.as_ref().ok_or(FetchError::NotRunning {
            status: ComponentStatus::Error,
        })?;

        let query =
            provider
                .get_query_instance(query_id)
                .await
                .map_err(|_| FetchError::NotRunning {
                    status: ComponentStatus::Error,
                })?;

        let snapshot = query.fetch_snapshot().await?;
        Ok(SnapshotStream::from_snapshot(snapshot))
    }
}
