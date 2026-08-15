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

#![deny(missing_docs)]

//! The one WorkGraph event format, shared by every WorkGraph component.
//!
//! This crate is the single definition of:
//!
//! * the [outer comment grammar](comment) — `WorkGraphEvent/v1`, one summary
//!   line, one raw JSON object, no Markdown fence;
//! * the [common event envelope](event) and its four exact payloads;
//! * the [deterministic identifiers](ids) — body digest, `runId`, `eventId`;
//! * the [generated human summaries](summary);
//! * the [authoritative Source row contract](row) every reaction is triggered
//!   by;
//! * [duplicate coalescing and conflict detection](dedup); and
//! * the [immutable author identity](trust) every component keys trust on.
//!
//! Nothing else in the repository may re-implement any of these.
//!
//! # Trust model
//!
//! The event JSON asserts **only** correlation: which run, which Project Item,
//! which subject, and what happened. It carries no actor, repository, issue
//! number, subject type, timestamp, or human summary, because a comment body is
//! written by whoever holds the token and can therefore claim anything.
//!
//! Identity and time come from GitHub's own immutable metadata as delivered by
//! the authoritative GitHub Source. That Source projects exactly four author
//! fields — `authorId`, `authorDatabaseId`, `authorType`, `authorLogin` — and
//! trust is keyed on `authorDatabaseId` + `authorType` alone. `authorId` is
//! audit data and `authorLogin` is display-only (logins can be renamed and
//! reclaimed). No GitHub App ID is involved; see [`trust`] for the exact
//! contract and its limitations. Repository, issue number, and subject type
//! come from the graph relation between the comment, the issue, and the Project
//! Item.
//!
//! # Minimal workflow
//!
//! ```text
//! reaction/http      -> ResponsibilityAssigned  -> status AwaitingValidation
//! copilot-agent-task -> ExecutionStarted
//! issue-validator    -> CompletedIssueValidation
//! workgraph-router   -> RoutingDecided          -> status AwaitingIssueRiskProfiling
//!                                                      or NeedsMoreInformation
//! ```
//!
//! Only the middle and last steps are WorkGraph-specific reactions. The
//! assignment step is the generic [`reaction/http`] reaction driven by a query,
//! so this repository ships no WorkGraph-specific assignment component.
//!
//! [`reaction/http`]: https://github.com/drasi-project/drasi-core/tree/main/components/reactions/http
//!
//! # Example
//!
//! ```
//! use drasi_workgraph_common::{
//!     comment::{parse_comment, render_comment},
//!     event::{
//!         AssignedResponsibilityType, ProfileRef, ResponsibilityAssignedPayload,
//!         WorkGraphEvent, WorkGraphEventPayload,
//!     },
//!     ids::{body_digest, run_id},
//!     summary::{summary_for, SubjectRef},
//! };
//!
//! let item = "PVTI_lADOABCDEF4AbcDEzgXYZ123";
//! let subject = "I_kwDOABCDEF6ABCDE";
//! let digest = body_digest(Some("Please validate this."));
//! let run = run_id(item, subject, &digest);
//!
//! let event = WorkGraphEvent::new(
//!     run,
//!     item,
//!     subject,
//!     WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
//!         responsibility_type: AssignedResponsibilityType::IssueValidation,
//!         profile_ref: ProfileRef::new(
//!             "issue-validator",
//!             "0123456789abcdef0123456789abcdef01234567",
//!         )?,
//!         content_digest: digest,
//!     }),
//! )?;
//!
//! let summary = summary_for(
//!     &event,
//!     SubjectRef { repository: "drasi-project/drasi-core", number: 742 },
//! );
//! let body = render_comment(&event, &summary)?;
//! assert_eq!(parse_comment(&body)?.event, event);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod comment;
pub mod dedup;
pub mod event;
pub mod ids;
pub mod row;
pub mod summary;
pub mod trust;

pub use comment::{
    is_forbidden_summary_char, parse_comment, render_comment, CommentError, WorkGraphComment,
    COMMENT_MARKER, MAX_SUMMARY_CHARS,
};
pub use dedup::{adopt_published_event, coalesce, DuplicateError, ObservedComment};
pub use event::{
    AssignedResponsibilityType, CompletedIssueValidationPayload, EventError, EventId, ExecutionId,
    ExecutionStartedPayload, NextResponsibilityType, ProfileRef, ResponsibilityAssignedPayload,
    RoutingDecidedPayload, RoutingFromStatus, RoutingToStatus, RunId, Sha256Digest,
    ValidationOutcome, ValidationReasonCode, WorkGraphEvent, WorkGraphEventPayload,
    WorkGraphEventType, SCHEMA_VERSION,
};
pub use ids::{body_digest, event_id, run_id};
pub use row::{
    accept_event_row, AcceptedEventRow, EventRow, RowError, AUTHOR_DATABASE_ID_FIELD,
    AUTHOR_TYPE_FIELD, BODY_DIGEST_FIELD, IS_EDITED_FIELD,
};
pub use summary::{summary_for, SubjectRef};
pub use trust::{
    author_identity_from_github_user, author_identity_from_source_row, is_trusted,
    validate_trusted_author, ActorType, AuthorIdentity, TrustError, TrustedAuthor,
};

/// Project status tokens used by the minimal WorkGraph workflow.
pub mod status {
    /// Set once a responsibility is assigned (by the generic HTTP reaction
    /// that writes the `ResponsibilityAssigned` comment).
    pub const AWAITING_VALIDATION: &str = "AwaitingValidation";
    /// Terminal status for a passing validation.
    pub const AWAITING_ISSUE_RISK_PROFILING: &str = "AwaitingIssueRiskProfiling";
    /// Terminal status for a failing validation.
    pub const NEEDS_MORE_INFORMATION: &str = "NeedsMoreInformation";
}
