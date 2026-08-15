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

//! The routing-candidate row contract.
//!
//! A row **is** one authoritative `CompletedIssueValidation` comment as the
//! GitHub Source projected it, together with the Project Item it completes.
//! The router therefore never scans an issue thread looking for "a completion":
//! the row names the exact completion event, and
//! [`drasi_workgraph_common::row::accept_event_row`] proves it is trusted,
//! unedited, well-formed, and bound to this row's item, subject, and issue body.
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
//! | `eventCommentNodeId` | `GitHubIssueComment` node ID | the completion comment |
//! | `eventBody` | `GitHubIssueComment.body` | the strict `WorkGraphEvent/v1` body |
//! | `authorDatabaseId` | `GitHubIssueComment.authorDatabaseId` | **exact Source name**; half the trust key |
//! | `authorType` | `GitHubIssueComment.authorType` | **exact Source name**; half the trust key |
//! | `isEdited` | `GitHubIssueComment.isEdited` | **exact Source name**; must be `false` |
//!
//! The row still carries **no** outcome, event ID, responsibility, or
//! destination status: those come from the accepted event's payload, and the
//! routing table itself is fixed by
//! [`drasi_workgraph_common::event::RoutingDecidedPayload`]. Everything else the
//! router acts on — the live item status, the assignment/start chain, and the
//! current issue body — is re-read from GitHub before any write, so a stale row
//! can never cause an unintended transition.

use drasi_workgraph_common::event::WorkGraphEventType;
use drasi_workgraph_common::row::{accept_event_row, AcceptedEventRow, EventRow, RowError};
use serde::{Deserialize, Serialize};

use crate::config::{WorkgraphRouterReactionConfig, ROUTABLE_STATUS};

/// One authoritative `CompletedIssueValidation` comment and the Project Item it
/// completes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RoutingCandidate {
    /// `owner/repo` of the subject issue.
    pub repository: String,
    /// The subject issue number.
    pub subject_number: u64,
    /// The subject issue node ID (`I_...`).
    pub subject_node_id: String,
    /// The Project (v2) node ID (`PVT_...`).
    pub project_node_id: String,
    /// The Project (v2) item node ID (`PVTI_...`).
    pub project_item_node_id: String,
    /// The status the query observed for the item.
    pub project_status: String,
    /// The subject issue's authoritative body digest (`sha256:<64-hex>`), the
    /// Source's own `bodyDigest` projection.
    pub body_digest: String,
    /// The node ID of the comment carrying the completion event.
    pub event_comment_node_id: String,
    /// The exact comment body, in the strict `WorkGraphEvent/v1` grammar.
    pub event_body: String,
    /// The comment author's immutable numeric database ID (`authorDatabaseId`).
    pub author_database_id: u64,
    /// The comment author's actor type (`authorType`).
    pub author_type: String,
    /// Whether the comment was edited (`isEdited`); a `true` row is rejected.
    pub is_edited: bool,
}

impl RoutingCandidate {
    /// Reject rows that are structurally wrong or outside the allowlists.
    ///
    /// The event itself (author trust, edited flag, grammar, type, and run
    /// binding) is validated by [`Self::accept_completion`].
    pub fn validate(&self, config: &WorkgraphRouterReactionConfig) -> anyhow::Result<()> {
        if !config.allows_repository(&self.repository) {
            anyhow::bail!(
                "repository '{}' is not in allowedRepositories",
                self.repository
            );
        }
        if self.subject_number == 0 {
            anyhow::bail!("subjectNumber must be greater than 0");
        }
        if !self.subject_node_id.starts_with("I_") {
            anyhow::bail!(
                "subjectNodeId '{}' must be a GitHub issue node ID",
                self.subject_node_id
            );
        }
        if !config.allows_project(&self.project_node_id) {
            anyhow::bail!(
                "projectNodeId '{}' is not in allowedProjects",
                self.project_node_id
            );
        }
        if !self.project_item_node_id.starts_with("PVTI_") {
            anyhow::bail!(
                "projectItemNodeId '{}' must be a Project v2 item node ID",
                self.project_item_node_id
            );
        }
        if self.project_status != ROUTABLE_STATUS {
            anyhow::bail!(
                "projectStatus '{}' is not the routable status '{ROUTABLE_STATUS}'",
                self.project_status
            );
        }
        if self.event_comment_node_id.trim().is_empty() {
            anyhow::bail!("eventCommentNodeId must name the comment carrying the completion");
        }
        Ok(())
    }

    /// The row's authoritative Source projection of the completion comment.
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

