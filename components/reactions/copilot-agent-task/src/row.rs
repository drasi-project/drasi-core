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

//! The launch-query row schema and its allowlist validation.
//!
//! The reaction subscribes to a single "launch query" and acts **only** on
//! `Add` result diffs (see [`crate::reaction`]). Each added row **is** one
//! authoritative `ResponsibilityAssigned` comment as the GitHub Source
//! projected it: the row carries the comment body, the comment's immutable
//! author metadata, its edited flag, and the subject issue's body digest. The
//! reaction therefore never goes looking for "some comment that looks like an
//! assignment" — the row names the exact event it acts on, and
//! [`drasi_workgraph_common::row::accept_event_row`] proves that event is
//! trusted, unedited, well-formed, and bound to this row's item, subject, and
//! issue body.
//!
//! # Row fields
//!
//! | Row field | Source origin | Role |
//! |---|---|---|
//! | `repository` | `GitHubIssue.repositoryNameWithOwner` | allowlisted `owner/repo` |
//! | `subjectNumber` | `GitHubIssue.number` | issue number |
//! | `subjectNodeId` | `GitHubIssue` node ID | run binding |
//! | `projectNodeId` | `GitHubProject` node ID | Project binding |
//! | `projectItemNodeId` | `GitHubProjectItem` node ID | run binding |
//! | `projectStatus` | `GitHubProjectItem.statusName` | must be `AwaitingValidation` |
//! | `bodyDigest` | `GitHubIssue.bodyDigest` | **exact Source name**; run binding |
//! | `eventCommentNodeId` | `GitHubIssueComment` node ID | the assignment comment |
//! | `eventBody` | `GitHubIssueComment.body` | the strict `WorkGraphEvent/v1` body |
//! | `authorDatabaseId` | `GitHubIssueComment.authorDatabaseId` | **exact Source name**; half the trust key |
//! | `authorType` | `GitHubIssueComment.authorType` | **exact Source name**; half the trust key |
//! | `isEdited` | `GitHubIssueComment.isEdited` | **exact Source name**; must be `false` |
//! | `requestedModel` / `fallbackModel` / `baseRef` | query policy | the task to create |
//!
//! There is no `runId` row field: the run is *derived* from
//! `projectItemNodeId`, `subjectNodeId`, and `bodyDigest`, and the assignment
//! event must name exactly that run. A row can therefore never nominate a run
//! its own binding does not produce.
//!
//! Anything that does not match the exact schema — an unknown field, a
//! malformed node ID, a disallowed repository/model, an edited comment, an
//! untrusted author, or an event that is not a `ResponsibilityAssigned` for
//! this exact run — makes the row a permanent rejection (fail-closed: the
//! reaction never launches on malformed or disallowed input).

use anyhow::{bail, Context, Result};
use drasi_workgraph_common::event::WorkGraphEventType;
use drasi_workgraph_common::row::{accept_event_row, AcceptedEventRow, EventRow, RowError};
use drasi_workgraph_common::status::AWAITING_VALIDATION;
use serde::{Deserialize, Serialize};

use crate::config::CopilotAgentTaskReactionConfig;

/// One row of the launch query's result set: one authoritative
/// `ResponsibilityAssigned` comment plus the model policy for its task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct LaunchRow {
    /// `"owner/repo"`.
    pub repository: String,
    /// The subject issue number (`> 0`).
    pub subject_number: u64,
    /// The subject issue node ID (`I_...`).
    pub subject_node_id: String,
    /// The Project (v2) node ID (`PVT_...`).
    pub project_node_id: String,
    /// The Project (v2) item node ID (`PVTI_...`).
    pub project_item_node_id: String,
    /// The item status the query observed; must be `AwaitingValidation`.
    pub project_status: String,
    /// The subject issue's authoritative body digest (`sha256:<64-hex>`), the
    /// Source's own `bodyDigest` projection.
    pub body_digest: String,
    /// The node ID of the comment carrying the assignment event.
    pub event_comment_node_id: String,
    /// The exact comment body, in the strict `WorkGraphEvent/v1` grammar.
    pub event_body: String,
    /// The comment author's immutable numeric database ID (`authorDatabaseId`).
    pub author_database_id: u64,
    /// The comment author's actor type (`authorType`).
    pub author_type: String,
    /// Whether the comment was edited (`isEdited`); a `true` row is rejected.
    pub is_edited: bool,
    /// The model requested for the agent task.
    pub requested_model: String,
    /// An optional fallback model, tried once if the requested model is
    /// rejected as unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    /// The git ref the agent task (and the pinned profile blob) is based on.
    pub base_ref: String,
}

