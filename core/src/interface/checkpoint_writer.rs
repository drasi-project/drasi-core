// Copyright 2026 The Drasi Authors.
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

//! Checkpoint Writer Trait
//!
//! Atomic checkpoint persistence for source sequence tracking and config hashing.
//!
//! Implementations are paired with an [`IndexBackendPlugin`](super::IndexBackendPlugin)
//! and share the same session state. [`CheckpointWriter::stage_checkpoint`] writes
//! into the currently-active session transaction (opened by
//! [`SessionControl::begin`](super::SessionControl)) and is committed by the
//! session's outer commit alongside the index updates. All other methods operate
//! outside a session transaction and commit independently.
//!
//! This trait lives in core (not lib) so that index plugins can implement it
//! without taking a reverse dependency on `drasi-lib`. The orchestration logic
//! that consumes the writer (deciding when to call `stage_checkpoint`, wrapping
//! `process_source_change` in an outer transaction, etc.) lives in `drasi-lib`.

use async_trait::async_trait;
use std::collections::HashMap;

use super::IndexError;

/// Atomic checkpoint persistence for source sequence tracking and config hashing.
///
/// See the [module-level documentation](self) for usage context.
///
/// # Method semantics
///
/// - [`stage_checkpoint`](Self::stage_checkpoint) MUST be called between
///   `SessionControl::begin` and `SessionControl::commit`. The write is staged
///   into the active session transaction and persisted by the outer commit.
/// - All other methods operate independently of the session transaction and
///   commit on their own.
///
/// # Encoding
///
/// `sequence` and `hash` are `u64` values. Implementations choose a backend-
/// idiomatic encoding (RocksDB uses big-endian raw bytes; Garnet uses decimal
/// ASCII strings). The encoding is internal to each implementation.
#[async_trait]
pub trait CheckpointWriter: Send + Sync {
    /// Stage a source checkpoint into the active session transaction.
    ///
    /// Must be called inside an open session (between `SessionControl::begin`
    /// and `SessionControl::commit`). Returns an error if no session is active.
    async fn stage_checkpoint(&self, source_id: &str, sequence: u64) -> Result<(), IndexError>;

    /// Read the committed checkpoint for a single source.
    ///
    /// Returns `None` if no checkpoint has been written for `source_id`.
    /// Reads committed state directly; does not require an active session.
    async fn read_checkpoint(&self, source_id: &str) -> Result<Option<u64>, IndexError>;

    /// Read all committed source checkpoints, keyed by source id.
    ///
    /// Returns an empty map if no checkpoints have been written.
    /// Reads committed state directly; does not require an active session.
    async fn read_all_checkpoints(&self) -> Result<HashMap<String, u64>, IndexError>;

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
