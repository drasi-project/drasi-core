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

//! The authoritative Source row that carries one WorkGraph event.
//!
//! A WorkGraph reaction is triggered by a query row, not by polling an issue
//! thread. That row is projected by the authoritative GitHub Source, so it is
//! the row — never a re-read of "some comment that looks right" — that names
//! the event a reaction acts on.
//!
//! # The exact Source field names
//!
//! Four row properties are the Source's own spellings and may never be renamed
//! at a call site:
//!
//! | Row field | Source node | Meaning |
//! |---|---|---|
//! | [`BODY_DIGEST_FIELD`] (`bodyDigest`) | `GitHubIssue` | `"sha256:" + lowerHex(sha256(utf8(body ?? "")))` of the **subject issue body** |
//! | [`AUTHOR_DATABASE_ID_FIELD`] (`authorDatabaseId`) | `GitHubIssueComment` | the comment author's immutable numeric database ID |
//! | [`AUTHOR_TYPE_FIELD`] (`authorType`) | `GitHubIssueComment` | `User` / `Bot` / `Organization` |
//! | [`IS_EDITED_FIELD`] (`isEdited`) | `GitHubIssueComment` | whether the comment body was edited after it was written |
//!
//! `bodyDigest` is the **issue** body digest because that is the only
//! `bodyDigest` the Source contract defines: it is projected on `GitHubIssue`
//! and `GitHubPullRequest`, never on a comment node. It is exactly
//! [`crate::ids::body_digest`] of the subject issue body, which is also an input
//! to [`crate::ids::run_id`], so a row carrying it binds the event
//! to the precise issue body the run was opened for. A caller that also wants
//! to detect a *comment* body changing after acceptance hashes the exact
//! comment body it accepted and pins that hash durably; that is a separate
//! concern from this row contract.
//!
//! The remaining row properties (repository, subject, Project, item, the
//! comment's node ID, and the comment body) come from the graph relations
//! between the comment, the issue, and the Project item. They are query
//! projections rather than Source spellings, so each reaction names them in its
//! own row schema.
//!
//! # What acceptance proves
//!
//! [`accept_event_row`] is the single seam between a query row and a trusted
//! event. It fails closed on the first violation and proves, in order:
//!
//! 1. the comment is **unedited** (`isEdited == false`);
//! 2. the author is **exactly** the configured trusted author
//!    (`authorDatabaseId` + `authorType`; see [`crate::trust`]);
//! 3. `bodyDigest` is a well-formed `sha256:<64-hex>` digest;
//! 4. the comment body parses under the **strict** `WorkGraphEvent/v1` grammar
//!    into a fully validated event (which also proves the deterministic
//!    `eventId` derivation);
//! 5. the event is the **expected type**;
//! 6. the event names the row's Project item and subject; and
//! 7. the event's `runId` is exactly
//!    `run_id(projectItemNodeId, bodyDigest)`.
//!
//! Nothing here consults the network: acceptance is a pure function of the row.
//! A reaction still re-reads live GitHub state before it writes, but it does so
//! to confirm the row is *still* true, never to discover which event to act on.

use crate::comment::{parse_comment, CommentError};
use crate::event::{EventError, RunId, Sha256Digest, WorkGraphEvent, WorkGraphEventType};
use crate::ids::run_id;
use crate::trust::{ActorType, AuthorIdentity, TrustedAuthor};

/// The Source's spelling of the subject issue's body digest.
pub const BODY_DIGEST_FIELD: &str = "bodyDigest";
/// The Source's spelling of the comment author's numeric database ID.
pub const AUTHOR_DATABASE_ID_FIELD: &str = "authorDatabaseId";
/// The Source's spelling of the comment author's actor type.
pub const AUTHOR_TYPE_FIELD: &str = "authorType";
/// The Source's spelling of the comment's edited flag.
pub const IS_EDITED_FIELD: &str = "isEdited";

