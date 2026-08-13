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
use drasi_lib::state_store::{StateStoreCompareAndSwapResult, StateStoreProvider};
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
    #[serde(default = "default_fencing_epoch")]
    pub fencing_epoch: u64,
    #[serde(default)]
    pub lease_expires_at_unix_secs: i64,
    pub policy_id: String,
    pub policy_type: String,
    pub policy_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    pub created_at: String,
    pub completed: bool,
}

fn default_fencing_epoch() -> u64 {
    1
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_instance_id: Option<String>,
    #[serde(default)]
    pub fencing_epoch: u64,
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
    pub failure_fencing_epoch: Option<u64>,
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
            owner_instance_id: reservation.owner_instance_id.clone(),
            fencing_epoch: reservation.fencing_epoch,
            policy_id: reservation.policy_id.clone(),
            policy_type: reservation.policy_type.clone(),
            policy_version: reservation.policy_version.clone(),
            decision: None,
            selected_transition: None,
            progress: SideEffectProgress::default(),
            ambiguous: false,
            failed: false,
            failure_fencing_epoch: None,
            last_error: None,
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    pub fn mark_error(&mut self, error: impl Into<String>, ambiguous: bool) {
        self.failed = true;
        self.ambiguous = ambiguous;
        self.failure_fencing_epoch = None;
        self.last_error = Some(error.into());
        self.updated_at = Utc::now().to_rfc3339();
    }

    pub fn mark_error_with_epoch(
        &mut self,
        error: impl Into<String>,
        ambiguous: bool,
        fencing_epoch: u64,
    ) {
        self.mark_error(error, ambiguous);
        self.failure_fencing_epoch = Some(fencing_epoch);
    }

    pub fn clear_error(&mut self) {
        self.failed = false;
        self.ambiguous = false;
        self.failure_fencing_epoch = None;
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

#[derive(Debug, Clone)]
pub struct PersistedReservationRecord {
    pub record: ReservationRecord,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PersistedRoutingStateRecord {
    pub record: RoutingStateRecord,
    pub bytes: Vec<u8>,
}

pub async fn load_reservation(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    reservation_key: &str,
) -> anyhow::Result<Option<ReservationRecord>> {
    Ok(
        load_reservation_with_bytes(store, store_id, reservation_key)
            .await?
            .map(|persisted| persisted.record),
    )
}

pub async fn load_reservation_with_bytes(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    reservation_key: &str,
) -> anyhow::Result<Option<PersistedReservationRecord>> {
    let key = reservation_store_key(reservation_key);
    let Some(bytes) = store
        .get(store_id, &key)
        .await
        .map_err(|e| anyhow::anyhow!("state-store get reservation failed: {e}"))?
    else {
        return Ok(None);
    };
    let record = serde_json::from_slice::<ReservationRecord>(&bytes)
        .map_err(|e| anyhow::anyhow!("failed to deserialize reservation record: {e}"))?;
    Ok(Some(PersistedReservationRecord { record, bytes }))
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
    let bytes = serialize_reservation(record)?;
    let swapped = store
        .compare_and_swap(store_id, &key, None, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("state-store CAS reservation-create failed: {e}"))?;
    match swapped {
        StateStoreCompareAndSwapResult::Swapped => Ok(None),
        StateStoreCompareAndSwapResult::Mismatch => {
            let Some(existing) = load_reservation(store, store_id, &record.reservation_key).await?
            else {
                anyhow::bail!("state-store CAS reservation-create mismatched but value vanished");
            };
            Ok(Some(existing))
        }
    }
}

pub async fn compare_and_swap_reservation(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    reservation_key: &str,
    expected: Option<&[u8]>,
    new_record: &ReservationRecord,
) -> anyhow::Result<bool> {
    let key = reservation_store_key(reservation_key);
    let bytes = serialize_reservation(new_record)?;
    let outcome = store
        .compare_and_swap(store_id, &key, expected, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("state-store CAS reservation failed: {e}"))?;
    Ok(matches!(outcome, StateStoreCompareAndSwapResult::Swapped))
}

pub async fn load_routing_state(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    reservation_key: &str,
) -> anyhow::Result<Option<RoutingStateRecord>> {
    Ok(
        load_routing_state_with_bytes(store, store_id, reservation_key)
            .await?
            .map(|persisted| persisted.record),
    )
}

pub async fn load_routing_state_with_bytes(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    reservation_key: &str,
) -> anyhow::Result<Option<PersistedRoutingStateRecord>> {
    let key = routing_state_store_key(reservation_key);
    let Some(bytes) = store
        .get(store_id, &key)
        .await
        .map_err(|e| anyhow::anyhow!("state-store get routing-state failed: {e}"))?
    else {
        return Ok(None);
    };
    let record = serde_json::from_slice::<RoutingStateRecord>(&bytes)
        .map_err(|e| anyhow::anyhow!("failed to deserialize routing-state record: {e}"))?;
    Ok(Some(PersistedRoutingStateRecord { record, bytes }))
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

pub async fn compare_and_swap_routing_state(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    reservation_key: &str,
    expected: Option<&[u8]>,
    new_record: &RoutingStateRecord,
) -> anyhow::Result<bool> {
    let key = routing_state_store_key(reservation_key);
    let bytes = serde_json::to_vec(new_record)
        .map_err(|e| anyhow::anyhow!("failed to serialize routing-state record: {e}"))?;
    let outcome = store
        .compare_and_swap(store_id, &key, expected, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("state-store CAS routing-state failed: {e}"))?;
    Ok(matches!(outcome, StateStoreCompareAndSwapResult::Swapped))
}

pub fn serialize_reservation(record: &ReservationRecord) -> anyhow::Result<Vec<u8>> {
    serde_json::to_vec(record)
        .map_err(|e| anyhow::anyhow!("failed to serialize reservation record: {e}"))
}

pub fn deserialize_reservation(bytes: &[u8]) -> anyhow::Result<ReservationRecord> {
    serde_json::from_slice::<ReservationRecord>(bytes)
        .map_err(|e| anyhow::anyhow!("failed to deserialize reservation record: {e}"))
}
