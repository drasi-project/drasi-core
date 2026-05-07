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
//! session's outer commit alongside the index updates. All other methods operate
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

/// Atomic checkpoint persistence for source sequence tracking and config hashing.
///
/// # Method semantics
///
/// - [`stage_checkpoint`](Self::stage_checkpoint) SHOULD be called between
///   `SessionControl::begin` and `SessionControl::commit` for persistent backends.
///   The write is staged into the active session transaction and persisted by the
///   outer commit. For volatile (in-memory) backends, it applies immediately.
/// - All other methods operate independently of the session transaction and
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
    /// `resume_from` and `last_sequence` to sources on restart. Volatile
    /// (in-memory) stores return `false`; persistent stores (RocksDB, Garnet)
    /// return `true`.
    fn is_persistent(&self) -> bool;

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

    /// Delete all source checkpoints and the config hash.
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
}
