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

//! Durable execution/reservation state.
//!
//! Every launch attempt is tracked by an [`ExecutionRecord`] persisted in the
//! reaction's `StateStoreProvider` partition, keyed by
//! `execution:{routeId}:{responsibilityId}:{attempt}` (see
//! [`crate::ids::reservation_key`]). The record is written **before** any
//! external side effect (task creation, comment posting) and updated as the
//! attempt progresses, which is what makes duplicate delivery, crashes, and
//! restarts safe:
//!
//! ```text
//! Reserved -> Starting -> Started (comment_posted=false) -> Started (comment_posted=true)
//!                 \-> Ambiguous (creation outcome unknown; needs reconciliation)
//!                 \-> Failed (permanent: validation/preflight rejected the row)
//! ```
//!
//! The query-level checkpoint (advanced by `ReactionBase::run_standard_loop`)
//! is a completely separate, coarser-grained position marker; **this** record
//! is what provides row-level idempotency across restarts and redelivery.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use drasi_lib::state_store::StateStoreProvider;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Lifecycle status of one launch attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// The attempt is durably reserved; no external call has been made yet.
    Reserved,
    /// Preflight passed and the create-task request is in flight (or the
    /// process crashed before the response was durably recorded).
    Starting,
    /// The task was confirmed created (HTTP 201, ID captured). `comment_posted`
    /// tracks whether the workgraph execution comment still needs to be sent.
    Started,
    /// The create-task call's outcome is unknown (e.g. a transport-level
    /// error/timeout with no HTTP response) and reconciliation has not yet
    /// found — or has found more than one — a matching task. **Never**
    /// blindly retried; see `crate::github::reconcile`.
    Ambiguous,
    /// A permanent condition (failed validation or preflight) means this
    /// attempt will never launch. Terminal — not retried.
    Failed,
}

/// Persisted state for one `(routeId, responsibilityId, attempt)` launch
/// attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRecord {
    pub route_id: String,
    pub responsibility_id: String,
    pub attempt: u32,

    pub execution_id: String,
    pub expected_event_id: String,
    pub required_event_type: String,

    pub repository: String,
    pub issue_number: u64,
    pub issue_node_id: String,
    pub agent_profile: String,
    pub profile_ref: String,

    pub status: ExecutionStatus,

    pub requested_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_used: Option<String>,
    #[serde(default)]
    pub used_fallback: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_time: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,

    #[serde(default)]
    pub comment_posted: bool,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub const WORKGRAPH_EXECUTION_STATE_SCHEMA_V1: &str = "workgraph.execution-state/v1";

/// Stable structured-log envelope emitted after an `Ambiguous` or `Failed`
/// execution record is durably written. It intentionally excludes
/// `last_error`, prompts, and credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkGraphExecutionStateV1 {
    pub schema: String,
    pub reaction_id: String,
    pub execution_id: String,
    pub route_id: String,
    pub responsibility_id: String,
    pub attempt: u32,
    pub status: ExecutionStatus,
    pub repository: String,
    pub issue_number: u64,
    pub error_present: bool,
    pub observed_at: DateTime<Utc>,
}

