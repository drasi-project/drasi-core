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
use drasi_core::models::SourceChange;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// Trait for types that have a timestamp, required for priority queue ordering
pub trait Timestamped {
    fn timestamp(&self) -> chrono::DateTime<chrono::Utc>;
}

/// Type of Drasi component
///
/// Used for identifying component types in events and monitoring.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ComponentType {
    Source,
    Query,
    Reaction,
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
/// Stopped → Starting → Running → Stopping → Stopped
///                ↓
///              Error
/// ```
///
/// # Status Values
///
/// - **Starting**: Component is initializing (connecting to resources, loading data, etc.)
/// - **Running**: Component is actively processing (ingesting, querying, or delivering)
/// - **Stopping**: Component is shutting down gracefully
/// - **Stopped**: Component is not running (initial or final state)
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ComponentStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Reconfiguring,
    Error,
}

#[derive(Debug, Clone)]
pub struct SourceChangeEvent {
    pub source_id: String,
    pub change: SourceChange,
    pub timestamp: chrono::DateTime<chrono::Utc>,
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
}

/// Control operation types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ControlOperation {
    Insert,
    Update,
    Delete,
}

/// Unified event envelope carrying both data changes and control messages
#[derive(Debug, Clone)]
pub enum SourceEvent {
    /// Data change event from source
    Change(SourceChange),
    /// Control event for query coordination
    Control(SourceControl),
    /// Bootstrap start marker for a specific query
    BootstrapStart { query_id: String },
    /// Bootstrap end marker for a specific query
    BootstrapEnd { query_id: String },
}

/// Wrapper for source events with metadata
#[derive(Debug, Clone)]
pub struct SourceEventWrapper {
    pub source_id: String,
    pub event: SourceEvent,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Optional profiling metadata for performance tracking
    pub profiling: Option<ProfilingMetadata>,
}

impl SourceEventWrapper {
    /// Create a new SourceEventWrapper without profiling
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
        }
    }

    /// Create a new SourceEventWrapper with profiling metadata
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
        }
    }

    /// Consume this wrapper and return its components.
    /// This enables zero-copy extraction when the wrapper has sole ownership.
    pub fn into_parts(
        self,
    ) -> (
        String,
        SourceEvent,
        chrono::DateTime<chrono::Utc>,
        Option<ProfilingMetadata>,
    ) {
        (self.source_id, self.event, self.timestamp, self.profiling)
    }

    /// Try to extract components from an Arc<SourceEventWrapper>.
    /// Uses Arc::try_unwrap to avoid cloning when we have sole ownership.
    /// Returns Ok with owned components if sole owner, Err with Arc back if shared.
    ///
    /// This enables zero-copy in Channel dispatch mode (single consumer per event)
    /// while still working correctly in Broadcast mode (cloning required).
    pub fn try_unwrap_arc(
        arc_self: Arc<Self>,
    ) -> Result<
        (
            String,
            SourceEvent,
            chrono::DateTime<chrono::Utc>,
            Option<ProfilingMetadata>,
        ),
        Arc<Self>,
    > {
        Arc::try_unwrap(arc_self).map(|wrapper| wrapper.into_parts())
    }
}

// Implement Timestamped for SourceEventWrapper for use in generic priority queue
impl Timestamped for SourceEventWrapper {
    fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        self.timestamp
    }
}

/// Arc-wrapped SourceEventWrapper for zero-copy distribution
pub type ArcSourceEvent = Arc<SourceEventWrapper>;

/// Bootstrap event wrapper for dedicated bootstrap channels
#[derive(Debug, Clone)]
pub struct BootstrapEvent {
    pub source_id: String,
    pub change: SourceChange,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub sequence: u64,
}

/// Bootstrap completion signal
#[derive(Debug, Clone)]
pub struct BootstrapComplete {
    pub source_id: String,
    pub total_events: u64,
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
    pub receiver: Box<dyn super::ChangeReceiver<SourceEventWrapper>>,
    pub bootstrap_receiver: Option<BootstrapEventReceiver>,
}

/// Subscription response from Query to Reaction
pub struct QuerySubscriptionResponse {
    pub query_id: String,
    pub receiver: Box<dyn super::ChangeReceiver<QueryResult>>,
}

