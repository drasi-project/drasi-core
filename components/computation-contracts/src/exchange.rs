// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Shared source/query exchange data; no subscription or component ownership.

use crate::profiling::ProfilingMetadata;
use bytes::Bytes;
use drasi_core_types::models::SourceChange;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

/// Control events from sources for query coordination
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SourceControl {
    /// Query subscription control event
    Subscription {
        query_id: String,
        query_node_id: String,
        node_labels: Vec<String>,
        rel_labels: Vec<String>,
        operation: ControlOperation,
    },
    /// Signal from FutureQueueSource that one or more future items are due.
    FuturesDue,
}

/// Control operation types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ControlOperation {
    Insert,
    Update,
    Delete,
}

/// Unified event envelope carrying both data changes and control messages
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SourceEvent {
    /// Data change event from source
    Change(SourceChange),
    /// Control event for query coordination
    Control(SourceControl),
}

/// Source event envelope with a required source-local sequence number.
///
/// Sources may supply their own monotonically increasing sequence or allocate
/// one with `SourceBase::next_sequence`. The type guarantees that a sequence
/// is present, not that its value was assigned by the framework.
///
#[derive(Debug, Clone)]
pub struct SourceEventWrapper {
    pub source_id: String,
    pub event: SourceEvent,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Optional profiling metadata for performance tracking.
    pub profiling: Option<ProfilingMetadata>,
    /// Opaque source position bytes for stream resumption on restart.
    /// Only the source can interpret these bytes — the framework persists
    /// them alongside the assigned sequence and returns them on restart via
    /// `subscribe(resume_from: ...)`. `None` for volatile sources that don't
    /// support replay.
    pub source_position: Option<Bytes>,
    /// Source-local sequence used for watermarks, gap detection, and dedup.
    /// Live events must be dispatched in increasing sequence order, starting
    /// at one. Replay preserves the original sequence.
    pub sequence: u64,
}

impl SourceEventWrapper {
    /// Create an event with an explicit sequence and without profiling.
    pub fn new(
        source_id: String,
        event: SourceEvent,
        timestamp: chrono::DateTime<chrono::Utc>,
        sequence: u64,
    ) -> Self {
        Self {
            source_id,
            event,
            timestamp,
            profiling: None,
            source_position: None,
            sequence,
        }
    }

    /// Create an event with an explicit sequence and profiling metadata.
    pub fn with_profiling(
        source_id: String,
        event: SourceEvent,
        timestamp: chrono::DateTime<chrono::Utc>,
        profiling: ProfilingMetadata,
        sequence: u64,
    ) -> Self {
        Self {
            source_id,
            event,
            timestamp,
            profiling: Some(profiling),
            source_position: None,
            sequence,
        }
    }

    /// Create an event with a required sequence and optional profiling metadata.
    pub fn with_sequence(
        source_id: String,
        event: SourceEvent,
        timestamp: chrono::DateTime<chrono::Utc>,
        sequence: u64,
        profiling: Option<ProfilingMetadata>,
    ) -> Self {
        Self {
            profiling,
            ..Self::new(source_id, event, timestamp, sequence)
        }
    }

    /// Attach the opaque source position bytes for stream resumption.
    #[must_use]
    pub fn with_source_position(mut self, position: Bytes) -> Self {
        self.source_position = Some(position);
        self
    }

    /// Set the opaque source position bytes for stream resumption in place.
    pub fn set_source_position(&mut self, position: Bytes) {
        self.source_position = Some(position);
    }
}

/// Decomposed parts of a [`SourceEventWrapper`], returned by
/// [`SourceEventWrapper::into_parts()`].
///
/// Using a named struct instead of a tuple makes call sites resilient to
/// field reordering and easier to evolve with new fields.
#[derive(Debug)]
pub struct SourceEventParts {
    pub source_id: String,
    pub event: SourceEvent,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub profiling: Option<ProfilingMetadata>,
    pub sequence: u64,
    pub source_position: Option<Bytes>,
}

