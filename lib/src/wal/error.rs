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

//! Error types for the Write-Ahead Log plugin contract.

use thiserror::Error;

/// Errors that can occur during WAL operations.
///
/// All variants include source_id context where relevant, since a single
/// `WalProvider` instance typically serves multiple sources.
#[derive(Debug, Error)]
pub enum WalError {
    /// The WAL for the given source has reached its configured maximum event capacity
    /// and the policy is [`CapacityPolicy::RejectIncoming`](super::CapacityPolicy::RejectIncoming).
    #[error("WAL capacity exhausted for source '{0}'")]
    CapacityExhausted(String),

    /// The requested replay position has been pruned and is older than the
    /// oldest retained sequence. Reading from a sequence greater than `head`
    /// is not an error — it returns an empty result (the caller is caught up).
    #[error("Position {requested} unavailable for source '{source_id}' (oldest available: {oldest_available:?})")]
    PositionUnavailable {
        source_id: String,
        requested: u64,
        oldest_available: Option<u64>,
    },

    /// Called a WAL operation for a source that was never registered.
    #[error("Source '{0}' is not registered with the WAL provider")]
    SourceNotRegistered(String),

    /// Attempted to register a source that is already registered with a different config.
    #[error("Source '{0}' is already registered with a different config")]
    SourceAlreadyRegistered(String),

    /// The supplied source_id is not acceptable to this provider (e.g., contains
    /// path separators, `..`, or other characters a file-backed provider cannot
    /// safely map to disk).
    #[error("Invalid source_id '{0}': {1}")]
    InvalidSourceId(String, String),

    /// The event is not valid for WAL storage (e.g., `SourceChange::Future`).
    #[error("Invalid event: {0}")]
    InvalidEvent(String),

    /// Configuration validation failed (e.g., `max_events` below `MIN_MAX_EVENTS`).
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    /// An error in the underlying storage backend.
    #[error("Storage error: {0}")]
    StorageError(String),

    /// An error during event serialization or deserialization.
    #[error("Serialization error: {0}")]
    SerializationError(String),
}
