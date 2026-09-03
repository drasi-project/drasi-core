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

use crate::profiling::ProfilingMetadata;
use bytes::Bytes;
use drasi_core::models::SourceChange;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// Trait for types that have a timestamp, required for priority queue ordering
pub trait Timestamped {
    fn timestamp(&self) -> chrono::DateTime<chrono::Utc>;
}

/// Trait for types carrying a monotonic per-source sequence number.
///
/// Used by the priority queue as the final tie-breaker when both the timestamp
/// and the source rank are equal (i.e. two events from the *same* source that
/// share a timestamp). Because a sequence is only meaningful within a single
/// source, it is only ever compared between events of equal source rank.
pub trait Sequenced {
    fn sequence(&self) -> u64;
}

/// Type of Drasi component
///
/// Used for identifying component types in events and monitoring.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ComponentType {
    Source,
    Query,
    Reaction,
    BootstrapProvider,
    IdentityProvider,
}

/// Execution status of a Drasi component
///
/// `ComponentStatus` represents the current lifecycle state of sources, queries, and reactions.
/// Components transition through these states during their lifecycle, from creation through
/// execution to shutdown.
///
/// # Status Lifecycle
///
/// A typical component lifecycle follows this progression:
///
/// ```text
/// Added → Starting → Running → Stopping → Stopped
///              ↓                              ↓
///            Error                         Removed
/// ```
///
/// # Status Values
///
/// - **Added**: Component has been registered in the graph but not yet started
/// - **Starting**: Component is initializing (connecting to resources, loading data, etc.)
/// - **Running**: Component is actively processing (ingesting, querying, or delivering)
/// - **Stopping**: Component is shutting down gracefully
/// - **Stopped**: Component is not running (stopped after previously running)
/// - **Removed**: Component has been removed from the graph
/// - **Error**: Component encountered an error and cannot continue (see error_message)
///
/// # Usage
///
/// Status is available through runtime information methods on [`DrasiLib`](crate::DrasiLib):
///
/// - [`get_source_status()`](crate::DrasiLib::get_source_status)
/// - [`get_query_status()`](crate::DrasiLib::get_query_status)
/// - [`get_reaction_status()`](crate::DrasiLib::get_reaction_status)
///
/// And through runtime info structs:
///
/// - [`SourceRuntime`](crate::SourceRuntime)
/// - [`QueryRuntime`](crate::QueryRuntime)
/// - [`ReactionRuntime`](crate::ReactionRuntime)
///
/// # Examples
///
/// ## Monitoring Component Status
///
/// ```no_run
/// use drasi_lib::{DrasiLib, ComponentStatus};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder().with_id("my-server").build().await?;
/// core.start().await?;
///
/// // Check source status
/// let source_status = core.get_source_status("orders_db").await?;
/// match source_status {
///     ComponentStatus::Running => println!("Source is running"),
///     ComponentStatus::Error => println!("Source has errors"),
///     ComponentStatus::Starting => println!("Source is starting up"),
///     _ => println!("Source status: {:?}", source_status),
/// }
///
/// // Get detailed info with status
/// let source_info = core.get_source_info("orders_db").await?;
/// if source_info.status == ComponentStatus::Error {
///     if let Some(error) = source_info.error_message {
///         eprintln!("Error: {}", error);
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// ## Waiting for Component to Start
///
/// ```no_run
/// use drasi_lib::{DrasiLib, ComponentStatus};
/// use tokio::time::{sleep, Duration};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder().with_id("my-server").build().await?;
/// core.start_source("orders_db").await?;
///
/// // Poll until source is running
/// loop {
///     let status = core.get_source_status("orders_db").await?;
///     match status {
///         ComponentStatus::Running => break,
///         ComponentStatus::Error => return Err("Source failed to start".into()),
///         _ => sleep(Duration::from_millis(100)).await,
///     }
/// }
/// println!("Source is now running");
/// # Ok(())
/// # }
/// ```
///
/// ## Checking All Components
///
/// ```no_run
/// use drasi_lib::{DrasiLib, ComponentStatus};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder().with_id("my-server").build().await?;
/// core.start().await?;
///
/// // Check all sources
/// let sources = core.list_sources().await?;
/// for (id, status) in sources {
///     println!("Source {}: {:?}", id, status);
/// }
///
/// // Check all queries
/// let queries = core.list_queries().await?;
/// for (id, status) in queries {
///     println!("Query {}: {:?}", id, status);
/// }
///
/// // Check all reactions
/// let reactions = core.list_reactions().await?;
/// for (id, status) in reactions {
///     println!("Reaction {}: {:?}", id, status);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ComponentStatus {
    /// Component has been registered in the graph but not yet started.
    Added,
    Starting,
    Running,
    Stopping,
    Stopped,
    /// Component has been removed from the graph.
    Removed,
    Reconfiguring,
    Error,
}