    /// Accept the row's `CompletedIssueValidation` event.
    ///
    /// Proves the comment is unedited, authored by `trustedAuthorDatabaseId` +
    /// `trustedAuthorType`, parses under the strict grammar into a
    /// `CompletedIssueValidation` event, and binds this row's item, subject, and
    /// issue-body digest. See [`drasi_workgraph_common::row`].
    pub fn accept_completion(
        &self,
        config: &WorkgraphRouterReactionConfig,
    ) -> Result<AcceptedEventRow, RowError> {
        accept_event_row(
            &self.event_row(),
            &config.trusted_author(),
            WorkGraphEventType::CompletedIssueValidation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_workgraph_common::comment::render_comment;
    use drasi_workgraph_common::event::{
        CompletedIssueValidationPayload, ExecutionId, ExecutionStartedPayload, ValidationOutcome,
        ValidationReasonCode, WorkGraphEvent, WorkGraphEventPayload,
    };
    use drasi_workgraph_common::ids::{body_digest, run_id};
    use drasi_workgraph_common::summary::{summary_for, SubjectRef};
    use drasi_workgraph_common::trust::ActorType;

    const ITEM: &str = "PVTI_item";
    const SUBJECT: &str = "I_kwDOABCDEF6ABCDE";
    const ISSUE_BODY: &str = "Please validate. workgraph:validate";
    const TRUSTED_AUTHOR: u64 = 4021243;
    const EXECUTION: &str = "2f1c9e11-4a9d-4b66-a30d-1b8e7721fa4c";

    fn config() -> WorkgraphRouterReactionConfig {
        WorkgraphRouterReactionConfig {
            allowed_repositories: vec!["drasi-project/drasi-core".to_string()],
            allowed_projects: vec!["PVT_project".to_string()],
            expected_project_status_field_node_id: "PVTSSF_status".to_string(),
            trusted_author_database_id: TRUSTED_AUTHOR,
            trusted_author_type: ActorType::Bot,
            ..WorkgraphRouterReactionConfig::default()
        }
    }

    fn body_for(event: &WorkGraphEvent) -> String {
        let summary = summary_for(
            event,
            SubjectRef {
                repository: "drasi-project/drasi-core",
                number: 742,
            },
        );
        render_comment(event, &summary).expect("render")
    }

    fn completion_body() -> String {
        let event = WorkGraphEvent::new(
            run_id(ITEM, SUBJECT, &body_digest(Some(ISSUE_BODY))),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
                execution_id: ExecutionId::from_suffix(EXECUTION).expect("execution"),
                outcome: ValidationOutcome::Passed,
                reason_code: ValidationReasonCode::RequiredMarkerPresent,
            }),
        )
        .expect("completion event");
        body_for(&event)
    }

    fn candidate() -> RoutingCandidate {
        RoutingCandidate {
            repository: "drasi-project/drasi-core".to_string(),
            subject_number: 742,
            subject_node_id: SUBJECT.to_string(),
            project_node_id: "PVT_project".to_string(),
            project_item_node_id: ITEM.to_string(),
            project_status: ROUTABLE_STATUS.to_string(),
            body_digest: body_digest(Some(ISSUE_BODY)).as_str().to_string(),
            event_comment_node_id: "IC_completion".to_string(),
            event_body: completion_body(),
            author_database_id: TRUSTED_AUTHOR,
            author_type: "Bot".to_string(),
            is_edited: false,
        }
    }

    #[test]
    fn valid_candidate_passes() {
        let row = candidate();
        row.validate(&config()).expect("valid candidate");
        let accepted = row.accept_completion(&config()).expect("accepted");
        assert_eq!(
            accepted.run_id.as_str(),
            run_id(ITEM, SUBJECT, &body_digest(Some(ISSUE_BODY))).as_str()
        );
    }

    #[test]
    fn allowlists_are_enforced() {
        let mut row = candidate();
        row.repository = "attacker/repo".to_string();
        assert!(row
            .validate(&config())
            .expect_err("repo allowlist")
            .to_string()
            .contains("allowedRepositories"));

        let mut row = candidate();
        row.project_node_id = "PVT_other".to_string();
        assert!(row
            .validate(&config())
            .expect_err("project allowlist")
            .to_string()
            .contains("allowedProjects"));
    }

    #[test]
    fn node_id_shapes_are_enforced() {
        let mut row = candidate();
        row.subject_node_id = "PR_notanissue".to_string();
        assert!(row.validate(&config()).is_err());

        let mut row = candidate();
        row.project_item_node_id = "PVT_notanitem".to_string();
        assert!(row.validate(&config()).is_err());
    }