impl WorkGraphExecutionStateV1 {
    pub fn from_record(reaction_id: &str, record: &ExecutionRecord) -> Option<Self> {
        match record.status {
            ExecutionStatus::Ambiguous | ExecutionStatus::Failed => Some(Self {
                schema: WORKGRAPH_EXECUTION_STATE_SCHEMA_V1.to_string(),
                reaction_id: reaction_id.to_string(),
                execution_id: record.execution_id.clone(),
                route_id: record.route_id.clone(),
                responsibility_id: record.responsibility_id.clone(),
                attempt: record.attempt,
                status: record.status,
                repository: record.repository.clone(),
                issue_number: record.issue_number,
                error_present: record.last_error.is_some(),
                observed_at: record.updated_at,
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
            "schema", "reactionId", "executionId", "routeId", "responsibilityId", "attempt",
            "status", "repository", "issueNumber", "errorPresent", "observedAt"
        ],
        "properties": {
            "schema": { "const": WORKGRAPH_EXECUTION_STATE_SCHEMA_V1 },
            "reactionId": { "type": "string" },
            "executionId": { "type": "string" },
            "routeId": { "type": "string" },
            "responsibilityId": { "type": "string" },
            "attempt": { "type": "integer", "minimum": 1 },
            "status": { "enum": ["ambiguous", "failed"] },
            "repository": { "type": "string" },
            "issueNumber": { "type": "integer" },
            "errorPresent": { "type": "boolean" },
            "observedAt": { "type": "string", "format": "date-time" }
        },
        "additionalProperties": false
    })
}

impl ExecutionRecord {
    /// A brand-new record in the `Reserved` state.
    #[allow(clippy::too_many_arguments)]
    pub fn new_reserved(
        route_id: &str,
        responsibility_id: &str,
        attempt: u32,
        execution_id: &str,
        expected_event_id: &str,
        required_event_type: &str,
        repository: &str,
        issue_number: u64,
        issue_node_id: &str,
        agent_profile: &str,
        profile_ref: &str,
        requested_model: &str,
        fallback_model: Option<&str>,
    ) -> Self {
        let now = Utc::now();
        Self {
            route_id: route_id.to_string(),
            responsibility_id: responsibility_id.to_string(),
            attempt,
            execution_id: execution_id.to_string(),
            expected_event_id: expected_event_id.to_string(),
            required_event_type: required_event_type.to_string(),
            repository: repository.to_string(),
            issue_number,
            issue_node_id: issue_node_id.to_string(),
            agent_profile: agent_profile.to_string(),
            profile_ref: profile_ref.to_string(),
            status: ExecutionStatus::Reserved,
            requested_model: requested_model.to_string(),
            fallback_model: fallback_model.map(|s| s.to_string()),
            model_used: None,
            used_fallback: false,
            task_id: None,
            task_url: None,
            request_time: None,
            last_error: None,
            comment_posted: false,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }
}

/// Load an [`ExecutionRecord`] from the state store, if one exists.
pub async fn load(
    store: &dyn StateStoreProvider,
    store_id: &str,
    route_id: &str,
    responsibility_id: &str,
    attempt: u32,
) -> Result<Option<ExecutionRecord>> {
    let key = crate::ids::reservation_key(route_id, responsibility_id, attempt);
    match store
        .get(store_id, &key)
        .await
        .map_err(|e| anyhow::anyhow!("state store get failed: {e}"))?
    {
        Some(bytes) => {
            let record: ExecutionRecord =
                serde_json::from_slice(&bytes).context("failed to deserialize ExecutionRecord")?;
            Ok(Some(record))
        }
        None => Ok(None),
    }
}

/// Durably persist an [`ExecutionRecord`]. Called before every external
/// side effect (and again after) so the record on disk always reflects the
/// most recent known state.
pub async fn save(
    store: &dyn StateStoreProvider,
    store_id: &str,
    record: &ExecutionRecord,
) -> Result<()> {
    let key =
        crate::ids::reservation_key(&record.route_id, &record.responsibility_id, record.attempt);
    let bytes = serde_json::to_vec(record).context("failed to serialize ExecutionRecord")?;
    let observation = WorkGraphExecutionStateV1::from_record(store_id, record)
        .map(|value| {
            serde_json::to_string(&value)
                .context("failed to serialize workgraph execution-state observation")
        })
        .transpose()?;
    store
        .set(store_id, &key, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("state store set failed: {e}"))?;
    if let Some(observation) = observation {
        log::warn!(target: "workgraph.execution_state", "{observation}");
    }
    Ok(())
}

/// What the caller should do next for a given `(routeId, responsibilityId,
/// attempt)`, having consulted the durable record (if any).
pub enum ReservationOutcome {
    /// No prior record: freshly reserved and persisted. Proceed with
    /// preflight + launch.
    New(ExecutionRecord),
    /// Already fully completed (task created *and* comment posted). Skip —
    /// this is the duplicate-delivery / already-processed case.
    AlreadyDone(ExecutionRecord),
    /// Task creation was confirmed but the comment was not (yet) recorded as
    /// posted. Resume at the comment-posting step; do **not** recreate the
    /// task.
    ResumeCommentOnly(ExecutionRecord),
    /// The process crashed between reserving and confirming task creation, or
    /// the create-task call's outcome was unknown. Reconciliation must run
    /// before any further action.
    NeedsReconciliation(ExecutionRecord),
    /// This attempt permanently failed (validation/preflight). Skip.
    PermanentlyFailed(ExecutionRecord),
}

/// Reserve (or resume) a launch attempt. This is the durable reservation the
/// requirements call for: the record is written to the state store before
/// any GitHub API call is attempted, and is idempotent across restarts and
/// redelivery because it is looked up (not blindly overwritten) first.
///
/// A `Mutex` guarding this function's caller serializes concurrent callers
/// within one process (the reaction's processing loop is itself
/// single-threaded per query, but the guard makes the function safe to reuse
/// from tests or a future multi-worker loop without a race between the load
/// and the save).
#[allow(clippy::too_many_arguments)]
pub async fn reserve_or_resume(
    store: &dyn StateStoreProvider,
    store_id: &str,
    route_id: &str,
    responsibility_id: &str,
    attempt: u32,
    execution_id: &str,
    expected_event_id: &str,
    required_event_type: &str,
    repository: &str,
    issue_number: u64,
    issue_node_id: &str,
    agent_profile: &str,
    profile_ref: &str,
    requested_model: &str,
    fallback_model: Option<&str>,
) -> Result<ReservationOutcome> {
    if let Some(existing) = load(store, store_id, route_id, responsibility_id, attempt).await? {
        let outcome = match existing.status {
            ExecutionStatus::Started if existing.comment_posted => {
                ReservationOutcome::AlreadyDone(existing)
            }
            ExecutionStatus::Started => ReservationOutcome::ResumeCommentOnly(existing),
            ExecutionStatus::Starting | ExecutionStatus::Ambiguous => {
                ReservationOutcome::NeedsReconciliation(existing)
            }
            ExecutionStatus::Failed => ReservationOutcome::PermanentlyFailed(existing),
            // A bare `Reserved` record with no follow-up (crash immediately
            // after the reservation write, before preflight even started) is
            // safe to resume as a fresh launch — no external call was made.
            ExecutionStatus::Reserved => ReservationOutcome::New(existing),
        };
        return Ok(outcome);
    }

    let record = ExecutionRecord::new_reserved(
        route_id,
        responsibility_id,
        attempt,
        execution_id,
        expected_event_id,
        required_event_type,
        repository,
        issue_number,
        issue_node_id,
        agent_profile,
        profile_ref,
        requested_model,
        fallback_model,
    );
    save(store, store_id, &record).await?;
    Ok(ReservationOutcome::New(record))
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::state_store::MemoryStateStoreProvider;

    fn sample() -> ExecutionRecord {
        ExecutionRecord::new_reserved(
            "route-1",
            "resp-1",
            1,
            "exec-1",
            "evt-1",
            "CompletedIssueValidation",
            "owner/repo",
            42,
            "I_issue",
            "issue-validator",
            "issue-validator@0123456789abcdef0123456789abcdef01234567",
            "gpt-5",
            Some("gpt-4"),
        )
    }

    #[tokio::test]
    async fn round_trips_through_state_store() {
        let store = MemoryStateStoreProvider::new();
        let record = sample();
        save(&store, "reaction-1", &record).await.unwrap();
        let loaded = load(&store, "reaction-1", "route-1", "resp-1", 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded, record);
    }

    #[tokio::test]
    async fn reserve_or_resume_creates_new_record_when_absent() {
        let store = MemoryStateStoreProvider::new();
        let outcome = reserve_or_resume(
            &store,
            "r1",
            "route-1",
            "resp-1",
            1,
            "exec-1",
            "evt-1",
            "CompletedIssueValidation",
            "owner/repo",
            1,
            "I_issue",
            "issue-validator",
            "issue-validator@0123456789abcdef0123456789abcdef01234567",
            "gpt-5",
            None,
        )
        .await
        .unwrap();
        match outcome {
            ReservationOutcome::New(r) => assert_eq!(r.status, ExecutionStatus::Reserved),
            _ => panic!("expected New"),
        }
        // Persisted for real.
        assert!(load(&store, "r1", "route-1", "resp-1", 1)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn reserve_or_resume_reports_already_done() {
        let store = MemoryStateStoreProvider::new();
        let mut record = sample();
        record.status = ExecutionStatus::Started;
        record.comment_posted = true;
        save(&store, "r1", &record).await.unwrap();

        let outcome = reserve_or_resume(
            &store,
            "r1",
            "route-1",
            "resp-1",
            1,
            "exec-1",
            "evt-1",
            "CompletedIssueValidation",
            "owner/repo",
            42,
            "I_issue",
            "issue-validator",
            "issue-validator@0123456789abcdef0123456789abcdef01234567",
            "gpt-5",
            Some("gpt-4"),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, ReservationOutcome::AlreadyDone(_)));
    }

    #[test]
    fn failed_observation_is_structured_and_excludes_error_text() {
        let mut record = sample();
        record.status = ExecutionStatus::Failed;
        record.last_error = Some("sensitive upstream response".to_string());
        let observation = WorkGraphExecutionStateV1::from_record("reaction-1", &record).unwrap();
        let value = serde_json::to_value(observation).unwrap();
        assert_eq!(value["schema"], WORKGRAPH_EXECUTION_STATE_SCHEMA_V1);
        assert_eq!(value["reactionId"], "reaction-1");
        assert_eq!(value["status"], "failed");
        assert_eq!(value["errorPresent"], true);
        assert!(value.get("lastError").is_none());
        assert!(!value.to_string().contains("sensitive upstream response"));
    }

    #[test]
    fn successful_states_do_not_emit_failure_observations() {
        let mut record = sample();
        record.status = ExecutionStatus::Started;
        assert!(WorkGraphExecutionStateV1::from_record("reaction-1", &record).is_none());
    }

    #[tokio::test]
    async fn reserve_or_resume_reports_resume_comment_only() {
        let store = MemoryStateStoreProvider::new();
        let mut record = sample();
        record.status = ExecutionStatus::Started;
        record.comment_posted = false;
        record.task_id = Some("task-1".to_string());
        save(&store, "r1", &record).await.unwrap();

        let outcome = reserve_or_resume(
            &store,
            "r1",
            "route-1",
            "resp-1",
            1,
            "exec-1",
            "evt-1",
            "CompletedIssueValidation",
            "owner/repo",
            42,
            "I_issue",
            "issue-validator",
            "issue-validator@0123456789abcdef0123456789abcdef01234567",
            "gpt-5",
            Some("gpt-4"),
        )
        .await
        .unwrap();
        match outcome {
            ReservationOutcome::ResumeCommentOnly(r) => {
                assert_eq!(r.task_id.as_deref(), Some("task-1"))
            }
            _ => panic!("expected ResumeCommentOnly"),
        }
    }

    #[tokio::test]
    async fn reserve_or_resume_reports_needs_reconciliation_for_starting_and_ambiguous() {
        let store = MemoryStateStoreProvider::new();
        for status in [ExecutionStatus::Starting, ExecutionStatus::Ambiguous] {
            let mut record = sample();
            record.status = status;
            save(&store, "r1", &record).await.unwrap();

            let outcome = reserve_or_resume(
                &store,
                "r1",
                "route-1",
                "resp-1",
                1,
                "exec-1",
                "evt-1",
                "CompletedIssueValidation",
                "owner/repo",
                42,
                "I_issue",
                "issue-validator",
                "issue-validator@0123456789abcdef0123456789abcdef01234567",
                "gpt-5",
                Some("gpt-4"),
            )
            .await
            .unwrap();
            assert!(
                matches!(outcome, ReservationOutcome::NeedsReconciliation(_)),
                "status {status:?} should need reconciliation"
            );
        }
    }

    #[tokio::test]
    async fn reserve_or_resume_reports_permanently_failed() {
        let store = MemoryStateStoreProvider::new();
        let mut record = sample();
        record.status = ExecutionStatus::Failed;
        save(&store, "r1", &record).await.unwrap();

        let outcome = reserve_or_resume(
            &store,
            "r1",
            "route-1",
            "resp-1",
            1,
            "exec-1",
            "evt-1",
            "CompletedIssueValidation",
            "owner/repo",
            42,
            "I_issue",
            "issue-validator",
            "issue-validator@0123456789abcdef0123456789abcdef01234567",
            "gpt-5",
            Some("gpt-4"),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, ReservationOutcome::PermanentlyFailed(_)));
    }

    #[tokio::test]
    async fn reserve_or_resume_resumes_bare_reserved_as_new() {
        let store = MemoryStateStoreProvider::new();
        let record = sample(); // status = Reserved
        save(&store, "r1", &record).await.unwrap();

        let outcome = reserve_or_resume(
            &store,
            "r1",
            "route-1",
            "resp-1",
            1,
            "exec-1",
            "evt-1",
            "CompletedIssueValidation",
            "owner/repo",
            42,
            "I_issue",
            "issue-validator",
            "issue-validator@0123456789abcdef0123456789abcdef01234567",
            "gpt-5",
            Some("gpt-4"),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, ReservationOutcome::New(_)));
    }
}