/// A source change event with metadata for dispatching to queries.
#[derive(Debug, Clone)]
pub struct SourceChangeEvent {
    pub source_id: String,
    pub change: SourceChange,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// WAL-assigned sequence number, if durability is enabled for this source.
    pub sequence: Option<u64>,
}

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

/// Unstamped, source-authored event payload.
///
/// This is the **only** type a source author constructs. It deliberately has
/// **no `sequence` field**: the framework assigns the monotonic sequence when
/// the draft is dispatched via [`SourceBase::dispatch_event`] /
/// [`SourceBase::dispatch_events_batch`], which is the sole bridge that turns a
/// draft into a downstream [`StampedSourceEvent`]. Because "unstamped" and
/// "stamped" are distinct types, it is a *compile-time* guarantee that no event
/// reaches the query side without a framework-assigned sequence.
///
/// [`SourceBase::dispatch_event`]: crate::sources::base::SourceBase::dispatch_event
/// [`SourceBase::dispatch_events_batch`]: crate::sources::base::SourceBase::dispatch_events_batch
#[derive(Debug, Clone)]
pub struct SourceEventDraft {
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
    /// Optional source-supplied sequence (the WAL fast-path).
    ///
    /// **Most sources leave this `None`** and let the framework assign the
    /// sequence — that is the recommended path (the source owns the *position*;
    /// the framework owns the *sequence*). A source with its own durable,
    /// monotonic ordinal (e.g. a WAL append index) *may* supply it here; the
    /// framework then uses that value and advances its counter past it to stay
    /// monotonic. It is never possible to omit the sequence downstream: if this
    /// is `None`, the framework still stamps one.
    pub supplied_sequence: Option<u64>,
}

impl SourceEventDraft {
    /// Create a new unstamped draft without profiling.
    pub fn new(
        source_id: String,
        event: SourceEvent,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            source_id,
            event,
            timestamp,
            profiling: None,
            source_position: None,
            supplied_sequence: None,
        }
    }

    /// Create a new unstamped draft with profiling metadata.
    pub fn with_profiling(
        source_id: String,
        event: SourceEvent,
        timestamp: chrono::DateTime<chrono::Utc>,
        profiling: ProfilingMetadata,
    ) -> Self {
        Self {
            source_id,
            event,
            timestamp,
            profiling: Some(profiling),
            source_position: None,
            supplied_sequence: None,
        }
    }

    /// Attach the opaque source position bytes for stream resumption.
    #[must_use]
    pub fn with_source_position(mut self, position: Bytes) -> Self {
        self.source_position = Some(position);
        self
    }

    /// Attach a source-supplied sequence (WAL fast-path). See
    /// [`supplied_sequence`](Self::supplied_sequence).
    #[must_use]
    pub fn with_supplied_sequence(mut self, sequence: u64) -> Self {
        self.supplied_sequence = Some(sequence);
        self
    }

    /// Set the opaque source position bytes for stream resumption in place.
    pub fn set_source_position(&mut self, position: Bytes) {
        self.source_position = Some(position);
    }
}

/// Stamped, framework-produced source event carrying a **mandatory** sequence.
///
/// Produced only by the framework — either by [`SourceBase::dispatch_event`] /
/// [`SourceBase::dispatch_events_batch`] stamping a [`SourceEventDraft`], or by
/// framework-internal replay/reconstruction paths. It is `#[non_exhaustive]`, so
/// no other crate can build one with a struct literal, and the only public
/// cross-crate constructor ([`from_ffi_parts`](Self::from_ffi_parts)) *requires*
/// a `u64` sequence. Together with `sequence: u64` (not `Option`), this makes
/// "an event with no sequence" unrepresentable past the source boundary.
///
/// [`SourceBase::dispatch_event`]: crate::sources::base::SourceBase::dispatch_event
/// [`SourceBase::dispatch_events_batch`]: crate::sources::base::SourceBase::dispatch_events_batch
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StampedSourceEvent {
    pub source_id: String,
    pub event: SourceEvent,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Optional profiling metadata for performance tracking.
    pub profiling: Option<ProfilingMetadata>,
    /// Monotonic sequence number assigned by the framework.
    /// Used for ordering, watermarks, gap detection, and dedup.
    pub sequence: u64,
    /// Opaque source position bytes for stream resumption on restart.
    /// `None` for volatile sources that don't support replay.
    pub source_position: Option<Bytes>,
}