    #[test]
    fn only_awaiting_validation_is_routable() {
        for status in ["AwaitingRouting", "Triage", "AwaitingIssueRiskProfiling"] {
            let mut row = candidate();
            row.project_status = status.to_string();
            assert!(
                row.validate(&config())
                    .expect_err("wrong status")
                    .to_string()
                    .contains("routable status"),
                "status '{status}' must not be routable"
            );
        }
    }

    #[test]
    fn the_completion_comment_must_be_named() {
        let mut row = candidate();
        row.event_comment_node_id = "   ".to_string();
        assert!(row
            .validate(&config())
            .expect_err("missing comment node id")
            .to_string()
            .contains("eventCommentNodeId"));
    }

    #[test]
    fn an_edited_completion_row_is_never_accepted() {
        let mut row = candidate();
        row.is_edited = true;
        assert_eq!(
            row.accept_completion(&config()).expect_err("edited"),
            RowError::Edited
        );
    }

    #[test]
    fn an_untrusted_completion_author_is_never_accepted() {
        let mut wrong_id = candidate();
        wrong_id.author_database_id = TRUSTED_AUTHOR + 1;
        assert!(matches!(
            wrong_id.accept_completion(&config()),
            Err(RowError::Untrusted { .. })
        ));

        let mut wrong_type = candidate();
        wrong_type.author_type = "User".to_string();
        assert!(matches!(
            wrong_type.accept_completion(&config()),
            Err(RowError::Untrusted { .. })
        ));

        let mut unknown_type = candidate();
        unknown_type.author_type = "Mannequin".to_string();
        assert!(matches!(
            unknown_type.accept_completion(&config()),
            Err(RowError::ActorType(_))
        ));
    }

    #[test]
    fn a_non_completion_event_is_never_accepted() {
        let started = WorkGraphEvent::new(
            run_id(ITEM, SUBJECT, &body_digest(Some(ISSUE_BODY))),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
                execution_id: ExecutionId::from_suffix(EXECUTION).expect("execution"),
                task_id: "task-1".to_string(),
            }),
        )
        .expect("started event");
        let mut row = candidate();
        row.event_body = body_for(&started);
        assert!(matches!(
            row.accept_completion(&config()),
            Err(RowError::EventType { .. })
        ));
    }

    #[test]
    fn a_stale_body_digest_breaks_the_run_binding() {
        let mut row = candidate();
        row.body_digest = body_digest(Some("an edited issue body"))
            .as_str()
            .to_string();
        assert!(matches!(
            row.accept_completion(&config()),
            Err(RowError::RunId { .. })
        ));
    }

    #[test]
    fn a_row_that_renames_the_item_or_subject_is_never_accepted() {
        let mut other_item = candidate();
        other_item.project_item_node_id = "PVTI_other".to_string();
        assert!(matches!(
            other_item.accept_completion(&config()),
            Err(RowError::ProjectItem { .. })
        ));

        let mut other_subject = candidate();
        other_subject.subject_node_id = "I_other".to_string();
        assert!(matches!(
            other_subject.accept_completion(&config()),
            Err(RowError::Subject { .. })
        ));
    }

    #[test]
    fn a_legacy_body_is_never_accepted() {
        let mut row = candidate();
        let json = row
            .event_body
            .split_once("\n\n")
            .and_then(|(_, rest)| rest.split_once("\n\n"))
            .map(|(_, json)| json.to_string())
            .expect("event json");
        row.event_body = json;
        assert!(matches!(
            row.accept_completion(&config()),
            Err(RowError::Body(_))
        ));
    }

    #[test]
    fn unknown_and_removed_row_fields_are_rejected() {
        // The old row carried the outcome, the event ID, and author logins;
        // trusting any of them would let a query forge a routing decision.
        for removed in [
            "outcome",
            "eventId",
            "commentAuthor",
            "routeId",
            "responsibilityId",
            "policyId",
            "runId",
        ] {
            let mut value = serde_json::to_value(candidate()).expect("serialize");
            value[removed] = serde_json::json!("x");
            let error = serde_json::from_value::<RoutingCandidate>(value)
                .expect_err("removed row field must be rejected");
            assert!(
                error.to_string().contains(removed),
                "unexpected error for '{removed}': {error}"
            );
        }
    }

    #[test]
    fn the_row_round_trips_through_json() {
        let row = candidate();
        let value = serde_json::to_value(&row).expect("serialize");
        assert_eq!(
            serde_json::from_value::<RoutingCandidate>(value).expect("parse"),
            row
        );
    }
}
