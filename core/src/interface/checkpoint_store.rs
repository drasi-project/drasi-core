// Copyright 2024 The Drasi Authors.
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

//! Checkpoint Store Trait
//!
//! Atomic checkpoint persistence for source sequence tracking, opaque source
//! position bytes (for native stream resumption), and config hashing.
//!
//! Implementations are paired with an [`IndexBackendPlugin`](super::IndexBackendPlugin)
//! and share the same session state. [`CheckpointStore::stage_checkpoint`] writes
//! into the active [`SessionGuard`](super::SessionGuard) root and commits with
//! the index updates. Transaction-capable providers also stage clears, config
//! hashes and result sequences in that root.
//!
//! This trait lives in core (not lib) so that index plugins can implement it
//! without taking a reverse dependency on `drasi-lib`.

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;

use super::IndexError;

/// Per-source checkpoint data.
///
/// Contains the monotonic sequence number and an optional opaque position
/// that the source interprets to seek back into its native change stream
/// (e.g., Postgres LSN, Kafka offset, EventHub sequence number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceCheckpoint {
    pub sequence: u64,
    /// Opaque position bytes for native stream resumption.
    /// Sources interpret these to seek back into their change stream.
    pub source_position: Option<Bytes>,
}

impl SourceCheckpoint {
    pub fn new(sequence: u64, source_position: Option<Bytes>) -> Self {
        Self {
            sequence,
            source_position,
        }
    }
}

/// Atomic checkpoint persistence for source sequence tracking and config hashing.
///
/// # Method semantics
///
/// - [`stage_checkpoint`](Self::stage_checkpoint) requires an active
///   [`SessionGuard`](super::SessionGuard) root for persistent backends.
///   The write is staged into the active session transaction and persisted by the
///   outer commit. For volatile (in-memory) backends, it applies immediately.
/// - Transaction-capable providers stage other writes and clears in an active
///   root, and their reads include staged changes. Without a root, these methods
///   operate on committed state.
/// - Callers must mark nontransactional roots dirty before administrative writes
///   or clears, so an aborted reset requires rebuilding rather than retrying.
///
/// # Source positions
///
/// Each source feeding a query has its own checkpoint entry. The opaque
/// `source_position` bytes allow native stream resumption — on restart, the
/// query reads each source's position and passes it via `resume_from`.
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    /// Whether this store persists checkpoints across process restarts.
    ///
    /// The orchestration layer uses this to decide whether to propagate
    /// `resume_from` to sources on restart. Volatile
    /// (in-memory) stores return `false`; persistent stores (RocksDB, Garnet)
    /// return `true`.
    fn is_persistent(&self) -> bool;

    /// Stage a source checkpoint into the active session transaction.
    ///
    /// For persistent backends, must be called inside an open root.
    /// Returns an error if no session is active.
    ///
    /// For volatile backends (in-memory), applies immediately.
    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError>;

    /// Read the checkpoint for a single source.
    ///
    /// Returns `None` if no checkpoint has been written for `source_id`.
    /// Includes staged changes in an active root; otherwise reads committed state.
    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError>;

    /// Read all source checkpoints, keyed by source id.
    ///
    /// Returns an empty map if no checkpoints have been written.
    /// Includes staged changes in an active root; otherwise reads committed state.
    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError>;

    /// Delete all source checkpoints and the config hash.
    ///
    /// Used during auto-reset recovery and `delete_query(cleanup: true)`.
    /// Transaction-capable providers stage this clear in the active root.
    /// The persisted result sequence is preserved.
    async fn clear_checkpoints(&self) -> Result<(), IndexError>;

    /// Write the query config hash.
    ///
    /// Used at startup to detect query configuration changes that require a
    /// full re-bootstrap. Participates in an active root.
    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError>;

    /// Read the stored config hash.
    ///
    /// Returns `None` if no hash has been written. Includes staged changes
    /// when called inside a root.
    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError>;

    /// Write the last persisted result sequence for a query.
    ///
    /// Stage alongside outbox, live-result and source-checkpoint writes before
    /// committing the root. Records the highest persisted output sequence so
    /// recovery can detect outbox gaps.
    ///
    /// Default: no-op (volatile backends that don't track result sequences).
    async fn write_result_sequence(
        &self,
        _query_id: &str,
        _sequence: u64,
    ) -> Result<(), IndexError> {
        Ok(())
    }

    /// Read the last persisted result sequence for a query.
    ///
    /// Returns `None` if no result sequence has been written.
    /// Used during crash recovery to determine the outbox replay point.
    /// Includes staged changes when called inside a root.
    ///
    /// Default: returns `None` (volatile backends).
    async fn read_result_sequence(&self, _query_id: &str) -> Result<Option<u64>, IndexError> {
        Ok(None)
    }
}
