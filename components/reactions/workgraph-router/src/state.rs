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

use std::sync::Arc;

use chrono::Utc;
use drasi_lib::state_store::{StateStoreCreateIfAbsentResult, StateStoreProvider};
use serde::{Deserialize, Serialize};

use crate::candidate::RoutingCandidate;
use crate::decision::RoutingDecision;

const RESERVATION_PREFIX: &str = "workgraph-router/reservations/";
const ROUTING_STATE_PREFIX: &str = "workgraph-router/state/";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReservationRecord {
    pub reservation_key: String,
    pub execution_id: String,
    pub required_event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_instance_id: Option<String>,
    pub policy_id: String,
    pub policy_type: String,
    pub policy_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    pub created_at: String,
    pub completed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SideEffectProgress {
    pub decision_comment_written: bool,
    pub responsibility_written: bool,
    pub project_status_updated: bool,
}

impl SideEffectProgress {
    pub fn is_complete(&self) -> bool {
        self.decision_comment_written && self.responsibility_written && self.project_status_updated
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingStateRecord {
    pub reservation_key: String,
    pub execution_id: String,
    pub required_event_type: String,
    pub policy_id: String,
    pub policy_type: String,
    pub policy_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<RoutingDecision>,
    #[serde(default)]
    pub selected_transition: Option<(String, String)>,
    #[serde(default)]
    pub progress: SideEffectProgress,
    #[serde(default)]
    pub ambiguous: bool,
    #[serde(default)]
    pub failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub updated_at: String,
}

impl RoutingStateRecord {
    pub fn new(candidate: &RoutingCandidate, reservation: &ReservationRecord) -> Self {
        Self {
            reservation_key: reservation.reservation_key.clone(),
            execution_id: candidate.execution_id.clone(),
            required_event_type: candidate.required_event_type.clone(),
            policy_id: reservation.policy_id.clone(),
            policy_type: reservation.policy_type.clone(),
            policy_version: reservation.policy_version.clone(),
            decision: None,
            selected_transition: None,
            progress: SideEffectProgress::default(),
            ambiguous: false,
            failed: false,
            last_error: None,
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    pub fn mark_error(&mut self, error: impl Into<String>, ambiguous: bool) {
        self.failed = true;
        self.ambiguous = ambiguous;
        self.last_error = Some(error.into());
        self.updated_at = Utc::now().to_rfc3339();
    }

    pub fn clear_error(&mut self) {
        self.failed = false;
        self.ambiguous = false;
        self.last_error = None;
        self.updated_at = Utc::now().to_rfc3339();
    }

    pub fn mark_progress(&mut self, progress: SideEffectProgress) {
        self.progress = progress;
        self.updated_at = Utc::now().to_rfc3339();
    }
}

pub fn reservation_store_key(reservation_key: &str) -> String {
    format!("{RESERVATION_PREFIX}{reservation_key}")
}

pub fn routing_state_store_key(reservation_key: &str) -> String {
    format!("{ROUTING_STATE_PREFIX}{reservation_key}")
}

pub async fn load_reservation(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    reservation_key: &str,
) -> anyhow::Result<Option<ReservationRecord>> {
    let key = reservation_store_key(reservation_key);
    let Some(bytes) = store
        .get(store_id, &key)
        .await
        .map_err(|e| anyhow::anyhow!("state-store get reservation failed: {e}"))?
    else {
        return Ok(None);
    };
    serde_json::from_slice::<ReservationRecord>(&bytes)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("failed to deserialize reservation record: {e}"))
}

pub async fn save_reservation(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    record: &ReservationRecord,
) -> anyhow::Result<()> {
    let key = reservation_store_key(&record.reservation_key);
    let bytes = serde_json::to_vec(record)
        .map_err(|e| anyhow::anyhow!("failed to serialize reservation record: {e}"))?;
    store
        .set(store_id, &key, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("state-store set reservation failed: {e}"))?;
    Ok(())
}

pub async fn create_reservation_if_absent(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    record: &ReservationRecord,
) -> anyhow::Result<Option<ReservationRecord>> {
    let key = reservation_store_key(&record.reservation_key);
    let bytes = serde_json::to_vec(record)
        .map_err(|e| anyhow::anyhow!("failed to serialize reservation record: {e}"))?;
    let outcome = store
        .create_if_absent(store_id, &key, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("state-store create-if-absent reservation failed: {e}"))?;
    match outcome {
        StateStoreCreateIfAbsentResult::Created => Ok(None),
        StateStoreCreateIfAbsentResult::Existing(existing_bytes) => {
            deserialize_reservation(existing_bytes).map(Some)
        }
    }
}

pub async fn load_routing_state(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    reservation_key: &str,
) -> anyhow::Result<Option<RoutingStateRecord>> {
    let key = routing_state_store_key(reservation_key);
    let Some(bytes) = store
        .get(store_id, &key)
        .await
        .map_err(|e| anyhow::anyhow!("state-store get routing-state failed: {e}"))?
    else {
        return Ok(None);
    };
    serde_json::from_slice::<RoutingStateRecord>(&bytes)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("failed to deserialize routing-state record: {e}"))
}

pub async fn save_routing_state(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    record: &RoutingStateRecord,
) -> anyhow::Result<()> {
    let key = routing_state_store_key(&record.reservation_key);
    let bytes = serde_json::to_vec(record)
        .map_err(|e| anyhow::anyhow!("failed to serialize routing-state record: {e}"))?;
    store
        .set(store_id, &key, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("state-store set routing-state failed: {e}"))?;
    Ok(())
}

fn deserialize_reservation(bytes: Vec<u8>) -> anyhow::Result<ReservationRecord> {
    serde_json::from_slice::<ReservationRecord>(&bytes)
        .map_err(|e| anyhow::anyhow!("failed to deserialize reservation record: {e}"))
}
