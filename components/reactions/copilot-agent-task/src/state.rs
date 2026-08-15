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

//! Durable execution state.
//!
//! Exactly one record exists per `runId`, keyed by
//! [`crate::ids::reservation_key`], and it is written **before** the first
//! external write (task creation, comment posting). That ordering is what makes
//! recovery decidable: if a record exists, the launch may already have had an
//! external effect, so the reaction reconciles against GitHub rather than
//! retrying blindly.
//!
//! ```text
//! Reserved -> TaskCreated -> Completed
//!      \-> Ambiguous (a write's outcome is unknown; needs reconciliation)
//!      \-> Failed    (a permanent create-task rejection; terminal)
//! ```
//!
//! Every mutation is an exact-bytes compare-and-swap, so a stale in-memory copy
//! can never clobber newer progress.

use std::sync::Arc;

use chrono::Utc;
use drasi_lib::state_store::{StateStoreCompareAndSwapResult, StateStoreProvider};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::row::LaunchRow;

/// Schema version of the persisted record.
pub const EXECUTION_RECORD_SCHEMA: &str = "workgraph.execution-record/v1";

/// Lifecycle status of one run's execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// The run is durably reserved; no external call has been made yet.
    Reserved,
    /// The agent task was confirmed created; the `ExecutionStarted` comment has
    /// not yet been durably recorded.
    TaskCreated,
    /// The `ExecutionStarted` comment is durable. Terminal success.
    Completed,
    /// A write's outcome is unknown (e.g. a transport error with no HTTP
    /// response). **Never** blindly retried; a restart reconciles against
    /// GitHub. See [`crate::github`].
    Ambiguous,
    /// A permanent create-task rejection means this run will never launch.
    /// Terminal.
    Failed,
}

/// The durable record for one execution run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRecord {
    /// Record schema version.
    pub schema_version: String,
    /// The deterministic run identifier this record launches.
    pub run_id: String,
    /// The deterministic `ExecutionStarted` event identifier.
    pub event_id: String,
    /// The stable execution identifier embedded in the prompt and the comment.
    pub execution_id: String,
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
    /// The model requested for the agent task.
    pub requested_model: String,
    /// The optional fallback model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    /// The git ref pinned before task creation.
    pub base_ref: String,
    /// The model of the in-flight or confirmed attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_used: Option<String>,
    /// Whether the confirmed/in-flight attempt used the fallback model.
    #[serde(default)]
    pub used_fallback: bool,
    /// The created task ID, once creation is confirmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// The created task URL, once creation is confirmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_url: Option<String>,
    /// The comment node ID, once the `ExecutionStarted` comment is durable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_node_id: Option<String>,
    /// The lifecycle status.
    pub status: ExecutionStatus,
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

impl ExecutionRecord {
    /// Build the initial (reserved) record for a run.
    pub fn new(
        run_id: &str,
        event_id: &str,
        execution_id: &str,
        row: &LaunchRow,
        content_digest: &str,
        profile_ref: &str,
    ) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            schema_version: EXECUTION_RECORD_SCHEMA.to_string(),
            run_id: run_id.to_string(),
            event_id: event_id.to_string(),
            execution_id: execution_id.to_string(),
            repository: row.repository.clone(),
            subject_number: row.subject_number,
            subject_node_id: row.subject_node_id.clone(),
            project_node_id: row.project_node_id.clone(),
            project_item_node_id: row.project_item_node_id.clone(),
            content_digest: content_digest.to_string(),
            profile_ref: profile_ref.to_string(),
            requested_model: row.requested_model.clone(),
            fallback_model: row.fallback_model.clone(),
            base_ref: row.base_ref.clone(),
            model_used: None,
            used_fallback: false,
            task_id: None,
            task_url: None,
            comment_node_id: None,
            status: ExecutionStatus::Reserved,
            ambiguous: false,
            last_error: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// Whether the agent task is confirmed created.
    pub fn task_confirmed(&self) -> bool {
        self.task_id.is_some()
    }

    /// Whether the `ExecutionStarted` comment is durably recorded.
    pub fn is_complete(&self) -> bool {
        self.comment_node_id.is_some()
    }

