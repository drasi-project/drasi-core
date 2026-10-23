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
//! into the currently-active session transaction (opened by
//! [`SessionControl::begin`](super::SessionControl)) and is committed by the
//! session's outer commit alongside the index updates. Other `stage_*` methods
//! have the same transactional semantics. Non-staging writes operate
//! outside a session transaction and commit independently.
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

/// Query-wide bootstrap boundary, independent of any source's opaque position.
///
/// `InProgress` must be committed before the first bootstrap index write.
/// `Complete` must commit atomically with every source's handover checkpoint,
/// the initial live-results snapshot and its result sequence. Missing metadata
/// is legacy/unknown state, not evidence that bootstrap completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapState {
    InProgress,
    Complete {
        /// Sources whose snapshots were indexed. Other sources can resume from
        /// sequence zero if they have not produced a checkpoint yet.
        sources: Vec<String>,
    },
}

/// Atomic checkpoint persistence for source sequence tracking and config hashing.
///
/// # Method semantics
///
/// - `stage_*` methods require an active session for persistent backends.
///   Writes persist at the outer `SessionControl::commit` alongside index
///   updates. For volatile (in-memory) backends, they apply immediately.
/// - Non-staging writes operate independently of the session transaction and
///   commit on their own.
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

    /// Stage the query-wide bootstrap state in the active index session.
    ///
    /// Persistent implementations must reject calls without an active session;
    /// volatile implementations apply immediately. There is deliberately no
    /// default: a no-op would permit recovery against partially built indexes.
    async fn stage_bootstrap_state(&self, state: BootstrapState) -> Result<(), IndexError>;

    /// Read committed bootstrap metadata, independently of any active session.
    ///
    /// `None` means no metadata exists (new or legacy storage). Unknown versions
    /// and invalid values must return an error, never `None`.
    async fn read_bootstrap_state(&self) -> Result<Option<BootstrapState>, IndexError>;

    /// Stage a source checkpoint into the active session transaction.
    ///
    /// For persistent backends, must be called inside an open session (between
    /// `SessionControl::begin` and `SessionControl::commit`). Returns an error
    /// if no session is active.
    ///
    /// For volatile backends (in-memory), applies immediately.
    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError>;

    /// Read the committed checkpoint for a single source.
    ///
    /// Returns `None` if no checkpoint has been written for `source_id`.
    /// Reads committed state directly; does not require an active session.
    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError>;

    /// Read all committed source checkpoints, keyed by source id.
    ///
    /// Returns an empty map if no checkpoints have been written.
    /// Reads committed state directly; does not require an active session.
    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError>;

    /// Delete all source checkpoints, bootstrap metadata and the config hash.
    ///
    /// Used during auto-reset recovery and `delete_query(cleanup: true)`.
    /// Standalone commit; not part of any outer session transaction.
    async fn clear_checkpoints(&self) -> Result<(), IndexError>;

    /// Write the query config hash.
    ///
    /// Used at startup to detect query configuration changes that require a
    /// full re-bootstrap. Standalone commit.
    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError>;

    /// Read the stored config hash.
    ///
    /// Returns `None` if no hash has been written. Called at startup before
    /// any session transaction begins.
    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError>;

    /// Stage the last persisted result sequence into the active session transaction.
    ///
    /// For persistent backends, must be called inside an open session (between
    /// `SessionControl::begin` and `SessionControl::commit`) together with outbox
    /// and live-results writes. The write is persisted by the outer commit.
    ///
    /// For volatile backends, applies immediately (same as
    /// [`write_result_sequence`](Self::write_result_sequence)).
    ///
    /// There is no default: every backend must choose staged-in-session vs
    /// immediate apply. A default that called
    /// [`write_result_sequence`](Self::write_result_sequence) would silently
    /// commit outside the outer transaction.
    async fn stage_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError>;

    /// Write the last persisted result sequence for a query.
    ///
    /// Standalone commit — does not require an active session. Used by wipe/reset
    /// paths. Event processing should call
    /// [`stage_result_sequence`](Self::stage_result_sequence) inside the outer
    /// session instead so the sequence commits atomically with outbox and live
    /// results.
    ///
    /// Default: no-op (volatile backends that don't track result sequences).
    async fn write_result_sequence(
        &self,
        _query_id: &str,
        _sequence: u64,
    ) -> Result<(), IndexError> {
        Ok(())
    }

    /// Read the last committed result sequence for a query.
    ///
    /// Returns `None` if no result sequence has been written.
    /// Used during crash recovery to determine the outbox replay point.
    ///
    /// Default: returns `None` (volatile backends).
    async fn read_result_sequence(&self, _query_id: &str) -> Result<Option<u64>, IndexError> {
        Ok(None)
    }

    /// Persist the query output generation.
    ///
    /// Bumped when output is wiped/rebuilt so reactions can distinguish a new
    /// sequence 1 from a previous generation. Default: no-op.
    async fn write_output_generation(
        &self,
        _query_id: &str,
        _generation: u64,
    ) -> Result<(), IndexError> {
        Ok(())
    }

    /// Read the persisted output generation.
    ///
    /// Returns `None` if none has been written (treated as generation 0).
    async fn read_output_generation(&self, _query_id: &str) -> Result<Option<u64>, IndexError> {
        Ok(None)
    }
}
