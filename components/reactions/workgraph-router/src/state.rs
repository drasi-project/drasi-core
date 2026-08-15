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

//! Durable routing state.
//!
//! Exactly one record exists per `runId`, and it is written **before** the
//! first GitHub write. That ordering is what makes recovery decidable: if a
//! record exists, routing may already have had an external effect, so the
//! reaction reconciles against GitHub rather than retrying blindly.
//!
//! The record also pins the decision's inputs *and* its intended output:
//!
//! * the **accepted completion comment** — its immutable node ID *and* the
//!   SHA-256 of the exact body that was accepted — so a completion edited after
//!   the fact can never quietly change what the router already decided;
//! * the decided destination status and outcome, so a resumed run finishes the
//!   same decision instead of re-deriving one; and
//! * the **canonical JSON of the `RoutingDecided` event** the run intends to
//!   publish, so a resumed run can verify the published decision comment
//!   without re-deriving anything from live state.
//!
//! A second key — the **open-run pointer** for a Project item — maps the item
//! to the run that currently owns it. It is written before the first GitHub
//! write, and it is what lets a resumed run find its own published decision
//! even when the issue body (and therefore the `runId` a fresh derivation would
//! produce) has changed since publication.
//!
//! Every mutation is an exact-bytes compare-and-swap, so a stale in-memory copy
//! can never clobber newer progress.

use std::sync::Arc;

use chrono::Utc;
use drasi_lib::state_store::{StateStoreCompareAndSwapResult, StateStoreProvider};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::candidate::RoutingCandidate;

/// State-store key prefix for routing records.
pub const ROUTING_RECORD_PREFIX: &str = "workgraph-router/runs/";

/// State-store key prefix for the per-Project-item open-run pointer.
pub const ROUTING_ITEM_PREFIX: &str = "workgraph-router/items/";

/// Schema version of the persisted record.
pub const ROUTING_RECORD_SCHEMA: &str = "workgraph.routing-record/v1";

/// Schema version of the persisted open-run pointer.
pub const ROUTING_ITEM_SCHEMA: &str = "workgraph.routing-item/v1";

/// Digest the exact bytes of an accepted comment body.
///
/// Used to detect an edit to a comment the router has already accepted as the
/// authoritative input to a decision.
pub fn comment_body_hash(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// The physical comment the router accepted as the completion for a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AcceptedCompletion {
    /// The immutable comment node ID that carried the completion event.
    pub comment_node_id: String,
    /// SHA-256 of the exact accepted comment body.
    pub body_hash: String,
}

/// The durable record for one routing decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingRecord {
    /// Record schema version.
    pub schema_version: String,
    /// The deterministic run identifier this record routes.
    pub run_id: String,
    /// The deterministic `RoutingDecided` event identifier.
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
    /// The issue-body digest this run is bound to.
    pub content_digest: String,
    /// The completion comment the decision was derived from.
    pub accepted_completion: AcceptedCompletion,
    /// The validation outcome that produced the decision.
    pub outcome: String,
    /// The status the item is being routed to.
    pub to_status: String,
    /// Canonical JSON of the `RoutingDecided` event this run publishes.
    ///
    /// A resumed run compares the published decision comment against these
    /// exact bytes, so the comparison never depends on re-deriving anything
    /// from live GitHub state.
    pub decision_event_json: String,
    /// The decision comment node ID, once that comment is durable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_comment_node_id: Option<String>,
    /// Whether the Project status has been observed at the decided status.
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