    /// Whether this run reached a terminal state and should not be relaunched.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            ExecutionStatus::Completed | ExecutionStatus::Failed
        )
    }

    /// Reject a record that describes a different subject than the current row.
    ///
    /// The `runId` binds the Project Item and body digest. The remaining identity
    /// fields bind the durable intent to its exact subject and Project context;
    /// any mismatch is unsafe to resume.
    pub fn ensure_matches(&self, run_id: &str, row: &LaunchRow) -> anyhow::Result<()> {
        if self.schema_version != EXECUTION_RECORD_SCHEMA {
            anyhow::bail!(
                "execution record schema '{}' is not '{EXECUTION_RECORD_SCHEMA}'",
                self.schema_version
            );
        }
        if self.run_id != run_id
            || self.repository != row.repository
            || self.subject_number != row.subject_number
            || self.subject_node_id != row.subject_node_id
            || self.project_node_id != row.project_node_id
            || self.project_item_node_id != row.project_item_node_id
        {
            anyhow::bail!(
                "execution record for run '{}' describes {}#{} on item '{}', which does not match this row",
                self.run_id,
                self.repository,
                self.subject_number,
                self.project_item_node_id
            );
        }
        Ok(())
    }

    fn touch(&mut self) {
        self.updated_at = Utc::now().to_rfc3339();
    }

    /// Record the model of the attempt that is about to be sent to GitHub. Kept
    /// in `Reserved` so a crash mid-flight leaves the record reconcilable.
    pub fn set_attempt_model(&mut self, model: impl Into<String>, used_fallback: bool) {
        self.model_used = Some(model.into());
        self.used_fallback = used_fallback;
        self.status = ExecutionStatus::Reserved;
        self.ambiguous = false;
        self.last_error = None;
        self.touch();
    }

    /// Record the confirmed agent task.
    pub fn set_task(
        &mut self,
        model: impl Into<String>,
        used_fallback: bool,
        task_id: impl Into<String>,
        task_url: Option<String>,
    ) {
        self.model_used = Some(model.into());
        self.used_fallback = used_fallback;
        self.task_id = Some(task_id.into());
        self.task_url = task_url;
        self.status = ExecutionStatus::TaskCreated;
        self.ambiguous = false;
        self.last_error = None;
        self.touch();
    }

    /// Record the durable `ExecutionStarted` comment (terminal success).
    pub fn set_comment(&mut self, comment_node_id: impl Into<String>) {
        self.comment_node_id = Some(comment_node_id.into());
        self.status = ExecutionStatus::Completed;
        self.ambiguous = false;
        self.last_error = None;
        self.touch();
    }

    /// Record that a write's outcome is unknown and must be reconciled.
    pub fn set_ambiguous(&mut self, error: impl Into<String>) {
        self.status = ExecutionStatus::Ambiguous;
        self.ambiguous = true;
        self.last_error = Some(error.into());
        self.touch();
    }

    /// Record a permanent create-task rejection (terminal).
    pub fn set_failed(&mut self, error: impl Into<String>) {
        self.status = ExecutionStatus::Failed;
        self.ambiguous = false;
        self.last_error = Some(error.into());
        self.touch();
    }
}

/// A record together with the exact bytes it was loaded from.
#[derive(Debug, Clone)]
pub struct PersistedExecutionRecord {
    /// The decoded record.
    pub record: ExecutionRecord,
    /// The exact bytes in the store, used as the compare-and-swap witness.
    pub bytes: Vec<u8>,
}

/// The state-store key for a run (identical to the reservation key).
pub fn record_key(run_id: &str) -> String {
    crate::ids::reservation_key(run_id)
}

fn serialize(record: &ExecutionRecord) -> anyhow::Result<Vec<u8>> {
    serde_json::to_vec(record)
        .map_err(|error| anyhow::anyhow!("failed to serialize execution record: {error}"))
}

/// Load the record for a run, if any.
pub async fn load_record(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    run_id: &str,
) -> anyhow::Result<Option<PersistedExecutionRecord>> {
    let key = record_key(run_id);
    let Some(bytes) = store
        .get(store_id, &key)
        .await
        .map_err(|error| anyhow::anyhow!("state-store get execution record failed: {error}"))?
    else {
        return Ok(None);
    };
    let record = serde_json::from_slice::<ExecutionRecord>(&bytes)
        .map_err(|error| anyhow::anyhow!("failed to deserialize execution record: {error}"))?;
    Ok(Some(PersistedExecutionRecord { record, bytes }))
}

