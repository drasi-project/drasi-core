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

//! Bounded WorkGraph integration types and projector seam.
//!
//! Core owns the normalized document types, the bounded opaque checkpoint, and
//! the object-safe `WorkGraphProjector` trait that Dogfood's adapter
//! implements. Core recognizes exact marker prefixes only:
//!
//! * `WorkGraphTask/v1` — task body marker
//! * `WorkGraphTaskAssignment/v1` — lifecycle artifact markers
//! * `WorkGraphTaskFork/v1`
//! * `WorkGraphTaskJoin/v1`
//! * `WorkGraphTaskDispatch/v1`
//! * `WorkGraphTaskResult/v1`
//! * `WorkGraphTaskEvaluation/v1`
//! * `WorkGraphTaskRoute/v1`
//! * `WorkGraphTaskError/v1`
//!
//! Core does **not** parse WorkGraph JSON semantics; the projector does.

use async_trait::async_trait;
use drasi_core::models::SourceChange;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::Arc;

// ── Exact marker prefixes recognized by Core ──────────────────────────────

/// Issue body prefix for WorkGraph tasks.
pub const WORKGRAPH_TASK_MARKER: &str = "WorkGraphTask/v1\n";

/// Comment body prefixes for WorkGraph lifecycle artifacts.
pub const WORKGRAPH_ASSIGNMENT_MARKER: &str = "WorkGraphTaskAssignment/v1\n";
pub const WORKGRAPH_FORK_MARKER: &str = "WorkGraphTaskFork/v1\n";
pub const WORKGRAPH_JOIN_MARKER: &str = "WorkGraphTaskJoin/v1\n";
pub const WORKGRAPH_DISPATCH_MARKER: &str = "WorkGraphTaskDispatch/v1\n";
pub const WORKGRAPH_RESULT_MARKER: &str = "WorkGraphTaskResult/v1\n";
pub const WORKGRAPH_EVALUATION_MARKER: &str = "WorkGraphTaskEvaluation/v1\n";
pub const WORKGRAPH_ROUTE_MARKER: &str = "WorkGraphTaskRoute/v1\n";
pub const WORKGRAPH_ERROR_MARKER: &str = "WorkGraphTaskError/v1\n";

/// Canonical accepted evaluator verdict.
pub const WORKGRAPH_EVALUATION_ACCEPTED: &str = "accepted";
/// Canonical rejected evaluator verdict.
pub const WORKGRAPH_EVALUATION_REJECTED: &str = "rejected";
/// Canonical Route action that requests another bounded worker attempt.
pub const WORKGRAPH_ROUTE_REWORK: &str = "rework";
/// Maximum human Root Issue comment body forwarded to the projector.
pub const MAX_ROOT_ISSUE_COMMENT_BODY_BYTES: usize = 64 * 1024;
/// Hard upper bound on bounded worker attempts for one task Assignment.
///
/// Core owns this allocator bound. The injected projector supplies it as
/// [`WorkGraphRouteBinding::max_attempts`] because definition-declared rework
/// policy now lives in the Reaction.
pub const MAX_WORKGRAPH_ATTEMPTS: u64 = 64;

/// Namespace shared by every WorkGraph-owned canonical identifier.
pub const WORKGRAPH_ID_NAMESPACE: &str = "urn:drasi:workgraph:id:v1";

/// Derive a canonical typed WorkGraph identifier.
///
/// The digest covers the namespace, type, and semantic inputs in order. Each
/// UTF-8 part is framed by its unsigned 64-bit big-endian byte length.
pub fn derive_workgraph_id(id_type: &str, semantic_inputs: &[&str]) -> String {
    let mut digest = Sha256::new();
    for part in std::iter::once(WORKGRAPH_ID_NAMESPACE)
        .chain(std::iter::once(id_type))
        .chain(semantic_inputs.iter().copied())
    {
        let bytes = part.as_bytes();
        let length = u64::try_from(bytes.len()).expect("WorkGraph ID input length fits in u64");
        digest.update(length.to_be_bytes());
        digest.update(bytes);
    }
    format!(
        "{WORKGRAPH_ID_NAMESPACE}:{id_type}:sha256:{}",
        hex::encode(digest.finalize())
    )
}