/// Decomposed parts of a [`StampedSourceEvent`], returned by
/// [`StampedSourceEvent::into_parts()`].
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

impl StampedSourceEvent {
    /// Stamp a [`SourceEventDraft`] with a framework-assigned `sequence`,
    /// producing the downstream event. This is `pub(crate)` because only the
    /// framework dispatch path may create a stamped event from a draft.
    pub(crate) fn stamp(draft: SourceEventDraft, sequence: u64) -> Self {
        Self {
            source_id: draft.source_id,
            event: draft.event,
            timestamp: draft.timestamp,
            profiling: draft.profiling,
            sequence,
            source_position: draft.source_position,
        }
    }

    /// Reconstruct a stamped event from parts that already crossed the plugin
    /// FFI boundary.
    ///
    /// This is the only public cross-crate constructor. It *requires* a `u64`
    /// sequence — a value the producing plugin's own framework already assigned
    /// — so it cannot be used to build an unsequenced event. It exists for the
    /// host SDK to rebuild a host-owned event from a serialized payload.
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

    /// Try to extract components from an `Arc<StampedSourceEvent>`.
    /// Uses Arc::try_unwrap to avoid cloning when we have sole ownership.
    /// Returns Ok with owned components if sole owner, Err with Arc back if shared.
    ///
    /// This enables zero-copy in Channel dispatch mode (single consumer per event)
    /// while still working correctly in Broadcast mode (cloning required).
    pub fn try_unwrap_arc(arc_self: Arc<Self>) -> Result<SourceEventParts, Arc<Self>> {
        Arc::try_unwrap(arc_self).map(|event| event.into_parts())
    }
}

// Implement Timestamped for StampedSourceEvent for use in generic priority queue
impl Timestamped for StampedSourceEvent {
    fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        self.timestamp
    }
}

// The framework-assigned sequence breaks same-timestamp ties within a source.
impl Sequenced for StampedSourceEvent {
    fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// Arc-wrapped StampedSourceEvent for zero-copy distribution
pub type ArcSourceEvent = Arc<StampedSourceEvent>;

/// Bootstrap event wrapper for dedicated bootstrap channels
#[derive(Debug, Clone)]
pub struct BootstrapEvent {
    pub source_id: String,
    pub change: SourceChange,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub sequence: u64,
}

/// Subscription request from Query to Source
#[derive(Debug, Clone)]
pub struct SubscriptionRequest {
    pub query_id: String,
    pub source_id: String,
    pub enable_bootstrap: bool,
    pub node_labels: Vec<String>,
    pub relation_labels: Vec<String>,
}

/// Subscription response from Source to Query
pub struct SubscriptionResponse {
    pub query_id: String,
    pub source_id: String,
    pub receiver: Box<dyn super::ChangeReceiver<StampedSourceEvent>>,
    pub bootstrap_receiver: Option<BootstrapEventReceiver>,
    /// Shared handle for the query to report its last durably-processed sequence position.
    /// Created by replay-capable sources when `request_position_handle` is true.
    /// The query writes to this atomically after each commit; the source reads the
    /// minimum across all subscribers to advance its upstream cursor.
    /// Sources should initialize this to `u64::MAX` (meaning "no position confirmed yet").
    pub position_handle: Option<Arc<AtomicU64>>,
    /// Receives the `BootstrapResult` after bootstrap completes, carrying the
    /// optional `source_position` snapshot boundary for the bootstrap-to-streaming
    /// transition. `None` when bootstrap is not active or for FFI/plugin sources.
    pub bootstrap_result_receiver:
        Option<tokio::sync::oneshot::Receiver<anyhow::Result<crate::bootstrap::BootstrapResult>>>,
}

/// Subscription response from Query to Reaction
pub struct QuerySubscriptionResponse {
    pub query_id: String,
    pub receiver: Box<dyn super::ChangeReceiver<QueryResult>>,
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

// Implement Timestamped for QueryResult for use in generic priority queue
impl Timestamped for QueryResult {
    fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        self.timestamp
    }
}

// The per-query sequence breaks same-timestamp ties for query results.
impl Sequenced for QueryResult {
    fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// Arc-wrapped QueryResult for zero-copy distribution
pub type ArcQueryResult = Arc<QueryResult>;

/// Lifecycle event emitted when a component's status changes.
///
/// Broadcast via the component event channel to all subscribers.
/// Used for monitoring, logging, and reactive lifecycle coordination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentEvent {
    pub component_id: String,
    pub component_type: ComponentType,
    pub status: ComponentStatus,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub message: Option<String>,
}

/// Control messages for component lifecycle management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMessage {
    Start(String),
    Stop(String),
    Status(String),
    Shutdown,
}

