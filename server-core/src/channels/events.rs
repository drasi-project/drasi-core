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
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ComponentType {
    Source,
    Query,
    Reaction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ComponentStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub query_id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub results: Vec<serde_json::Value>,
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
        results: Vec<serde_json::Value>,
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
        results: Vec<serde_json::Value>,
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

pub type SourceEventReceiver = mpsc::Receiver<SourceEventWrapper>;
pub type SourceEventSender = mpsc::Sender<SourceEventWrapper>;
pub type QueryResultReceiver = mpsc::Receiver<QueryResult>;
pub type QueryResultSender = mpsc::Sender<QueryResult>;
pub type ComponentEventReceiver = mpsc::Receiver<ComponentEvent>;
pub type ComponentEventSender = mpsc::Sender<ComponentEvent>;
pub type ControlMessageReceiver = mpsc::Receiver<ControlMessage>;
pub type ControlMessageSender = mpsc::Sender<ControlMessage>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapRequest {
    pub query_id: String,
    pub node_labels: Vec<String>,
    pub relation_labels: Vec<String>,
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapResponse {
    pub request_id: String,
    pub status: BootstrapStatus,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BootstrapStatus {
    Started,
    InProgress { count: usize },
    Completed { total_count: usize },
    Failed { error: String },
}

pub type BootstrapRequestReceiver = mpsc::Receiver<BootstrapRequest>;
pub type BootstrapRequestSender = mpsc::Sender<BootstrapRequest>;
pub type BootstrapResponseReceiver = mpsc::Receiver<BootstrapResponse>;
pub type BootstrapResponseSender = mpsc::Sender<BootstrapResponse>;

/// Control signals for bootstrap coordination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlSignal {
    /// Bootstrap has started for a query-source pair
    BootstrapStarted {
        query_id: String,
        source_id: String,
    },
    /// Bootstrap has completed for a query-source pair
    BootstrapCompleted {
        query_id: String,
        source_id: String,
    },
    /// Query has entered running state
    Running {
        query_id: String,
    },
    /// Query has stopped
    Stopped {
        query_id: String,
    },
    /// Query has been deleted
    Deleted {
        query_id: String,
    },
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
    pub source_event_tx: SourceEventSender,
    pub query_result_tx: QueryResultSender,
    pub component_event_tx: ComponentEventSender,
    pub _control_tx: ControlMessageSender,
    pub bootstrap_request_tx: BootstrapRequestSender,
    #[allow(dead_code)]
    pub bootstrap_response_tx: BootstrapResponseSender,
    pub control_signal_tx: ControlSignalSender,
}

pub struct EventReceivers {
    pub source_event_rx: SourceEventReceiver,
    pub query_result_rx: QueryResultReceiver,
    pub component_event_rx: ComponentEventReceiver,
    pub _control_rx: ControlMessageReceiver,
    pub bootstrap_request_rx: BootstrapRequestReceiver,
    #[allow(dead_code)]
    pub bootstrap_response_rx: BootstrapResponseReceiver,
    pub control_signal_rx: ControlSignalReceiver,
}

impl EventChannels {
    pub fn new() -> (Self, EventReceivers) {
        let (source_event_tx, source_event_rx) = mpsc::channel(1000);
        let (query_result_tx, query_result_rx) = mpsc::channel(1000);
        let (component_event_tx, component_event_rx) = mpsc::channel(1000);
        let (control_tx, control_rx) = mpsc::channel(100);
        let (bootstrap_request_tx, bootstrap_request_rx) = mpsc::channel(100);
        let (bootstrap_response_tx, bootstrap_response_rx) = mpsc::channel(100);
        let (control_signal_tx, control_signal_rx) = mpsc::channel(100);

        let channels = Self {
            source_event_tx,
            query_result_tx,
            component_event_tx,
            _control_tx: control_tx,
            bootstrap_request_tx,
            bootstrap_response_tx,
            control_signal_tx,
        };

        let receivers = EventReceivers {
            source_event_rx,
            query_result_rx,
            component_event_rx,
            _control_rx: control_rx,
            bootstrap_request_rx,
            bootstrap_response_rx,
            control_signal_rx,
        };

        (channels, receivers)
    }
}
