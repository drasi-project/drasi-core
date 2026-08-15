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

//! Durable admission state.
//!
//! Exactly one record exists per `runId`, and it is written **before** the first
//! GitHub write. That ordering is what makes recovery decidable: if a record
//! exists, admission may already have had an external effect, so the reaction
//! reconciles against GitHub rather than retrying blindly.
//!
//! Every mutation is an exact-bytes compare-and-swap, so a stale in-memory copy
//! can never clobber newer progress.

use std::sync::Arc;

use chrono::Utc;
use drasi_lib::state_store::{StateStoreCompareAndSwapResult, StateStoreProvider};
use serde::{Deserialize, Serialize};

use crate::candidate::AdmissionCandidate;

/// State-store key prefix for admission records.
pub const ADMISSION_RECORD_PREFIX: &str = "workgraph-admission/runs/";

/// Schema version of the persisted record.
pub const ADMISSION_RECORD_SCHEMA: &str = "workgraph.admission-record/v1";

/// The durable record for one admission run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionRecord {
    /// Record schema version.
    pub schema_version: String,
    /// The deterministic run identifier this record admits.
    pub run_id: String,
    /// The deterministic `ResponsibilityAssigned` event identifier.
    pub event_id: String,
    /// `owner/repo` of the subject issue.
    pub repository: String,
    /// The subject issue number.
    pub subject_number: u64,
    /// The subject issue node ID.
    pub subject_node_id: String,
    /// The Project node ID.
    pub project_node_id: String,
    /// The Project item node ID.
    pub project_item_node_id: String,
    /// The accepted issue-body digest this run is bound to.
    pub content_digest: String,
    /// The blob-pinned profile reference carried by the assignment.
    pub profile_ref: String,
    /// The comment node ID, once the assignment comment is durable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_node_id: Option<String>,
    /// Whether the Project status has been observed at the admitted status.
    #[serde(default)]
    pub status_applied: bool,
    /// Whether a write outcome is unknown and must be reconciled.
    #[serde(default)]
    pub ambiguous: bool,
    /// The last error observed for this run, for operator diagnosis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// When the record was first written.
    pub created_at: String,
    /// When the record was last written.
    pub updated_at: String,
}

impl AdmissionRecord {
    /// Build the initial (intent) record for a run.
    pub fn new(
        run_id: &str,
        event_id: &str,
        candidate: &AdmissionCandidate,
        content_digest: &str,
        profile_ref: &str,
    ) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            schema_version: ADMISSION_RECORD_SCHEMA.to_string(),
            run_id: run_id.to_string(),
            event_id: event_id.to_string(),
            repository: candidate.repository.clone(),
            subject_number: candidate.subject_number,
            subject_node_id: candidate.subject_node_id.clone(),
            project_node_id: candidate.project_node_id.clone(),
            project_item_node_id: candidate.project_item_node_id.clone(),
            content_digest: content_digest.to_string(),
            profile_ref: profile_ref.to_string(),
            comment_node_id: None,
            status_applied: false,
            ambiguous: false,
            last_error: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Whether both side effects are durably recorded.
    pub fn is_complete(&self) -> bool {
        self.comment_node_id.is_some() && self.status_applied
    }

    /// Reject a record that describes a different subject than the current row.
    ///
    /// The `runId` binds the Project Item, the subject, and the body digest, so
    /// a mismatch here means the state store has been corrupted or a `runId`
    /// collision occurred; either way, writing again would be unsafe.
    pub fn ensure_matches(&self, candidate: &AdmissionCandidate) -> anyhow::Result<()> {
        if self.schema_version != ADMISSION_RECORD_SCHEMA {
            anyhow::bail!(
                "admission record schema '{}' is not '{ADMISSION_RECORD_SCHEMA}'",
                self.schema_version
            );
        }
        if self.repository != candidate.repository
            || self.subject_number != candidate.subject_number
            || self.subject_node_id != candidate.subject_node_id
            || self.project_node_id != candidate.project_node_id
            || self.project_item_node_id != candidate.project_item_node_id
        {
            anyhow::bail!(
                "admission record for run '{}' describes {}#{} on item '{}', which does not match this row",
                self.run_id,
                self.repository,
                self.subject_number,
                self.project_item_node_id
            );
        }
        Ok(())
    }

