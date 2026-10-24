// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use bytes::Bytes;

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