/// Typed result diff emitted by continuous queries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ResultDiff {
    #[serde(rename = "ADD")]
    Add { data: serde_json::Value },
    #[serde(rename = "DELETE")]
    Delete { data: serde_json::Value },
    #[serde(rename = "UPDATE")]
    Update {
        data: serde_json::Value,
        before: serde_json::Value,
        after: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        grouping_keys: Option<Vec<String>>,
    },
    #[serde(rename = "aggregation")]
    Aggregation {
        before: Option<serde_json::Value>,
        after: serde_json::Value,
    },
    #[serde(rename = "noop")]
    Noop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub query_id: String,
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
        timestamp: chrono::DateTime<chrono::Utc>,
        results: Vec<ResultDiff>,
        metadata: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            query_id,
            timestamp,
            results,
            metadata,
            profiling: None,
        }
    }

    /// Create a new QueryResult with profiling metadata
    pub fn with_profiling(
        query_id: String,
        timestamp: chrono::DateTime<chrono::Utc>,
        results: Vec<ResultDiff>,
        metadata: HashMap<String, serde_json::Value>,
        profiling: ProfilingMetadata,
    ) -> Self {
        Self {
            query_id,
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

/// Arc-wrapped QueryResult for zero-copy distribution
pub type ArcQueryResult = Arc<QueryResult>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentEvent {
    pub component_id: String,
    pub component_type: ComponentType,
    pub status: ComponentStatus,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMessage {
    Start(String),
    Stop(String),
    Status(String),
    Shutdown,
}

pub type ComponentEventReceiver = mpsc::Receiver<ComponentEvent>;
pub type ComponentEventSender = mpsc::Sender<ComponentEvent>;
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
pub type BootstrapCompleteSender = mpsc::Sender<BootstrapComplete>;
pub type BootstrapCompleteReceiver = mpsc::Receiver<BootstrapComplete>;

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
    pub component_event_tx: ComponentEventSender,
    pub _control_tx: ControlMessageSender,
    pub control_signal_tx: ControlSignalSender,
}

pub struct EventReceivers {
    pub component_event_rx: ComponentEventReceiver,
    pub _control_rx: ControlMessageReceiver,
    pub control_signal_rx: ControlSignalReceiver,
}

impl EventChannels {
    pub fn new() -> (Self, EventReceivers) {
        let (component_event_tx, component_event_rx) = mpsc::channel(1000);
        let (control_tx, control_rx) = mpsc::channel(100);
        let (control_signal_tx, control_signal_rx) = mpsc::channel(100);

        let channels = Self {
            component_event_tx,
            _control_tx: control_tx,
            control_signal_tx,
        };

        let receivers = EventReceivers {
            component_event_rx,
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

    #[test]
    fn test_source_event_wrapper_into_parts() {
        let change = create_test_source_change();
        let wrapper = SourceEventWrapper::new(
            "test-source".to_string(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
        );

        let (source_id, event, _timestamp, profiling) = wrapper.into_parts();

        assert_eq!(source_id, "test-source");
        assert!(matches!(event, SourceEvent::Change(_)));
        assert!(profiling.is_none());
    }

    #[test]
    fn test_try_unwrap_arc_sole_owner() {
        let change = create_test_source_change();
        let wrapper = SourceEventWrapper::new(
            "test-source".to_string(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
        );
        let arc = Arc::new(wrapper);

        // With sole ownership, try_unwrap_arc should succeed
        let result = SourceEventWrapper::try_unwrap_arc(arc);
        assert!(result.is_ok());

        let (source_id, event, _timestamp, _profiling) = result.unwrap();
        assert_eq!(source_id, "test-source");
        assert!(matches!(event, SourceEvent::Change(_)));
    }

    #[test]
    fn test_try_unwrap_arc_shared() {
        let change = create_test_source_change();
        let wrapper = SourceEventWrapper::new(
            "test-source".to_string(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
        );
        let arc = Arc::new(wrapper);
        let _arc2 = arc.clone(); // Create another reference

        // With shared ownership, try_unwrap_arc should fail and return the Arc
        let result = SourceEventWrapper::try_unwrap_arc(arc);
        assert!(result.is_err());

        // The returned Arc should still be valid
        let returned_arc = result.unwrap_err();
        assert_eq!(returned_arc.source_id, "test-source");
    }

    #[test]
    fn test_zero_copy_extraction_path() {
        // Simulate the zero-copy extraction path used in query processing
        let change = create_test_source_change();
        let wrapper = SourceEventWrapper::new(
            "test-source".to_string(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
        );
        let arc = Arc::new(wrapper);

        // This is the zero-copy path - when we have sole ownership
        let (source_id, event, _timestamp, _profiling) =
            match SourceEventWrapper::try_unwrap_arc(arc) {
                Ok(parts) => parts,
                Err(arc) => {
                    // Fallback to cloning (would be needed in broadcast mode)
                    (
                        arc.source_id.clone(),
                        arc.event.clone(),
                        arc.timestamp,
                        arc.profiling.clone(),
                    )
                }
            };

        // Extract SourceChange from owned event (no clone!)
        let source_change = match event {
            SourceEvent::Change(change) => Some(change),
            _ => None,
        };

        assert_eq!(source_id, "test-source");
        assert!(source_change.is_some());
    }
}