impl RoutingRecord {
    /// Build the initial (intent) record for a run.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: &str,
        event_id: &str,
        candidate: &RoutingCandidate,
        content_digest: &str,
        accepted_completion: AcceptedCompletion,
        outcome: &str,
        to_status: &str,
        decision_event_json: &str,
    ) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            schema_version: ROUTING_RECORD_SCHEMA.to_string(),
            run_id: run_id.to_string(),
            event_id: event_id.to_string(),
            repository: candidate.repository.clone(),
            subject_number: candidate.subject_number,
            subject_node_id: candidate.subject_node_id.clone(),
            project_node_id: candidate.project_node_id.clone(),
            project_item_node_id: candidate.project_item_node_id.clone(),
            content_digest: content_digest.to_string(),
            accepted_completion,
            outcome: outcome.to_string(),
            to_status: to_status.to_string(),
            decision_event_json: decision_event_json.to_string(),
            decision_comment_node_id: None,
            status_applied: false,
            ambiguous: false,
            last_error: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Whether both side effects are durably recorded.
    pub fn is_complete(&self) -> bool {
        self.decision_comment_node_id.is_some() && self.status_applied
    }

    /// Whether the decision comment is durably published but the final status
    /// move has not been applied yet.
    ///
    /// A run in this state must be finished from the persisted decision, never
    /// re-derived from live state: the decision is already visible in the issue
    /// thread, so abandoning it would strand the item.
    pub fn is_published_but_unapplied(&self) -> bool {
        self.decision_comment_node_id.is_some() && !self.status_applied
    }

    /// Reject a record that describes a different subject than the current row.
    ///
    /// The `runId` binds the Project Item, the subject, and the body digest, so
    /// a mismatch here means the state store has been corrupted or a `runId`
    /// collision occurred; either way, writing again would be unsafe.
    pub fn ensure_matches(&self, candidate: &RoutingCandidate) -> anyhow::Result<()> {
        if self.schema_version != ROUTING_RECORD_SCHEMA {
            anyhow::bail!(
                "routing record schema '{}' is not '{ROUTING_RECORD_SCHEMA}'",
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
                "routing record for run '{}' describes {}#{} on item '{}', which does not match this row",
                self.run_id,
                self.repository,
                self.subject_number,
                self.project_item_node_id
            );
        }
        Ok(())
    }

    /// Reject a decision whose inputs changed since it was recorded.
    ///
    /// A run is decided exactly once. If the completion comment the decision
    /// was derived from has been edited, or a different physical comment now
    /// claims the completion, the router must not proceed: the recorded
    /// decision may no longer reflect what the issue thread says.
    pub fn ensure_decision_inputs_unchanged(
        &self,
        observed: &AcceptedCompletion,
        outcome: &str,
        to_status: &str,
    ) -> anyhow::Result<()> {
        if self.accepted_completion.comment_node_id != observed.comment_node_id {
            anyhow::bail!(
                "run '{}' was decided from completion comment '{}' but comment '{}' now claims the completion",
                self.run_id,
                self.accepted_completion.comment_node_id,
                observed.comment_node_id
            );
        }
        if self.accepted_completion.body_hash != observed.body_hash {
            anyhow::bail!(
                "completion comment '{}' for run '{}' was edited after it was accepted; refusing to route",
                self.accepted_completion.comment_node_id,
                self.run_id
            );
        }
        if self.outcome != outcome || self.to_status != to_status {
            anyhow::bail!(
                "run '{}' was decided as '{}' -> '{}' but now derives '{outcome}' -> '{to_status}'",
                self.run_id,
                self.outcome,
                self.to_status
            );
        }
        Ok(())
    }

    fn touch(&mut self) {
        self.updated_at = Utc::now().to_rfc3339();
    }

    /// Record the comment that carries the routing decision.
    pub fn set_decision_comment(&mut self, comment_node_id: impl Into<String>) {
        self.decision_comment_node_id = Some(comment_node_id.into());
        self.ambiguous = false;
        self.last_error = None;
        self.touch();
    }

    /// Record that the Project status is at the decided status.
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
pub struct PersistedRoutingRecord {
    /// The decoded record.
    pub record: RoutingRecord,
    /// The exact bytes in the store, used as the compare-and-swap witness.
    pub bytes: Vec<u8>,
}