/// Return whether `value` has the exact canonical grammar for `id_type`.
pub fn is_typed_workgraph_id(value: &str, id_type: &str) -> bool {
    value
        .strip_prefix(&format!("{WORKGRAPH_ID_NAMESPACE}:{id_type}:sha256:"))
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

/// Returns true if `body` begins with any WorkGraph lifecycle artifact marker.
pub fn is_workgraph_lifecycle_marker(body: &str) -> bool {
    body.starts_with(WORKGRAPH_ASSIGNMENT_MARKER)
        || body.starts_with(WORKGRAPH_FORK_MARKER)
        || body.starts_with(WORKGRAPH_JOIN_MARKER)
        || body.starts_with(WORKGRAPH_DISPATCH_MARKER)
        || body.starts_with(WORKGRAPH_RESULT_MARKER)
        || body.starts_with(WORKGRAPH_EVALUATION_MARKER)
        || body.starts_with(WORKGRAPH_ROUTE_MARKER)
        || body.starts_with(WORKGRAPH_ERROR_MARKER)
}

/// Returns the lifecycle marker kind for trust classification.
///
/// `Assignment`, `Fork`, `Join`, and `Dispatch` use assigner trust; `Result`,
/// `Evaluation`, `Route`, and `Error` use reporter trust.
pub fn lifecycle_trust_role(body: &str) -> Option<LifecycleTrustRole> {
    if body.starts_with(WORKGRAPH_ASSIGNMENT_MARKER)
        || body.starts_with(WORKGRAPH_FORK_MARKER)
        || body.starts_with(WORKGRAPH_JOIN_MARKER)
        || body.starts_with(WORKGRAPH_DISPATCH_MARKER)
    {
        Some(LifecycleTrustRole::Assigner)
    } else if body.starts_with(WORKGRAPH_RESULT_MARKER)
        || body.starts_with(WORKGRAPH_EVALUATION_MARKER)
        || body.starts_with(WORKGRAPH_ROUTE_MARKER)
        || body.starts_with(WORKGRAPH_ERROR_MARKER)
    {
        Some(LifecycleTrustRole::Reporter)
    } else {
        None
    }
}

/// Trust role for lifecycle artifact author/editor checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleTrustRole {
    /// Assignment/Fork/Join/Dispatch — requires assigner trust.
    Assigner,
    /// Result/Evaluation/Route/Error — requires reporter trust.
    Reporter,
}

// ── Normalized document types ─────────────────────────────────────────────

/// A Root Issue admitted into WorkGraph by the exact `workgraph` label.
///
/// `admission_id` identifies one continuous labeled generation. Ordinary edits
/// preserve it; removing and later re-adding the label creates a new value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootIssueDocument {
    pub source_key: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub repository_node_id: String,
    pub issue_number: u64,
    pub title: String,
    pub body: String,
    pub is_open: bool,
    pub admission_id: String,
    /// Sorted, case-sensitive `workgraph:` labels from the source Issue.
    pub workgraph_labels: Vec<String>,
    /// False only for the exact `workgraph:ignore` or `workgraph:error` label.
    pub workgraph_include: bool,
}

/// An ordinary GitHub Issue normalized by Core for generic Source projection.
///
/// WorkGraph protocol interpretation remains projector-owned. Core only
/// supplies authenticated GitHub identity, content, state, and exact label
/// inclusion metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubIssueDocument {
    pub source_key: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub repository_node_id: String,
    pub issue_database_id: u64,
    pub issue_number: u64,
    pub title: String,
    pub body: String,
    pub is_open: bool,
    pub state_reason: String,
    pub labels: Vec<String>,
    pub workgraph_labels: Vec<String>,
    pub workgraph_include: bool,
}

/// A WorkGraph task document derived from a GitHub issue whose body begins with
/// `WorkGraphTask/v1\n`.
///
/// `source_key` is the GitHub issue node ID. The five locator fields
/// (`sourceKey`, `repositoryOwner`, `repositoryName`, `issueNumber`,
/// `issueNodeId`) are carried in a separate [`GitHubIssueLocator`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskDocument {
    pub source_key: String,
    pub body: String,
    pub is_open: bool,
    pub state_reason: String,
    pub parent_source_key: Option<String>,
    /// Sorted, case-sensitive `workgraph:` labels from the source Issue.
    pub workgraph_labels: Vec<String>,
    /// False only for the exact `workgraph:ignore` or `workgraph:error` label.
    pub workgraph_include: bool,
}