    /// Reject a record that is not the record for this exact run.
    ///
    /// A record is loaded by `runId`, so a disagreeing `runId` or
    /// `contentDigest` means the store is corrupt or a key collided. Either way
    /// the record can no longer be trusted to describe the run being resumed,
    /// and resuming it would risk publishing an assignment for a different body.
    pub fn ensure_bound_to(
        &self,
        candidate: &AdmissionCandidate,
        run_id: &str,
        content_digest: &str,
    ) -> anyhow::Result<()> {
        self.ensure_matches(candidate)?;
        if self.run_id != run_id {
            anyhow::bail!(
                "admission record stored for run '{run_id}' declares run '{}'",
                self.run_id
            );
        }
        if self.content_digest != content_digest {
            anyhow::bail!(
                "admission record for run '{}' is bound to content digest '{}', not the authoritative '{content_digest}'",
                self.run_id,
                self.content_digest
            );
        }
        Ok(())
    }

    fn touch(&mut self) {
        self.updated_at = Utc::now().to_rfc3339();
    }

    /// Record the comment that carries the assignment.
    pub fn set_comment(&mut self, comment_node_id: impl Into<String>) {
        self.comment_node_id = Some(comment_node_id.into());
        self.ambiguous = false;
        self.last_error = None;
        self.touch();
    }

    /// Record that the Project status is at the admitted status.
    pub fn set_status_applied(&mut self) {
        self.status_applied = true;
        self.ambiguous = false;
        self.last_error = None;
        self.touch();
    }

    /// Record an error, optionally marking the run as ambiguous.
    pub fn set_error(&mut self, error: impl Into<String>, ambiguous: bool) {
        self.last_error = Some(error.into());
        self.ambiguous = ambiguous;
        self.touch();
    }
}

/// A record together with the exact bytes it was loaded from.
#[derive(Debug, Clone)]
pub struct PersistedAdmissionRecord {
    /// The decoded record.
    pub record: AdmissionRecord,
    /// The exact bytes in the store, used as the compare-and-swap witness.
    pub bytes: Vec<u8>,
}

/// The state-store key for a run.
pub fn record_key(run_id: &str) -> String {
    format!("{ADMISSION_RECORD_PREFIX}{run_id}")
}

fn serialize(record: &AdmissionRecord) -> anyhow::Result<Vec<u8>> {
    serde_json::to_vec(record)
        .map_err(|error| anyhow::anyhow!("failed to serialize admission record: {error}"))
}

/// Load the record for a run, if any.
pub async fn load_record(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    run_id: &str,
) -> anyhow::Result<Option<PersistedAdmissionRecord>> {
    let key = record_key(run_id);
    let Some(bytes) = store
        .get(store_id, &key)
        .await
        .map_err(|error| anyhow::anyhow!("state-store get admission record failed: {error}"))?
    else {
        return Ok(None);
    };
    let record = serde_json::from_slice::<AdmissionRecord>(&bytes)
        .map_err(|error| anyhow::anyhow!("failed to deserialize admission record: {error}"))?;
    Ok(Some(PersistedAdmissionRecord { record, bytes }))
}

/// Create the record only if none exists.
///
/// Returns the already-persisted record when another writer won the race.
pub async fn create_record_if_absent(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    record: &AdmissionRecord,
) -> anyhow::Result<Option<PersistedAdmissionRecord>> {
    let key = record_key(&record.run_id);
    let bytes = serialize(record)?;
    let outcome = store
        .compare_and_swap(store_id, &key, None, bytes)
        .await
        .map_err(|error| anyhow::anyhow!("state-store CAS admission-create failed: {error}"))?;
    match outcome {
        StateStoreCompareAndSwapResult::Swapped => Ok(None),
        StateStoreCompareAndSwapResult::Mismatch => {
            let existing = load_record(store, store_id, &record.run_id)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("admission record vanished after a CAS create mismatch")
                })?;
            Ok(Some(existing))
        }
    }
}