impl LaunchRow {
    /// Parse a launch row from the raw `data` payload of an `Add` diff.
    pub fn from_json(data: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(data.clone()).context("launch row does not match expected schema")
    }

    /// Split `repository` into `(owner, repo)`.
    pub fn owner_and_repo(&self) -> Result<(&str, &str)> {
        let (owner, repo) = self
            .repository
            .split_once('/')
            .with_context(|| format!("repository '{}' is not 'owner/repo'", self.repository))?;
        if owner.is_empty() || repo.is_empty() || repo.contains('/') {
            bail!("repository '{}' is not 'owner/repo'", self.repository);
        }
        Ok((owner, repo))
    }

    /// The row's authoritative Source projection of the assignment comment.
    pub fn event_row(&self) -> EventRow<'_> {
        EventRow {
            project_item_node_id: &self.project_item_node_id,
            subject_node_id: &self.subject_node_id,
            body_digest: &self.body_digest,
            author_database_id: self.author_database_id,
            author_type: &self.author_type,
            is_edited: self.is_edited,
            body: &self.event_body,
        }
    }

    /// Accept the row's `ResponsibilityAssigned` event.
    ///
    /// Proves the comment is unedited, authored by
    /// `trustedAssignmentAuthorDatabaseId` + `trustedAssignmentAuthorType`,
    /// parses under the strict grammar into a `ResponsibilityAssigned` event,
    /// and binds this row's item, subject, and issue-body digest. See
    /// [`drasi_workgraph_common::row`].
    pub fn accept_assignment(
        &self,
        config: &CopilotAgentTaskReactionConfig,
    ) -> std::result::Result<AcceptedEventRow, RowError> {
        accept_event_row(
            &self.event_row(),
            &config.trusted_assignment_author(),
            WorkGraphEventType::ResponsibilityAssigned,
        )
    }

    /// Validate a parsed row against the reaction's configured allowlists and
    /// the frozen identifier grammar. Fails closed: an empty allowlist allows
    /// nothing. Every failure is **permanent** — retrying the identical row can
    /// never change the outcome, so the reaction logs and skips it.
    ///
    /// The event itself (author trust, edited flag, grammar, type, and run
    /// binding) is validated by [`Self::accept_assignment`].
    pub fn validate(&self, config: &CopilotAgentTaskReactionConfig) -> Result<()> {
        if !config
            .allowed_repositories
            .iter()
            .any(|allowed| allowed == &self.repository)
        {
            bail!(
                "repository '{}' is not in allowedRepositories",
                self.repository
            );
        }
        // Ensure the allowlisted value is also a usable 'owner/repo'.
        self.owner_and_repo()?;

        if self.subject_number == 0 {
            bail!("subjectNumber must be greater than 0");
        }
        if !self.subject_node_id.starts_with("I_") {
            bail!(
                "subjectNodeId '{}' must be a GitHub issue node ID starting with 'I_'",
                self.subject_node_id
            );
        }
        if !self.project_node_id.starts_with("PVT_") {
            bail!(
                "projectNodeId '{}' must be a GitHub Projects v2 node ID starting with 'PVT_'",
                self.project_node_id
            );
        }
        if !self.project_item_node_id.starts_with("PVTI_") {
            bail!(
                "projectItemNodeId '{}' must be a GitHub Projects v2 item node ID starting with 'PVTI_'",
                self.project_item_node_id
            );
        }
        if self.project_status != AWAITING_VALIDATION {
            bail!(
                "projectStatus '{}' is not the launchable status '{AWAITING_VALIDATION}'",
                self.project_status
            );
        }
        if self.event_comment_node_id.trim().is_empty() {
            bail!("eventCommentNodeId must name the comment carrying the assignment");
        }

        if !config
            .allowed_models
            .iter()
            .any(|allowed| allowed == &self.requested_model)
        {
            bail!(
                "requestedModel '{}' is not in allowedModels",
                self.requested_model
            );
        }
        if let Some(fallback) = &self.fallback_model {
            if fallback.is_empty() {
                bail!("fallbackModel must not be empty when present");
            }
            if !config
                .allowed_models
                .iter()
                .any(|allowed| allowed == fallback)
            {
                bail!("fallbackModel '{fallback}' is not in allowedModels");
            }
            if fallback == &self.requested_model {
                bail!("fallbackModel must differ from requestedModel");
            }
        }
        if self.base_ref.trim().is_empty() {
            bail!("baseRef must not be empty");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_workgraph_common::comment::render_comment;
    use drasi_workgraph_common::event::{
        AssignedResponsibilityType, ExecutionId, ExecutionStartedPayload, ProfileRef,
        ResponsibilityAssignedPayload, WorkGraphEvent, WorkGraphEventPayload,
    };
    use drasi_workgraph_common::ids::{body_digest, run_id};
    use drasi_workgraph_common::summary::{summary_for, SubjectRef};
    use drasi_workgraph_common::trust::ActorType;
    use serde_json::json;

    const ITEM: &str = "PVTI_test";
    const SUBJECT: &str = "I_kwDOtest";
    const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";
    const ISSUE_BODY: &str = "Please validate this issue.\n";
    const ASSIGNMENT_AUTHOR: u64 = 4021243;

    fn config() -> CopilotAgentTaskReactionConfig {
        CopilotAgentTaskReactionConfig {
            allowed_repositories: vec!["drasi-project/drasi-core".to_string()],
            allowed_profiles: vec!["issue-validator".to_string()],
            allowed_models: vec!["gpt-5".to_string(), "gpt-4".to_string()],
            trusted_assignment_author_database_id: ASSIGNMENT_AUTHOR,
            trusted_assignment_author_type: ActorType::Bot,
            ..Default::default()
        }
    }

    fn assignment_body() -> String {
        let digest = body_digest(Some(ISSUE_BODY));
        let event = WorkGraphEvent::new(
            run_id(ITEM, SUBJECT, &digest),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
                responsibility_type: AssignedResponsibilityType::IssueValidation,
                profile_ref: ProfileRef::new("issue-validator", BLOB).expect("profile"),
                content_digest: digest,
            }),
        )
        .expect("assignment event");
        let summary = summary_for(
            &event,
            SubjectRef {
                repository: "drasi-project/drasi-core",
                number: 42,
            },
        );
        render_comment(&event, &summary).expect("render")
    }

    fn sample_row() -> LaunchRow {
        LaunchRow {
            repository: "drasi-project/drasi-core".to_string(),
            subject_number: 42,
            subject_node_id: SUBJECT.to_string(),
            project_node_id: "PVT_test".to_string(),
            project_item_node_id: ITEM.to_string(),
            project_status: AWAITING_VALIDATION.to_string(),
            body_digest: body_digest(Some(ISSUE_BODY)).as_str().to_string(),
            event_comment_node_id: "IC_assignment".to_string(),
            event_body: assignment_body(),
            author_database_id: ASSIGNMENT_AUTHOR,
            author_type: "Bot".to_string(),
            is_edited: false,
            requested_model: "gpt-5".to_string(),
            fallback_model: Some("gpt-4".to_string()),
            base_ref: "main".to_string(),
        }
    }

    #[test]
    fn parses_from_json_and_round_trips() {
        let row = sample_row();
        let value = serde_json::to_value(&row).expect("serialize");
        assert_eq!(LaunchRow::from_json(&value).expect("parse"), row);
    }

    #[test]
    fn rejects_unknown_fields() {
        let mut value = serde_json::to_value(sample_row()).expect("serialize");
        value["surpriseField"] = json!("nope");
        assert!(LaunchRow::from_json(&value).is_err());
    }

    #[test]
    fn rejects_the_removed_run_id_row_field() {
        // `runId` is derived from the row's own binding; accepting one from the
        // query would let a row nominate a run its binding does not produce.
        let mut value = serde_json::to_value(sample_row()).expect("serialize");
        value["runId"] = json!("run:0000");
        let error = LaunchRow::from_json(&value).expect_err("runId must be rejected");
        assert!(error.to_string().contains("schema"), "{error}");
    }

    #[test]
    fn rejects_missing_fields() {
        let value = json!({ "repository": "only-one-field" });
        assert!(LaunchRow::from_json(&value).is_err());
    }

    #[test]
    fn owner_and_repo_splits() {
        let row = sample_row();
        let (owner, repo) = row.owner_and_repo().expect("split");
        assert_eq!(owner, "drasi-project");
        assert_eq!(repo, "drasi-core");
    }

    #[test]
    fn valid_row_passes() {
        let row = sample_row();
        row.validate(&config()).expect("valid row");
        let accepted = row.accept_assignment(&config()).expect("accepted");
        assert_eq!(
            accepted.run_id.as_str(),
            run_id(ITEM, SUBJECT, &body_digest(Some(ISSUE_BODY))).as_str()
        );
    }

    #[test]
    fn rejects_disallowed_repository() {
        let mut row = sample_row();
        row.repository = "evil/repo".to_string();
        assert!(row
            .validate(&config())
            .expect_err("disallowed repo")
            .to_string()
            .contains("allowedRepositories"));
    }

    #[test]
    fn fails_closed_on_empty_allowlists() {
        let error = sample_row()
            .validate(&CopilotAgentTaskReactionConfig::default())
            .expect_err("empty allowlists allow nothing");
        assert!(error.to_string().contains("allowedRepositories"));
    }

    #[test]
    fn rejects_disallowed_requested_model() {
        let mut row = sample_row();
        row.requested_model = "gpt-does-not-exist".to_string();
        assert!(row
            .validate(&config())
            .expect_err("disallowed model")
            .to_string()
            .contains("requestedModel"));
    }

    #[test]
    fn rejects_disallowed_fallback_model() {
        let mut row = sample_row();
        row.fallback_model = Some("gpt-nope".to_string());
        assert!(row
            .validate(&config())
            .expect_err("disallowed fallback")
            .to_string()
            .contains("fallbackModel"));
    }

    #[test]
    fn rejects_fallback_equal_to_requested() {
        let mut row = sample_row();
        row.fallback_model = Some(row.requested_model.clone());
        assert!(row
            .validate(&config())
            .expect_err("fallback equals requested")
            .to_string()
            .contains("differ"));
    }

    #[test]
    fn allows_missing_fallback_model() {
        let mut row = sample_row();
        row.fallback_model = None;
        row.validate(&config()).expect("missing fallback is fine");
    }

    #[test]
    fn rejects_bad_node_id_prefixes() {
        for mutate in [
            (|r: &mut LaunchRow| r.subject_node_id = "X_bad".to_string()) as fn(&mut LaunchRow),
            |r: &mut LaunchRow| r.project_node_id = "PVTI_wrong".to_string(),
            |r: &mut LaunchRow| r.project_item_node_id = "PVT_wrong".to_string(),
        ] {
            let mut row = sample_row();
            mutate(&mut row);
            assert!(row.validate(&config()).is_err());
        }
    }

    #[test]
    fn rejects_zero_subject_number() {
        let mut row = sample_row();
        row.subject_number = 0;
        assert!(row.validate(&config()).is_err());
    }

    #[test]
    fn only_awaiting_validation_is_launchable() {
        for status in ["Triage", "AwaitingRouting", "AwaitingIssueRiskProfiling"] {
            let mut row = sample_row();
            row.project_status = status.to_string();
            assert!(
                row.validate(&config())
                    .expect_err("wrong status")
                    .to_string()
                    .contains("launchable status"),
                "status '{status}' must not be launchable"
            );
        }
    }

    #[test]
    fn requires_the_assignment_comment_node_id() {
        let mut row = sample_row();
        row.event_comment_node_id = "  ".to_string();
        assert!(row
            .validate(&config())
            .expect_err("missing comment node id")
            .to_string()
            .contains("eventCommentNodeId"));
    }

    #[test]
    fn an_edited_assignment_row_is_never_accepted() {
        let mut row = sample_row();
        row.is_edited = true;
        assert_eq!(
            row.accept_assignment(&config()).expect_err("edited"),
            RowError::Edited
        );
    }

    #[test]
    fn an_untrusted_assignment_author_is_never_accepted() {
        let mut wrong_id = sample_row();
        wrong_id.author_database_id = ASSIGNMENT_AUTHOR + 1;
        assert!(matches!(
            wrong_id.accept_assignment(&config()),
            Err(RowError::Untrusted { .. })
        ));

        let mut wrong_type = sample_row();
        wrong_type.author_type = "User".to_string();
        assert!(matches!(
            wrong_type.accept_assignment(&config()),
            Err(RowError::Untrusted { .. })
        ));
    }

    #[test]
    fn a_non_assignment_event_is_never_accepted() {
        let digest = body_digest(Some(ISSUE_BODY));
        let started = WorkGraphEvent::new(
            run_id(ITEM, SUBJECT, &digest),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
                execution_id: ExecutionId::from_suffix("2f1c9e11-4a9d-4b66-a30d-1b8e7721fa4c")
                    .expect("execution id"),
                task_id: "task-1".to_string(),
            }),
        )
        .expect("started event");
        let summary = summary_for(
            &started,
            SubjectRef {
                repository: "drasi-project/drasi-core",
                number: 42,
            },
        );
        let mut row = sample_row();
        row.event_body = render_comment(&started, &summary).expect("render");
        assert!(matches!(
            row.accept_assignment(&config()),
            Err(RowError::EventType { .. })
        ));
    }

    #[test]
    fn a_stale_body_digest_breaks_the_run_binding() {
        let mut row = sample_row();
        row.body_digest = body_digest(Some("an edited issue body"))
            .as_str()
            .to_string();
        assert!(matches!(
            row.accept_assignment(&config()),
            Err(RowError::RunId { .. })
        ));
    }

    #[test]
    fn a_row_that_renames_the_item_or_subject_is_never_accepted() {
        let mut other_item = sample_row();
        other_item.project_item_node_id = "PVTI_other".to_string();
        assert!(matches!(
            other_item.accept_assignment(&config()),
            Err(RowError::ProjectItem { .. })
        ));

        let mut other_subject = sample_row();
        other_subject.subject_node_id = "I_other".to_string();
        assert!(matches!(
            other_subject.accept_assignment(&config()),
            Err(RowError::Subject { .. })
        ));
    }

    #[test]
    fn a_legacy_body_is_never_accepted() {
        let mut row = sample_row();
        let json = row
            .event_body
            .split_once("\n\n")
            .and_then(|(_, rest)| rest.split_once("\n\n"))
            .map(|(_, json)| json.to_string())
            .expect("event json");
        row.event_body = json;
        assert!(matches!(
            row.accept_assignment(&config()),
            Err(RowError::Body(_))
        ));
    }
}