/// A WorkGraph lifecycle artifact from a GitHub issue comment whose body begins
/// with one of the exact lifecycle markers.
///
/// `source_key` is the comment node ID. `task_source_key` is the issue node
/// ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LifecycleArtifactDocument {
    pub source_key: String,
    pub task_source_key: String,
    pub body: String,
    /// Immutable GitHub comment creation time as Unix milliseconds.
    pub created_at_revision: i64,
    /// Latest authoritative GitHub comment update time as Unix milliseconds.
    pub updated_at_revision: i64,
}

/// GitHub locator metadata carried separately from the task protocol document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubIssueLocator {
    pub source_key: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub issue_database_id: u64,
    pub issue_number: u64,
    pub issue_node_id: String,
}

/// A human-authored comment on the currently admitted Root Issue generation.
///
/// Core authenticates and normalizes this bounded evidence but does not assign
/// workflow meaning to it. The injected projector decides whether it qualifies
/// as wait/resume evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootIssueCommentDocument {
    pub source_key: String,
    pub root_issue_id: String,
    pub admission_id: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub repository_node_id: String,
    pub issue_number: u64,
    pub author_id: String,
    pub author_type: String,
    pub author_login: String,
    pub body: String,
    pub created_at_revision: i64,
    pub updated_at_revision: i64,
}

/// A normalized input for the WorkGraph projection.
///
/// Every variant carries observed GitHub state. Core never fetches or projects
/// a workflow definition: the pinned `WorkGraphWorkflowDefinition/v1` document
/// is loaded by the Reaction, which owns every definition-dependent decision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProjectionInput {
    RecordIssueRevision {
        source_key: String,
        revision: i64,
        state_fingerprint: String,
        authorization_transition: bool,
    },
    UpsertRootIssue(RootIssueDocument),
    DeleteRootIssue {
        source_key: String,
    },
    UpsertGitHubIssue(GitHubIssueDocument),
    DeleteGitHubIssue {
        source_key: String,
    },
    UpsertTask(TaskDocument),
    DeleteTask {
        source_key: String,
    },
    UpsertLifecycleArtifact(LifecycleArtifactDocument),
    DeleteLifecycleArtifact {
        source_key: String,
        updated_at_revision: i64,
    },
    UpsertLocator(GitHubIssueLocator),
    DeleteLocator {
        source_key: String,
    },
    UpsertRootIssueComment(RootIssueCommentDocument),
    DeleteRootIssueComment {
        source_key: String,
        root_issue_id: String,
        admission_id: String,
        repository_owner: String,
        repository_name: String,
        repository_node_id: String,
        issue_number: u64,
        updated_at_revision: i64,
    },
}

/// Complete bounded allocator projection derived by the trusted WorkGraph
/// projector.
///
/// Core validates the projection against authenticated normalized documents
/// and reconciles its Source-owned queue/lease state to this exact desired set
/// in the same transaction as the projector graph changes and checkpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphAllocatorProjection {
    pub tasks: Vec<WorkGraphTaskBinding>,
    pub assignments: Vec<WorkGraphAssignmentBinding>,
    pub dispatches: Vec<WorkGraphDispatchBinding>,
    pub results: Vec<WorkGraphResultBinding>,
    pub evaluations: Vec<WorkGraphEvaluateBinding>,
    pub routes: Vec<WorkGraphRouteBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphTaskBinding {
    pub source_key: String,
    pub root_issue_id: String,
    pub workflow_run_id: String,
    pub task_id: String,
    pub task_element_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphAssignmentBinding {
    pub source_key: String,
    pub task_source_key: String,
    pub root_issue_id: String,
    pub workflow_run_id: String,
    pub task_id: String,
    pub assignment_id: String,
    pub permitted_executors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphDispatchBinding {
    pub source_key: String,
    pub task_source_key: String,
    pub root_issue_id: String,
    pub workflow_run_id: String,
    pub task_id: String,
    pub assignment_id: String,
    pub lease_id: String,
    pub executor_id: String,
    pub slot_id: String,
}

/// Identity-bearing Result representation used by Core to validate direct
/// task/root/run lookup without parsing the projector-owned payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphResultBinding {
    pub source_key: String,
    pub task_source_key: String,
    pub root_issue_id: String,
    pub workflow_run_id: String,
    pub task_id: String,
    pub result_id: String,
    pub lease_id: String,
    pub attempt: u64,
}

/// Identity-bearing Evaluate representation used by Core to validate direct
/// task/root/run lookup without parsing the projector-owned payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphEvaluateBinding {
    pub source_key: String,
    pub task_source_key: String,
    pub root_issue_id: String,
    pub workflow_run_id: String,
    pub task_id: String,
    pub result_id: String,
    pub evaluation_id: String,
    pub attempt: u64,
    pub verdict: String,
}