/// The authoritative values one query row carries for one WorkGraph comment.
///
/// Borrowed rather than owned so a reaction can build it from its own typed row
/// struct without cloning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRow<'a> {
    /// The Project (v2) item node ID the row binds the event to.
    pub project_item_node_id: &'a str,
    /// The subject issue node ID the row binds the event to.
    pub subject_node_id: &'a str,
    /// The subject issue's `bodyDigest`, exactly as the Source projected it.
    pub body_digest: &'a str,
    /// The comment's `authorDatabaseId`.
    pub author_database_id: u64,
    /// The comment's `authorType` token.
    pub author_type: &'a str,
    /// The comment's `isEdited` flag.
    pub is_edited: bool,
    /// The comment body, exactly as the Source projected it.
    pub body: &'a str,
}

/// One accepted row: a trusted, unedited, fully bound WorkGraph event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedEventRow {
    /// The validated event the comment carried.
    pub event: WorkGraphEvent,
    /// The human summary line the comment carried.
    pub summary: String,
    /// The run the row binds to, re-derived from the row (never trusted from
    /// the event JSON).
    pub run_id: RunId,
    /// The parsed subject issue body digest.
    pub body_digest: Sha256Digest,
    /// The observed author identity, for logs and errors.
    pub author: AuthorIdentity,
}

/// Why a Source row was not accepted.
///
/// Every variant is a **permanent** rejection of that exact row: retrying the
/// identical row can never change the outcome.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RowError {
    /// `isEdited` was true.
    #[error("{IS_EDITED_FIELD} is true: an edited comment is never trusted")]
    Edited,
    /// `authorType` was not a GitHub actor type.
    #[error("{AUTHOR_TYPE_FIELD} '{0}' is not a GitHub actor type")]
    ActorType(String),
    /// The author was not the configured trusted author.
    #[error("comment author {observed} is not the trusted author {trusted}")]
    Untrusted {
        /// The observed author.
        observed: AuthorIdentity,
        /// The configured trusted author.
        trusted: TrustedAuthor,
    },
    /// `bodyDigest` was not a `sha256:<64-hex>` digest.
    #[error("{BODY_DIGEST_FIELD} is invalid: {0}")]
    BodyDigest(#[source] EventError),
    /// The comment body did not parse under the strict grammar.
    #[error("comment body is not a valid WorkGraphEvent/v1 comment: {0}")]
    Body(#[source] CommentError),
    /// The event was not the expected type.
    #[error("comment carries a {observed} event, not {expected}")]
    EventType {
        /// The event type the comment carried.
        observed: WorkGraphEventType,
        /// The event type the reaction requires.
        expected: WorkGraphEventType,
    },
    /// The event named a different Project item than the row.
    #[error("event names project item '{observed}', not the row's '{expected}'")]
    ProjectItem {
        /// The item the event named.
        observed: String,
        /// The item the row named.
        expected: String,
    },
    /// The event named a different subject than the row.
    #[error("event names subject '{observed}', not the row's '{expected}'")]
    Subject {
        /// The subject the event named.
        observed: String,
        /// The subject the row named.
        expected: String,
    },
    /// The event's `runId` was not derived from the row's own binding.
    #[error(
        "event runId '{observed}' is not the run '{expected}' derived from the row's \
         projectItemNodeId and {BODY_DIGEST_FIELD}"
    )]
    RunId {
        /// The run the event claimed.
        observed: RunId,
        /// The run the row derives.
        expected: RunId,
    },
}