/// Replace the record only if it still holds `expected` bytes.
pub async fn compare_and_swap_record(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    expected: &[u8],
    record: &AdmissionRecord,
) -> anyhow::Result<Option<Vec<u8>>> {
    let key = record_key(&record.run_id);
    let bytes = serialize(record)?;
    let outcome = store
        .compare_and_swap(store_id, &key, Some(expected), bytes.clone())
        .await
        .map_err(|error| anyhow::anyhow!("state-store CAS admission-update failed: {error}"))?;
    match outcome {
        StateStoreCompareAndSwapResult::Swapped => Ok(Some(bytes)),
        StateStoreCompareAndSwapResult::Mismatch => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::state_store::MemoryStateStoreProvider;

    fn candidate() -> AdmissionCandidate {
        AdmissionCandidate {
            repository: "drasi-project/drasi-core".to_string(),
            subject_number: 742,
            subject_node_id: "I_subject".to_string(),
            project_node_id: "PVT_project".to_string(),
            project_item_node_id: "PVTI_item".to_string(),
            project_status: "Triage".to_string(),
        }
    }

    fn record() -> AdmissionRecord {
        AdmissionRecord::new(
            "run:abc",
            "event:abc",
            &candidate(),
            "sha256:abc",
            "issue-validator@0123456789abcdef0123456789abcdef01234567",
        )
    }

    #[tokio::test]
    async fn create_is_idempotent_and_reports_the_existing_record() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let first = create_record_if_absent(store.clone(), "admission", &record())
            .await
            .expect("first create");
        assert!(first.is_none(), "first create wins");

        let second = create_record_if_absent(store.clone(), "admission", &record())
            .await
            .expect("second create")
            .expect("existing record is returned");
        assert_eq!(second.record.run_id, "run:abc");
        assert!(!second.record.is_complete());
    }

    #[tokio::test]
    async fn stale_witnesses_cannot_clobber_newer_progress() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        create_record_if_absent(store.clone(), "admission", &record())
            .await
            .expect("create");
        let loaded = load_record(store.clone(), "admission", "run:abc")
            .await
            .expect("load")
            .expect("exists");

        let mut advanced = loaded.record.clone();
        advanced.set_comment("IC_1");
        let new_bytes =
            compare_and_swap_record(store.clone(), "admission", &loaded.bytes, &advanced)
                .await
                .expect("cas")
                .expect("swap succeeds");

        // A writer holding the original bytes must lose.
        let mut stale = loaded.record.clone();
        stale.set_comment("IC_2");
        assert!(
            compare_and_swap_record(store.clone(), "admission", &loaded.bytes, &stale)
                .await
                .expect("cas")
                .is_none(),
            "stale witness must not overwrite newer progress"
        );

        let mut completed = advanced.clone();
        completed.set_status_applied();
        assert!(
            compare_and_swap_record(store.clone(), "admission", &new_bytes, &completed)
                .await
                .expect("cas")
                .is_some(),
            "fresh witness succeeds"
        );
        let final_record = load_record(store, "admission", "run:abc")
            .await
            .expect("load")
            .expect("exists")
            .record;
        assert_eq!(final_record.comment_node_id.as_deref(), Some("IC_1"));
        assert!(final_record.is_complete());
    }

    #[test]
    fn record_identity_is_checked_against_the_row() {
        let record = record();
        record.ensure_matches(&candidate()).expect("matching row");

        let mut other = candidate();
        other.project_item_node_id = "PVTI_other".to_string();
        assert!(record.ensure_matches(&other).is_err());

        let mut wrong_schema = record.clone();
        wrong_schema.schema_version = "workgraph.admission-record/v0".to_string();
        assert!(wrong_schema.ensure_matches(&candidate()).is_err());
    }

    #[test]
    fn a_record_must_be_bound_to_this_exact_run() {
        let record = record();
        record
            .ensure_bound_to(&candidate(), "run:abc", "sha256:abc")
            .expect("the intact record is bound to its own run");

        assert!(
            record
                .ensure_bound_to(&candidate(), "run:other", "sha256:abc")
                .is_err(),
            "a record stored under a different run must fail closed"
        );
        assert!(
            record
                .ensure_bound_to(&candidate(), "run:abc", "sha256:edited")
                .is_err(),
            "a record bound to another issue body must fail closed"
        );

        let mut other_row = candidate();
        other_row.project_item_node_id = "PVTI_other".to_string();
        assert!(
            record
                .ensure_bound_to(&other_row, "run:abc", "sha256:abc")
                .is_err(),
            "row binding is still enforced"
        );
    }

    #[test]
    fn error_and_progress_transitions_clear_ambiguity() {
        let mut record = record();
        record.set_error("transport failure", true);
        assert!(record.ambiguous);
        record.set_comment("IC_1");
        assert!(!record.ambiguous);
        assert!(record.last_error.is_none());
        assert!(!record.is_complete());
        record.set_status_applied();
        assert!(record.is_complete());
    }
}