/// Identity-bearing Route representation. Core does not parse Route bodies;
/// the injected projector supplies these canonical fields after selecting the
/// authoritative Result/Evaluation chain.
///
/// `max_attempts` is the allocator's attempt bound, not a workflow policy: the
/// projector no longer reads the workflow definition, so it supplies
/// [`MAX_WORKGRAPH_ATTEMPTS`] — the same bound an Assignment starts with. The
/// Reaction owns the definition-declared rework policy and simply stops
/// writing rework Routes once it is exhausted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphRouteBinding {
    pub source_key: String,
    pub task_source_key: String,
    pub root_issue_id: String,
    pub workflow_run_id: String,
    pub task_id: String,
    pub result_id: String,
    pub evaluation_id: String,
    pub route_id: String,
    pub action: String,
    pub attempt: u64,
    pub max_attempts: u64,
}

// ── Object-safe projector trait ───────────────────────────────────────────

/// The result of a successful `prepare()` call: an ordered graph-change batch,
/// complete allocator projection, and optional diagnostic.
pub struct PreparedProjection {
    /// Ordered changes to append to the WAL.
    pub changes: Vec<SourceChange>,
    /// Complete canonical allocator projection parsed by the trusted
    /// projector and reconciled by Core in the same WAL transaction.
    pub allocator: WorkGraphAllocatorProjection,
    /// If non-empty, the projection was rejected but the changes are
    /// fail-closed retractions that must still be durably appended.
    pub rejection: Option<String>,
    /// True when the projector's internal state was modified by this
    /// preparation. Even a no-op (identical replay) may update evidence
    /// tables.
    pub state_changed: bool,
    /// Opaque post-transition state used to restore the projector after a
    /// restart. It must be a bounded snapshot, not an append-only event log.
    pub checkpoint: Vec<u8>,
    /// Commit token bound to this exact prepared state.
    pub commit: Box<dyn PreparedProjectionCommit>,
}

/// Commit token returned by [`WorkGraphProjector::prepare`].
#[async_trait]
pub trait PreparedProjectionCommit: Send {
    /// Install the prepared projector state after the WAL append succeeds.
    async fn commit(self: Box<Self>);
}

/// Object-safe async trait for the WorkGraph graph projector.
///
/// The staged semantics are:
///
/// 1. Core calls `prepare(inputs, effective_from)` on a clone/staged copy.
/// 2. The prepared object exposes ordered `SourceChange`s, the complete
///    canonical allocator projection, and an optional rejection diagnostic.
/// 3. Core reconciles allocator state and durably appends all changes through
///    the existing WAL.
/// 4. Core calls `commit()` on the prepared object, which replaces the
///    projector's live state. **No state commit before WAL.**
///
/// This guarantees that a WAL append failure leaves the projector
/// uncommitted and recoverable.
#[async_trait]
pub trait WorkGraphProjector: Send + Sync {
    /// Prepare a projection: stage the input against a cloned/staged copy of
    /// the projector state and return the ordered changes.
    ///
    /// The projector must not commit its internal state until [`commit`] is
    /// called.
    async fn prepare(
        &self,
        inputs: Vec<ProjectionInput>,
        effective_from: u64,
    ) -> anyhow::Result<PreparedProjection>;

    /// Restore the projector from its durable checkpoint before accepting new
    /// deliveries. Called once during startup recovery.
    async fn restore(&self, checkpoint: &[u8]) -> anyhow::Result<()>;