/// Accept one Source row as a trusted event of `expected_type`.
///
/// See the [module documentation](self) for the exact contract and the order in
/// which it is enforced.
pub fn accept_event_row(
    row: &EventRow<'_>,
    trusted: &TrustedAuthor,
    expected_type: WorkGraphEventType,
) -> Result<AcceptedEventRow, RowError> {
    if row.is_edited {
        return Err(RowError::Edited);
    }
    let actor_type = ActorType::from_token(row.author_type)
        .ok_or_else(|| RowError::ActorType(row.author_type.to_string()))?;
    let author = AuthorIdentity::new(row.author_database_id, actor_type);
    if !trusted.matches(&author) {
        return Err(RowError::Untrusted {
            observed: author,
            trusted: *trusted,
        });
    }

    let body_digest =
        Sha256Digest::try_from(row.body_digest.to_string()).map_err(RowError::BodyDigest)?;
    let comment = parse_comment(row.body).map_err(RowError::Body)?;

    let observed_type = comment.event.event_type();
    if observed_type != expected_type {
        return Err(RowError::EventType {
            observed: observed_type,
            expected: expected_type,
        });
    }
    if comment.event.project_item_node_id != row.project_item_node_id {
        return Err(RowError::ProjectItem {
            observed: comment.event.project_item_node_id.clone(),
            expected: row.project_item_node_id.to_string(),
        });
    }
    if comment.event.subject_node_id != row.subject_node_id {
        return Err(RowError::Subject {
            observed: comment.event.subject_node_id.clone(),
            expected: row.subject_node_id.to_string(),
        });
    }
    let expected_run = run_id(row.project_item_node_id, &body_digest);
    if comment.event.run_id != expected_run {
        return Err(RowError::RunId {
            observed: comment.event.run_id.clone(),
            expected: expected_run,
        });
    }

    Ok(AcceptedEventRow {
        event: comment.event,
        summary: comment.summary,
        run_id: expected_run,
        body_digest,
        author,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::render_comment;
    use crate::event::{
        AssignedResponsibilityType, ExecutionId, ExecutionStartedPayload, ProfileRef,
        ResponsibilityAssignedPayload, WorkGraphEventPayload,
    };
    use crate::ids::body_digest;
    use crate::summary::summary_for;

    const ITEM: &str = "PVTI_lADOABCDEF4AbcDEzgXYZ123";
    const SUBJECT: &str = "I_kwDOABCDEF6ABCDE";
    const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";
    const ISSUE_BODY: &str = "Please validate this issue.\n";
    const DATABASE_ID: u64 = 4021243;

    fn trusted() -> TrustedAuthor {
        TrustedAuthor::new(DATABASE_ID, ActorType::Bot)
    }

    fn digest() -> Sha256Digest {
        body_digest(Some(ISSUE_BODY))
    }

    fn assignment_event() -> WorkGraphEvent {
        WorkGraphEvent::new(
            run_id(ITEM, &digest()),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
                responsibility_type: AssignedResponsibilityType::IssueValidation,
                profile_ref: ProfileRef::new("issue-validator", BLOB).expect("profile"),
                content_digest: digest(),
            }),
        )
        .expect("event")
    }

    fn body_for(event: &WorkGraphEvent) -> String {
        let summary = summary_for(event);
        render_comment(event, &summary).expect("render")
    }

    fn row<'a>(body: &'a str, digest: &'a str) -> EventRow<'a> {
        EventRow {
            project_item_node_id: ITEM,
            subject_node_id: SUBJECT,
            body_digest: digest,
            author_database_id: DATABASE_ID,
            author_type: "Bot",
            is_edited: false,
            body,
        }
    }

    #[test]
    fn a_trusted_unedited_bound_row_is_accepted() {
        let event = assignment_event();
        let body = body_for(&event);
        let digest = digest();
        let accepted = accept_event_row(
            &row(&body, digest.as_str()),
            &trusted(),
            WorkGraphEventType::ResponsibilityAssigned,
        )
        .expect("accepted");
        assert_eq!(accepted.event, event);
        assert_eq!(accepted.run_id, event.run_id);
        assert_eq!(accepted.body_digest, digest);
        assert_eq!(
            accepted.author,
            AuthorIdentity::new(DATABASE_ID, ActorType::Bot)
        );
        assert!(!accepted.summary.is_empty());
    }

    #[test]
    fn an_edited_row_is_rejected_before_anything_else() {
        let body = body_for(&assignment_event());
        let digest = digest();
        let mut row = row(&body, digest.as_str());
        row.is_edited = true;
        // Also make the author untrusted: `isEdited` must be the reported cause.
        row.author_database_id = 999;
        assert_eq!(
            accept_event_row(&row, &trusted(), WorkGraphEventType::ResponsibilityAssigned)
                .expect_err("edited"),
            RowError::Edited
        );
    }

    #[test]
    fn an_untrusted_author_is_rejected() {
        let body = body_for(&assignment_event());
        let digest = digest();

        let mut wrong_id = row(&body, digest.as_str());
        wrong_id.author_database_id = DATABASE_ID + 1;
        assert!(matches!(
            accept_event_row(
                &wrong_id,
                &trusted(),
                WorkGraphEventType::ResponsibilityAssigned
            ),
            Err(RowError::Untrusted { .. })
        ));

        let mut wrong_type = row(&body, digest.as_str());
        wrong_type.author_type = "User";
        assert!(matches!(
            accept_event_row(
                &wrong_type,
                &trusted(),
                WorkGraphEventType::ResponsibilityAssigned
            ),
            Err(RowError::Untrusted { .. })
        ));

        let mut unknown_type = row(&body, digest.as_str());
        unknown_type.author_type = "bot";
        assert_eq!(
            accept_event_row(
                &unknown_type,
                &trusted(),
                WorkGraphEventType::ResponsibilityAssigned
            )
            .expect_err("unknown actor type"),
            RowError::ActorType("bot".to_string())
        );
    }

    #[test]
    fn a_malformed_body_digest_is_rejected() {
        let body = body_for(&assignment_event());
        for value in [
            "",
            "sha256:nothex",
            "deadbeef",
            &format!("sha256:{}", "a".repeat(63)),
        ] {
            assert!(
                matches!(
                    accept_event_row(
                        &row(&body, value),
                        &trusted(),
                        WorkGraphEventType::ResponsibilityAssigned
                    ),
                    Err(RowError::BodyDigest(_))
                ),
                "'{value}' must be rejected"
            );
        }
    }

    #[test]
    fn a_legacy_or_unparseable_body_is_rejected() {
        let digest = digest();
        let event = assignment_event();
        let json = event.to_canonical_json();
        for body in [
            json.clone(),
            format!("WorkGraphEvent/v1\n```json\n{json}\n```"),
            format!("WorkGraphEvent/v1\n\nSummary\n\n{json}\n\nthanks!"),
            "not a workgraph comment".to_string(),
        ] {
            assert!(
                matches!(
                    accept_event_row(
                        &row(&body, digest.as_str()),
                        &trusted(),
                        WorkGraphEventType::ResponsibilityAssigned
                    ),
                    Err(RowError::Body(_))
                ),
                "body must be rejected: {body}"
            );
        }
    }

    #[test]
    fn a_wrong_event_type_is_rejected() {
        let run = run_id(ITEM, &digest());
        let started = WorkGraphEvent::new(
            run.clone(),
            ITEM,
            SUBJECT,
            WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
                execution_id: ExecutionId::from_run_id(&run),
                task_id: "task-1".to_string(),
            }),
        )
        .expect("event");
        let body = body_for(&started);
        let digest = digest();
        assert_eq!(
            accept_event_row(
                &row(&body, digest.as_str()),
                &trusted(),
                WorkGraphEventType::ResponsibilityAssigned
            )
            .expect_err("wrong type"),
            RowError::EventType {
                observed: WorkGraphEventType::ExecutionStarted,
                expected: WorkGraphEventType::ResponsibilityAssigned,
            }
        );
    }

    #[test]
    fn a_row_that_does_not_bind_the_event_is_rejected() {
        let body = body_for(&assignment_event());
        let digest = digest();

        let mut other_item = row(&body, digest.as_str());
        other_item.project_item_node_id = "PVTI_other";
        assert!(matches!(
            accept_event_row(
                &other_item,
                &trusted(),
                WorkGraphEventType::ResponsibilityAssigned
            ),
            Err(RowError::ProjectItem { .. })
        ));

        let mut other_subject = row(&body, digest.as_str());
        other_subject.subject_node_id = "I_other";
        assert!(matches!(
            accept_event_row(
                &other_subject,
                &trusted(),
                WorkGraphEventType::ResponsibilityAssigned
            ),
            Err(RowError::Subject { .. })
        ));
    }

    #[test]
    fn a_body_digest_from_a_different_body_breaks_the_run_binding() {
        let body = body_for(&assignment_event());
        let other = body_digest(Some("a different issue body"));
        assert!(matches!(
            accept_event_row(
                &row(&body, other.as_str()),
                &trusted(),
                WorkGraphEventType::ResponsibilityAssigned
            ),
            Err(RowError::RunId { .. })
        ));
    }
}