pub type ComponentEventBroadcastSender = broadcast::Sender<ComponentEvent>;
pub type ComponentEventBroadcastReceiver = broadcast::Receiver<ComponentEvent>;
/// Backward-compatible mpsc channel types used by host-sdk plugin callbacks.
/// New code should use `ComponentUpdateSender` from `component_graph` instead.
pub type ComponentEventSender = mpsc::Sender<ComponentEvent>;
pub type ComponentEventReceiver = mpsc::Receiver<ComponentEvent>;
pub type ControlMessageReceiver = mpsc::Receiver<ControlMessage>;
pub type ControlMessageSender = mpsc::Sender<ControlMessage>;

// Broadcast channel types for zero-copy event distribution
pub type SourceBroadcastSender = broadcast::Sender<ArcSourceEvent>;
pub type SourceBroadcastReceiver = broadcast::Receiver<ArcSourceEvent>;

// Broadcast channel types for zero-copy query result distribution
pub type QueryResultBroadcastSender = broadcast::Sender<ArcQueryResult>;
pub type QueryResultBroadcastReceiver = broadcast::Receiver<ArcQueryResult>;

// Bootstrap channel types for dedicated bootstrap data delivery
pub type BootstrapEventSender = mpsc::Sender<BootstrapEvent>;
pub type BootstrapEventReceiver = mpsc::Receiver<BootstrapEvent>;

/// Control signals for coordination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlSignal {
    /// Query has entered running state
    Running { query_id: String },
    /// Query has stopped
    Stopped { query_id: String },
    /// Query has been deleted
    Deleted { query_id: String },
}

/// Wrapper for control signals with metadata
#[derive(Debug, Clone)]
pub struct ControlSignalWrapper {
    pub signal: ControlSignal,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub sequence_number: Option<u64>,
}

impl ControlSignalWrapper {
    pub fn new(signal: ControlSignal) -> Self {
        Self {
            signal,
            timestamp: chrono::Utc::now(),
            sequence_number: None,
        }
    }

    pub fn with_sequence(signal: ControlSignal, sequence_number: u64) -> Self {
        Self {
            signal,
            timestamp: chrono::Utc::now(),
            sequence_number: Some(sequence_number),
        }
    }
}

pub type ControlSignalReceiver = mpsc::Receiver<ControlSignalWrapper>;
pub type ControlSignalSender = mpsc::Sender<ControlSignalWrapper>;

pub struct EventChannels {
    pub _control_tx: ControlMessageSender,
    pub control_signal_tx: ControlSignalSender,
}

pub struct EventReceivers {
    pub _control_rx: ControlMessageReceiver,
    pub control_signal_rx: ControlSignalReceiver,
}

