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

use drasi_core::models::SourceChange;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Minimal locator parsed from a webhook payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebhookLocator {
    pub event_type: String,
    pub action: String,
    pub node_id: Option<String>,
    pub repository_full_name: Option<String>,
    pub parent_issue_id: Option<String>,
    pub parent_pull_request_id: Option<String>,
    pub project_id: Option<String>,
    pub project_owner: Option<String>,
    pub project_number: Option<u32>,
}

/// Durable WAL entry payload derived from an admitted webhook delivery.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebhookDelivery {
    pub delivery_id: String,
    pub admitted_at: i64,
    pub locator: WebhookLocator,
}

/// Hydrator degradation state used by `/health`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HydratorHealth {
    pub stalled_delivery_id: Option<String>,
    pub retry_count: u32,
    pub next_retry_secs: Option<u64>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub terminal: bool,
}

/// Serializable element snapshot used for root-level diffing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RootSnapshot {
    pub root_id: String,
    pub root_kind: String,
    pub repository_full_name: Option<String>,
    #[serde(default)]
    pub committed_delivery_id: Option<String>,
    #[serde(default)]
    pub committed_sequence: Option<u64>,
    pub elements: HashMap<String, SnapshotElement>,
}

/// Serializable element record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotElement {
    pub element_type: String,
    pub id: String,
    pub labels: Vec<String>,
    pub properties: serde_json::Value,
    pub in_node_id: Option<String>,
    pub out_node_id: Option<String>,
}

/// Single durable owner of reconciliation, hydration, and replay state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileState {
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub index: HashMap<String, SnapshotElement>,
    #[serde(default)]
    pub pending_delta: Option<PendingDelta>,
    #[serde(default)]
    pub absences: HashMap<String, AbsenceObservation>,
    #[serde(default)]
    pub null_retry_counts: HashMap<u64, u32>,
    #[serde(default)]
    pub terminal_outcomes: HashMap<u64, HydrationTerminalOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingDelta {
    pub changes: Vec<SourceChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AbsenceObservation {
    pub generation: u64,
    pub wal_coverage_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HydrationTerminalOutcome {
    pub delivery_id: String,
    pub root_id: Option<String>,
    pub reason: String,
    pub attempts: u32,
    pub generation: u64,
}