/// Create the record only if none exists.
///
/// Returns the already-persisted record when another writer won the race.
pub async fn create_record_if_absent(
    store: Arc<dyn StateStoreProvider>,
    store_id: &str,
    record: &ExecutionRecord,
) -> anyhow::Result<Option<PersistedExecutionRecord>> {
    let key = record_key(&record.run_id);
    let bytes = serialize(record)?;
    let outcome = store
        .compare_and_swap(store_id, &key, None, bytes)
        .await
        .map_err(|error| anyhow::anyhow!("state-store CAS execution-create failed: {error}"))?;
    match outcome {
        StateStoreCompareAndSwapResult::Swapped => Ok(None),
        StateStoreCompareAndSwapResult::Mismatch => {
            let existing = load_record(store, store_id, &record.run_id)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("execution record vanished after a CAS create mismatch")
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
    record: &ExecutionRecord,
) -> anyhow::Result<Option<Vec<u8>>> {
    let key = record_key(&record.run_id);
    let bytes = serialize(record)?;
    let outcome = store
        .compare_and_swap(store_id, &key, Some(expected), bytes.clone())
        .await
        .map_err(|error| anyhow::anyhow!("state-store CAS execution-update failed: {error}"))?;
    match outcome {
        StateStoreCompareAndSwapResult::Swapped => Ok(Some(bytes)),
        StateStoreCompareAndSwapResult::Mismatch => Ok(None),
    }
}

pub const WORKGRAPH_EXECUTION_STATE_SCHEMA_V1: &str = "workgraph.execution-state/v1";

/// Stable structured-log envelope emitted after an `Ambiguous` or `Failed`
/// execution record is durably written. It intentionally excludes
/// `last_error`, prompts, and credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkGraphExecutionStateV1 {
    pub schema: String,
    pub reaction_id: String,
    pub execution_id: String,
    pub run_id: String,
    pub status: ExecutionStatus,
    pub repository: String,
    pub issue_number: u64,
    pub error_present: bool,
    pub observed_at: String,
}

impl WorkGraphExecutionStateV1 {
    pub fn from_record(reaction_id: &str, record: &ExecutionRecord) -> Option<Self> {
        match record.status {
            ExecutionStatus::Ambiguous | ExecutionStatus::Failed => Some(Self {
                schema: WORKGRAPH_EXECUTION_STATE_SCHEMA_V1.to_string(),
                reaction_id: reaction_id.to_string(),
                execution_id: record.execution_id.clone(),
                run_id: record.run_id.clone(),
                status: record.status,
                repository: record.repository.clone(),
                issue_number: record.subject_number,
                error_present: record.last_error.is_some(),
                observed_at: record.updated_at.clone(),
            }),
            _ => None,
        }
    }
}

pub fn workgraph_execution_state_v1_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://drasi.io/schemas/workgraph/workgraph.execution-state-v1.json",
        "title": "workgraph.execution-state/v1",
        "description": "Structured log emitted after a failed or ambiguous Copilot Agent Task execution state is durably persisted.",
        "type": "object",
        "required": [
            "schema", "reactionId", "executionId", "runId",
            "status", "repository", "issueNumber", "errorPresent", "observedAt"
        ],
        "properties": {
            "schema": { "const": WORKGRAPH_EXECUTION_STATE_SCHEMA_V1 },
            "reactionId": { "type": "string" },
            "executionId": { "type": "string" },
            "runId": { "type": "string" },
            "status": { "enum": ["ambiguous", "failed"] },
            "repository": { "type": "string" },
            "issueNumber": { "type": "integer" },
            "errorPresent": { "type": "boolean" },
            "observedAt": { "type": "string", "format": "date-time" }
        },
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::state_store::MemoryStateStoreProvider;

    const RUN_ID: &str =
        "validation:PVTI_item:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const EVENT_ID: &str =
        "event:validation:PVTI_item:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:ExecutionStarted";
    const EXECUTION_ID: &str =
        "execution:validation:PVTI_item:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn row() -> LaunchRow {
        LaunchRow {
            repository: "drasi-project/drasi-core".to_string(),
            subject_number: 742,
            subject_node_id: "I_subject".to_string(),
            project_node_id: "PVT_project".to_string(),
            project_item_node_id: "PVTI_item".to_string(),
            project_status: drasi_workgraph_common::status::AWAITING_VALIDATION.to_string(),
            body_digest: format!("sha256:{}", "a".repeat(64)),
            event_comment_node_id: "IC_assignment".to_string(),
            event_body: "WorkGraphEvent/v1".to_string(),
            author_database_id: 4021243,
            author_type: "Bot".to_string(),
            is_edited: false,
            requested_model: "gpt-5".to_string(),
            fallback_model: Some("gpt-4".to_string()),
            base_ref: "main".to_string(),
        }
    }

    fn record() -> ExecutionRecord {
        ExecutionRecord::new(
            RUN_ID,
            EVENT_ID,
            EXECUTION_ID,
            &row(),
            "sha256:abc",
            "issue-validator@0123456789abcdef0123456789abcdef01234567",
        )
    }

    #[tokio::test]
    async fn create_is_idempotent_and_reports_the_existing_record() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        let first = create_record_if_absent(store.clone(), "copilot", &record())
            .await
            .expect("first create");
        assert!(first.is_none(), "first create wins");

        let second = create_record_if_absent(store.clone(), "copilot", &record())
            .await
            .expect("second create")
            .expect("existing record is returned");
        assert_eq!(second.record.run_id, RUN_ID);
        assert!(!second.record.is_complete());
    }

    #[tokio::test]
    async fn stale_witnesses_cannot_clobber_newer_progress() {
        let store: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
        create_record_if_absent(store.clone(), "copilot", &record())
            .await
            .expect("create");
        let loaded = load_record(store.clone(), "copilot", RUN_ID)
            .await
            .expect("load")
            .expect("exists");

        let mut advanced = loaded.record.clone();
        advanced.set_task("gpt-5", false, "task-1", Some("https://task/1".to_string()));
        let new_bytes = compare_and_swap_record(store.clone(), "copilot", &loaded.bytes, &advanced)
            .await
            .expect("cas")
            .expect("swap succeeds");

        // A writer holding the original bytes must lose.
        let mut stale = loaded.record.clone();
        stale.set_task("gpt-4", true, "task-2", None);
        assert!(
            compare_and_swap_record(store.clone(), "copilot", &loaded.bytes, &stale)
                .await
                .expect("cas")
                .is_none(),
            "stale witness must not overwrite newer progress"
        );

        let mut completed = advanced.clone();
        completed.set_comment("IC_1");
        assert!(
            compare_and_swap_record(store.clone(), "copilot", &new_bytes, &completed)
                .await
                .expect("cas")
                .is_some(),
            "fresh witness succeeds"
        );
        let final_record = load_record(store, "copilot", RUN_ID)
            .await
            .expect("load")
            .expect("exists")
            .record;
        assert_eq!(final_record.task_id.as_deref(), Some("task-1"));
        assert!(final_record.is_complete());
    }

    #[test]
    fn record_identity_is_checked_against_the_row_and_the_derived_run() {
        let record = record();
        record.ensure_matches(RUN_ID, &row()).expect("matching row");

        let mut other = row();
        other.project_item_node_id = "PVTI_other".to_string();
        assert!(record.ensure_matches(RUN_ID, &other).is_err());

        // A row whose binding derives a different run can never adopt this
        // record, even when every other field matches.
        let other_run =
            "validation:PVTI_other:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(record.ensure_matches(other_run, &row()).is_err());

        let mut wrong_schema = record.clone();
        wrong_schema.schema_version = "workgraph.execution-record/v0".to_string();
        assert!(wrong_schema.ensure_matches(RUN_ID, &row()).is_err());
    }

    #[test]
    fn transitions_drive_the_structured_log() {
        let mut record = record();
        assert!(WorkGraphExecutionStateV1::from_record("copilot", &record).is_none());

        record.set_ambiguous("transport failure");
        let log = WorkGraphExecutionStateV1::from_record("copilot", &record)
            .expect("ambiguous emits a log");
        assert_eq!(log.status, ExecutionStatus::Ambiguous);
        assert_eq!(log.run_id, RUN_ID);
        assert!(log.error_present);

        record.set_task("gpt-5", false, "task-1", None);
        assert!(WorkGraphExecutionStateV1::from_record("copilot", &record).is_none());

        record.set_comment("IC_1");
        assert!(record.is_complete());
        assert!(WorkGraphExecutionStateV1::from_record("copilot", &record).is_none());

        record.set_failed("permanent rejection");
        let log = WorkGraphExecutionStateV1::from_record("copilot", &record).expect("failed emits");
        assert_eq!(log.status, ExecutionStatus::Failed);
    }
}