/// The state-store key for a run.
pub fn record_key(run_id: &str) -> String {
    format!("{ROUTING_RECORD_PREFIX}{run_id}")
}

fn serialize(record: &RoutingRecord) -> anyhow::Result<Vec<u8>> {
    serde_json::to_vec(record)
        .map_err(|error| anyhow::anyhow!("failed to serialize routing record: {error}"))
}

/// Load the record for a run, if any.
pub async fn load_record(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    run_id: &str,
) -> anyhow::Result<Option<PersistedRoutingRecord>> {
    let key = record_key(run_id);
    let Some(bytes) = store
        .get(store_id, &key)
        .await
        .map_err(|error| anyhow::anyhow!("state-store get routing record failed: {error}"))?
    else {
        return Ok(None);
    };
    let record = serde_json::from_slice::<RoutingRecord>(&bytes)
        .map_err(|error| anyhow::anyhow!("failed to deserialize routing record: {error}"))?;
    Ok(Some(PersistedRoutingRecord { record, bytes }))
}

/// Create the record only if none exists.
///
/// Returns the already-persisted record when another writer won the race.
pub async fn create_record_if_absent(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    record: &RoutingRecord,
) -> anyhow::Result<Option<PersistedRoutingRecord>> {
    let key = record_key(&record.run_id);
    let bytes = serialize(record)?;
    let outcome = store
        .compare_and_swap(store_id, &key, None, bytes)
        .await
        .map_err(|error| anyhow::anyhow!("state-store CAS routing-create failed: {error}"))?;
    match outcome {
        StateStoreCompareAndSwapResult::Swapped => Ok(None),
        StateStoreCompareAndSwapResult::Mismatch => {
            let existing = load_record(store, store_id, &record.run_id)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("routing record vanished after a CAS create mismatch")
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
    record: &RoutingRecord,
) -> anyhow::Result<Option<Vec<u8>>> {
    let key = record_key(&record.run_id);
    let bytes = serialize(record)?;
    let outcome = store
        .compare_and_swap(store_id, &key, Some(expected), bytes.clone())
        .await
        .map_err(|error| anyhow::anyhow!("state-store CAS routing-update failed: {error}"))?;
    match outcome {
        StateStoreCompareAndSwapResult::Swapped => Ok(Some(bytes)),
        StateStoreCompareAndSwapResult::Mismatch => Ok(None),
    }
}

/// The run that currently owns a Project item.
///
/// Written before the first GitHub write so that a later attempt can find the
/// run even if the issue body — and therefore a freshly derived `runId` — has
/// changed in the meantime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct OpenRunPointer {
    /// Pointer schema version.
    pub schema_version: String,
    /// The run that owns the item.
    pub run_id: String,
}

impl OpenRunPointer {
    /// Build a pointer to `run_id`.
    pub fn new(run_id: &str) -> Self {
        Self {
            schema_version: ROUTING_ITEM_SCHEMA.to_string(),
            run_id: run_id.to_string(),
        }
    }
}

/// The state-store key for a Project item's open-run pointer.
pub fn item_key(project_item_node_id: &str) -> String {
    format!("{ROUTING_ITEM_PREFIX}{project_item_node_id}")
}

/// Point `project_item_node_id` at `run_id`, replacing any earlier pointer.
///
/// Idempotent: writing the same pointer twice is a no-op in effect.
pub async fn set_open_run(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    project_item_node_id: &str,
    run_id: &str,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(&OpenRunPointer::new(run_id))
        .map_err(|error| anyhow::anyhow!("failed to serialize open-run pointer: {error}"))?;
    store
        .set(store_id, &item_key(project_item_node_id), bytes)
        .await
        .map_err(|error| anyhow::anyhow!("state-store set open-run pointer failed: {error}"))
}

/// The run currently pointed at by `project_item_node_id`, if any.
pub async fn load_open_run(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    project_item_node_id: &str,
) -> anyhow::Result<Option<String>> {
    let Some(bytes) = store
        .get(store_id, &item_key(project_item_node_id))
        .await
        .map_err(|error| anyhow::anyhow!("state-store get open-run pointer failed: {error}"))?
    else {
        return Ok(None);
    };
    let pointer = serde_json::from_slice::<OpenRunPointer>(&bytes)
        .map_err(|error| anyhow::anyhow!("failed to deserialize open-run pointer: {error}"))?;
    if pointer.schema_version != ROUTING_ITEM_SCHEMA {
        anyhow::bail!(
            "open-run pointer schema '{}' is not '{ROUTING_ITEM_SCHEMA}'",
            pointer.schema_version
        );
    }
    Ok(Some(pointer.run_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ROUTABLE_STATUS;
    use drasi_lib::state_store::MemoryStateStoreProvider;

    fn candidate() -> RoutingCandidate {
        RoutingCandidate {
            repository: "drasi-project/drasi-core".to_string(),
            subject_number: 742,
            subject_node_id: "I_subject".to_string(),
            project_node_id: "PVT_project".to_string(),
            project_item_node_id: "PVTI_item".to_string(),
            project_status: ROUTABLE_STATUS.to_string(),
        }
    }

    fn accepted() -> AcceptedCompletion {
        AcceptedCompletion {
            comment_node_id: "IC_completion".to_string(),
            body_hash: comment_body_hash("WorkGraphEvent/v1\n\nsummary\n\n{}"),
        }
    }

    const DECISION_JSON: &str = r#"{"schemaVersion":"workgraph.event/v1","eventId":"event:abc"}"#;

    fn record() -> RoutingRecord {
        RoutingRecord::new(
            "run:abc",
            "event:abc",
            &candidate(),
            "sha256:abc",
            accepted(),
            "passed",
            "AwaitingIssueRiskProfiling",
            DECISION_JSON,
        )
    }

    #[test]
    fn body_hashes_are_stable_and_sensitive() {
        assert_eq!(comment_body_hash("a"), comment_body_hash("a"));
        assert_ne!(comment_body_hash("a"), comment_body_hash("a "));
        assert!(comment_body_hash("a").starts_with("sha256:"));
        assert_eq!(comment_body_hash("").len(), "sha256:".len() + 64);
    }

    #[tokio::test]
    async fn create_is_idempotent_and_reports_the_existing_record() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let first = create_record_if_absent(store.clone(), "router", &record())
            .await
            .expect("first create");
        assert!(first.is_none(), "first create wins");

        let second = create_record_if_absent(store.clone(), "router", &record())
            .await
            .expect("second create")
            .expect("existing record is returned");
        assert_eq!(second.record.run_id, "run:abc");
        assert!(!second.record.is_complete());
    }

    #[tokio::test]
    async fn stale_witnesses_cannot_clobber_newer_progress() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        create_record_if_absent(store.clone(), "router", &record())
            .await
            .expect("create");
        let loaded = load_record(store.clone(), "router", "run:abc")
            .await
            .expect("load")
            .expect("exists");

        let mut advanced = loaded.record.clone();
        advanced.set_decision_comment("IC_1");
        let new_bytes = compare_and_swap_record(store.clone(), "router", &loaded.bytes, &advanced)
            .await
            .expect("cas")
            .expect("swap succeeds");

        let mut stale = loaded.record.clone();
        stale.set_decision_comment("IC_2");
        assert!(
            compare_and_swap_record(store.clone(), "router", &loaded.bytes, &stale)
                .await
                .expect("cas")
                .is_none(),
            "stale witness must not overwrite newer progress"
        );

        let mut completed = advanced.clone();
        completed.set_status_applied();
        assert!(
            compare_and_swap_record(store.clone(), "router", &new_bytes, &completed)
                .await
                .expect("cas")
                .is_some(),
            "fresh witness succeeds"
        );
        let final_record = load_record(store, "router", "run:abc")
            .await
            .expect("load")
            .expect("exists")
            .record;
        assert_eq!(
            final_record.decision_comment_node_id.as_deref(),
            Some("IC_1")
        );
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
        wrong_schema.schema_version = "workgraph.routing-record/v0".to_string();
        assert!(wrong_schema.ensure_matches(&candidate()).is_err());
    }

    #[test]
    fn an_edited_completion_comment_stops_a_resumed_decision() {
        let record = record();
        record
            .ensure_decision_inputs_unchanged(&accepted(), "passed", "AwaitingIssueRiskProfiling")
            .expect("unchanged inputs");

        let edited = AcceptedCompletion {
            comment_node_id: "IC_completion".to_string(),
            body_hash: comment_body_hash("WorkGraphEvent/v1\n\nedited\n\n{}"),
        };
        assert!(record
            .ensure_decision_inputs_unchanged(&edited, "passed", "AwaitingIssueRiskProfiling")
            .expect_err("edited body")
            .to_string()
            .contains("edited after it was accepted"));

        let different_comment = AcceptedCompletion {
            comment_node_id: "IC_other".to_string(),
            body_hash: accepted().body_hash,
        };
        assert!(record
            .ensure_decision_inputs_unchanged(
                &different_comment,
                "passed",
                "AwaitingIssueRiskProfiling"
            )
            .expect_err("different comment")
            .to_string()
            .contains("now claims the completion"));

        assert!(record
            .ensure_decision_inputs_unchanged(&accepted(), "failed", "NeedsMoreInformation")
            .expect_err("changed outcome")
            .to_string()
            .contains("was decided as"));
    }

    #[test]
    fn error_and_progress_transitions_clear_ambiguity() {
        let mut record = record();
        record.set_error("transport failure", true);
        assert!(record.ambiguous);
        assert!(!record.is_published_but_unapplied());
        record.set_decision_comment("IC_1");
        assert!(!record.ambiguous);
        assert!(record.last_error.is_none());
        assert!(!record.is_complete());
        assert!(
            record.is_published_but_unapplied(),
            "a published decision that has not moved the status must be resumable"
        );
        record.set_status_applied();
        assert!(record.is_complete());
        assert!(!record.is_published_but_unapplied());
    }

    #[test]
    fn the_intended_decision_event_is_pinned_by_the_record() {
        assert_eq!(record().decision_event_json, DECISION_JSON);
    }

    #[tokio::test]
    async fn the_open_run_pointer_survives_a_changed_run_id() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        assert!(load_open_run(store.clone(), "router", "PVTI_item")
            .await
            .expect("load")
            .is_none());

        set_open_run(store.clone(), "router", "PVTI_item", "run:abc")
            .await
            .expect("set pointer");
        assert_eq!(
            load_open_run(store.clone(), "router", "PVTI_item")
                .await
                .expect("load"),
            Some("run:abc".to_string())
        );

        // A later run for the same item replaces the pointer.
        set_open_run(store.clone(), "router", "PVTI_item", "run:def")
            .await
            .expect("replace pointer");
        assert_eq!(
            load_open_run(store.clone(), "router", "PVTI_item")
                .await
                .expect("load"),
            Some("run:def".to_string())
        );

        // Another item is unaffected.
        assert!(load_open_run(store, "router", "PVTI_other")
            .await
            .expect("load")
            .is_none());
    }

    #[tokio::test]
    async fn an_unknown_pointer_schema_is_rejected() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        store
            .set(
                "router",
                &item_key("PVTI_item"),
                serde_json::to_vec(&serde_json::json!({
                    "schemaVersion": "workgraph.routing-item/v0",
                    "runId": "run:abc"
                }))
                .expect("serialize"),
            )
            .await
            .expect("seed");
        assert!(load_open_run(store, "router", "PVTI_item").await.is_err());
    }
}