impl EventChannels {
    pub fn new() -> (Self, EventReceivers) {
        let (control_tx, control_rx) = mpsc::channel(100);
        let (control_signal_tx, control_signal_rx) = mpsc::channel(100);

        let channels = Self {
            _control_tx: control_tx,
            control_signal_tx,
        };

        let receivers = EventReceivers {
            _control_rx: control_rx,
            control_signal_rx,
        };

        (channels, receivers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};

    fn create_test_source_change() -> SourceChange {
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("TestSource", "test-node-1"),
                labels: vec!["TestLabel".into()].into(),
                effective_from: 1000,
            },
            properties: Default::default(),
        };
        SourceChange::Insert { element }
    }

    fn stamped(source_id: &str, sequence: u64) -> StampedSourceEvent {
        let draft = SourceEventDraft::new(
            source_id.to_string(),
            SourceEvent::Change(create_test_source_change()),
            chrono::Utc::now(),
        );
        StampedSourceEvent::stamp(draft, sequence)
    }

    #[test]
    fn test_stamped_event_into_parts() {
        let event = stamped("test-source", 7);

        let parts = event.into_parts();

        assert_eq!(parts.source_id, "test-source");
        assert!(matches!(parts.event, SourceEvent::Change(_)));
        assert!(parts.profiling.is_none());
        assert_eq!(parts.sequence, 7);
    }

    #[test]
    fn test_try_unwrap_arc_sole_owner() {
        let arc = Arc::new(stamped("test-source", 1));

        // With sole ownership, try_unwrap_arc should succeed
        let result = StampedSourceEvent::try_unwrap_arc(arc);
        assert!(result.is_ok());

        let parts = result.unwrap();
        assert_eq!(parts.source_id, "test-source");
        assert!(matches!(parts.event, SourceEvent::Change(_)));
    }

    #[test]
    fn test_try_unwrap_arc_shared() {
        let arc = Arc::new(stamped("test-source", 1));
        let _arc2 = arc.clone(); // Create another reference

        // With shared ownership, try_unwrap_arc should fail and return the Arc
        let result = StampedSourceEvent::try_unwrap_arc(arc);
        assert!(result.is_err());

        // The returned Arc should still be valid
        let returned_arc = result.unwrap_err();
        assert_eq!(returned_arc.source_id, "test-source");
    }

    #[test]
    fn test_zero_copy_extraction_path() {
        // Simulate the zero-copy extraction path used in query processing
        let arc = Arc::new(stamped("test-source", 3));

        // This is the zero-copy path - when we have sole ownership
        let parts = match StampedSourceEvent::try_unwrap_arc(arc) {
            Ok(parts) => parts,
            Err(arc) => {
                // Fallback to cloning (would be needed in broadcast mode)
                SourceEventParts {
                    source_id: arc.source_id.clone(),
                    event: arc.event.clone(),
                    timestamp: arc.timestamp,
                    profiling: arc.profiling.clone(),
                    sequence: arc.sequence,
                    source_position: arc.source_position.clone(),
                }
            }
        };

        // Extract SourceChange from owned event (no clone!)
        let source_change = match parts.event {
            SourceEvent::Change(change) => Some(change),
            _ => None,
        };

        assert_eq!(parts.source_id, "test-source");
        assert!(source_change.is_some());
    }

    #[test]
    fn test_stamp_assigns_sequence() {
        let draft = SourceEventDraft::new(
            "test-source".to_string(),
            SourceEvent::Change(create_test_source_change()),
            chrono::Utc::now(),
        );
        let event = StampedSourceEvent::stamp(draft, 42);
        assert_eq!(event.sequence, 42);
        assert!(event.profiling.is_none());

        let parts = event.into_parts();
        assert_eq!(parts.sequence, 42);
    }

    #[test]
    fn test_draft_has_no_supplied_sequence_by_default() {
        let draft = SourceEventDraft::new(
            "test-source".to_string(),
            SourceEvent::Change(create_test_source_change()),
            chrono::Utc::now(),
        );
        assert!(draft.supplied_sequence.is_none());
        assert!(draft.source_position.is_none());
    }

    #[test]
    fn test_subscription_response_with_position_handle() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::Arc;

        let handle = Arc::new(AtomicU64::new(u64::MAX));
        assert_eq!(handle.load(Ordering::Relaxed), u64::MAX);

        // Verify the handle can be cloned and read (simulates source reading query's position)
        let handle_clone = handle.clone();
        handle.store(500, Ordering::Relaxed);
        assert_eq!(handle_clone.load(Ordering::Relaxed), 500);
    }

    #[test]
    fn test_subscription_settings_with_resume_from() {
        use std::collections::HashSet;
        let position_bytes = Bytes::from_static(&[0x01, 0x02, 0x03, 0x04]);
        let settings = crate::config::SourceSubscriptionSettings {
            source_id: "test-source".to_string(),
            enable_bootstrap: false,
            query_id: "test-query".to_string(),
            nodes: HashSet::new(),
            relations: HashSet::new(),
            resume_from: Some(position_bytes.clone()),
            resume_sequence: None,
            request_position_handle: true,
        };
        assert_eq!(settings.resume_from, Some(position_bytes));
        assert!(settings.request_position_handle);
    }
}