impl SourceEventWrapper {
    /// Reconstruct an event from serialized plugin FFI payload parts.
    #[allow(clippy::too_many_arguments)]
    pub fn from_ffi_parts(
        source_id: String,
        event: SourceEvent,
        timestamp: chrono::DateTime<chrono::Utc>,
        profiling: Option<ProfilingMetadata>,
        sequence: u64,
        source_position: Option<Bytes>,
    ) -> Self {
        Self {
            source_id,
            event,
            timestamp,
            profiling,
            sequence,
            source_position,
        }
    }

    /// Consume this event and return its components as a named struct.
    /// This enables zero-copy extraction when the event has sole ownership.
    pub fn into_parts(self) -> SourceEventParts {
        SourceEventParts {
            source_id: self.source_id,
            event: self.event,
            timestamp: self.timestamp,
            profiling: self.profiling,
            sequence: self.sequence,
            source_position: self.source_position,
        }
    }

    /// Try to extract components from an `Arc<SourceEventWrapper>`.
    /// Uses Arc::try_unwrap to avoid cloning when we have sole ownership.
    /// Returns Ok with owned components if sole owner, Err with Arc back if shared.
    ///
    /// This enables zero-copy in Channel dispatch mode (single consumer per event)
    /// while still working correctly in Broadcast mode (cloning required).
    pub fn try_unwrap_arc(arc_self: Arc<Self>) -> Result<SourceEventParts, Arc<Self>> {
        Arc::try_unwrap(arc_self).map(|event| event.into_parts())
    }
}

/// Typed result diff emitted by continuous queries.
///
/// Each non-`Noop` variant carries a `row_signature` stamped by the core engine:
/// the path-solver binding hash for non-aggregating rows, and the grouping-key hash
/// for aggregations. Downstream consumers use it as the row identity in place of
/// JSON-equality matching.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResultDiff {
    #[serde(rename = "ADD")]
    Add {
        data: serde_json::Value,
        #[serde(default)]
        row_signature: u64,
    },
    #[serde(rename = "DELETE")]
    Delete {
        data: serde_json::Value,
        #[serde(default)]
        row_signature: u64,
    },
    #[serde(rename = "UPDATE")]
    Update {
        data: serde_json::Value,
        before: serde_json::Value,
        after: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        grouping_keys: Option<Vec<String>>,
        #[serde(default)]
        row_signature: u64,
    },
    #[serde(rename = "aggregation")]
    Aggregation {
        before: Option<serde_json::Value>,
        after: serde_json::Value,
        #[serde(default)]
        row_signature: u64,
    },
    #[serde(rename = "noop")]
    Noop,
}

/// Result emitted by a continuous query when data changes.
///
/// Contains the diff (added, updated, deleted rows) plus metadata and
/// optional profiling information. Dispatched to reactions via the priority queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub query_id: String,
    /// Monotonic per-query sequence number identifying this emission.
    /// Reactions persist this in their checkpoint, the outbox is keyed by it,
    /// and the bootstrap APIs return it as `as_of_sequence`.
    #[serde(default)]
    pub sequence: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub results: Vec<ResultDiff>,
    pub metadata: HashMap<String, serde_json::Value>,
    /// Optional profiling metadata for performance tracking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiling: Option<ProfilingMetadata>,
}

impl QueryResult {
    /// Create a new QueryResult without profiling
    pub fn new(
        query_id: String,
        sequence: u64,
        timestamp: chrono::DateTime<chrono::Utc>,
        results: Vec<ResultDiff>,
        metadata: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            query_id,
            sequence,
            timestamp,
            results,
            metadata,
            profiling: None,
        }
    }

    /// Create a new QueryResult with profiling metadata
    pub fn with_profiling(
        query_id: String,
        sequence: u64,
        timestamp: chrono::DateTime<chrono::Utc>,
        results: Vec<ResultDiff>,
        metadata: HashMap<String, serde_json::Value>,
        profiling: ProfilingMetadata,
    ) -> Self {
        Self {
            query_id,
            sequence,
            timestamp,
            results,
            metadata,
            profiling: Some(profiling),
        }
    }
}