    /// Return the source ID this projector was constructed for.
    fn source_id(&self) -> &str;
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Diagnostic reason for untrusted WorkGraph lifecycle artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkGraphTrustRejection {
    /// The configured `protocolTrust` is missing entirely.
    NoTrustConfigured,
    /// The author is not in the trusted set for the required role.
    AuthorUntrusted,
    /// The editor is not in the trusted set for the required role.
    EditorUntrusted,
    /// The comment was edited but the editor identity is unknown.
    UnattributedEdit,
    /// The issue is not a WorkGraph task (body does not begin with marker).
    NotWorkGraphTask,
}

impl fmt::Display for WorkGraphTrustRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTrustConfigured => write!(f, "no protocolTrust configured"),
            Self::AuthorUntrusted => write!(f, "author not in trusted identity set"),
            Self::EditorUntrusted => write!(f, "editor not in trusted identity set"),
            Self::UnattributedEdit => write!(f, "comment was edited but editor identity unknown"),
            Self::NotWorkGraphTask => {
                write!(f, "issue body does not begin with WorkGraph task marker")
            }
        }
    }
}

#[cfg(test)]
mod id_tests {
    use super::*;

    #[test]
    fn canonical_id_cross_vectors_match() {
        assert_eq!(
            derive_workgraph_id("admission", &["I_root", "delivery-123"]),
            "urn:drasi:workgraph:id:v1:admission:sha256:\
             c9bf2ec95516dc3e0168f7e977291e590f2c5443230db669faf4496f5bf89b61"
        );
        assert_eq!(
            derive_workgraph_id("lease", &["task-α", "assignment-β", "42"]),
            "urn:drasi:workgraph:id:v1:lease:sha256:\
             fdb83caf2b77a5b61b70c62d9cdb3111d7a2dbe10b57a99f3a5997634ee81a68"
        );
    }

    #[test]
    fn typed_id_validation_is_exact() {
        let task = derive_workgraph_id("task", &["semantic"]);
        assert!(is_typed_workgraph_id(&task, "task"));
        assert!(!is_typed_workgraph_id(&task, "assignment"));
        assert!(!is_typed_workgraph_id(
            &task.replace("c7f491", "C7F491"),
            "task"
        ));
        assert!(!is_typed_workgraph_id(
            "urn:drasi:workgraph:id:v1:task:sha256:abc",
            "task"
        ));
    }
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    #[test]
    fn fork_and_join_markers_have_exact_spelling() {
        assert_eq!(WORKGRAPH_FORK_MARKER, "WorkGraphTaskFork/v1\n");
        assert_eq!(WORKGRAPH_JOIN_MARKER, "WorkGraphTaskJoin/v1\n");
    }

    #[test]
    fn fork_and_join_are_recognized_lifecycle_markers() {
        for marker in [
            WORKGRAPH_ASSIGNMENT_MARKER,
            WORKGRAPH_FORK_MARKER,
            WORKGRAPH_JOIN_MARKER,
            WORKGRAPH_DISPATCH_MARKER,
            WORKGRAPH_RESULT_MARKER,
            WORKGRAPH_EVALUATION_MARKER,
            WORKGRAPH_ROUTE_MARKER,
            WORKGRAPH_ERROR_MARKER,
        ] {
            assert!(
                is_workgraph_lifecycle_marker(&format!("{marker}\n```json\n{{}}\n```\n")),
                "{marker} must be a lifecycle marker"
            );
        }
        assert!(!is_workgraph_lifecycle_marker(WORKGRAPH_TASK_MARKER));
        // Similar-but-wrong spellings are not lifecycle markers.
        assert!(!is_workgraph_lifecycle_marker("WorkGraphTaskFork/v2\nbody"));
        assert!(!is_workgraph_lifecycle_marker(
            "WorkGraphTaskJoins/v1\nbody"
        ));
    }

    #[test]
    fn fork_and_join_require_assigner_trust() {
        assert_eq!(
            lifecycle_trust_role(&format!("{WORKGRAPH_FORK_MARKER}body")),
            Some(LifecycleTrustRole::Assigner)
        );
        assert_eq!(
            lifecycle_trust_role(&format!("{WORKGRAPH_JOIN_MARKER}body")),
            Some(LifecycleTrustRole::Assigner)
        );
        // Fork/Join match Assignment/Dispatch, not the reporter roles.
        assert_eq!(
            lifecycle_trust_role(&format!("{WORKGRAPH_ASSIGNMENT_MARKER}body")),
            Some(LifecycleTrustRole::Assigner)
        );
        assert_eq!(
            lifecycle_trust_role(&format!("{WORKGRAPH_RESULT_MARKER}body")),
            Some(LifecycleTrustRole::Reporter)
        );
        assert_eq!(lifecycle_trust_role("not a marker"), None);
    }
}
