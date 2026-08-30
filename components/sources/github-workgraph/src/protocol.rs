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
//! * `WorkGraphTaskAssign/v1` — lifecycle artifact markers
//! * `WorkGraphTaskDispatch/v1`
//! * `WorkGraphTaskResult/v1`
//! * `WorkGraphTaskEvaluate/v1`
//!
//! Core does **not** parse WorkGraph JSON semantics; the projector does.

use async_trait::async_trait;
use drasi_core::models::SourceChange;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

// ── Exact marker prefixes recognized by Core ──────────────────────────────

/// Issue body prefix for WorkGraph tasks.
pub const WORKGRAPH_TASK_MARKER: &str = "WorkGraphTask/v1\n";

/// Comment body prefixes for WorkGraph lifecycle artifacts.
pub const WORKGRAPH_ASSIGN_MARKER: &str = "WorkGraphTaskAssign/v1\n";
pub const WORKGRAPH_DISPATCH_MARKER: &str = "WorkGraphTaskDispatch/v1\n";
pub const WORKGRAPH_RESULT_MARKER: &str = "WorkGraphTaskResult/v1\n";
pub const WORKGRAPH_EVALUATE_MARKER: &str = "WorkGraphTaskEvaluate/v1\n";

/// Returns true if `body` begins with any WorkGraph lifecycle artifact marker.
pub fn is_workgraph_lifecycle_marker(body: &str) -> bool {
    body.starts_with(WORKGRAPH_ASSIGN_MARKER)
        || body.starts_with(WORKGRAPH_DISPATCH_MARKER)
        || body.starts_with(WORKGRAPH_RESULT_MARKER)
        || body.starts_with(WORKGRAPH_EVALUATE_MARKER)
}

/// Returns the lifecycle marker kind for trust classification.
///
/// `Assign` and `Dispatch` use assigner trust; `Result` and `Evaluate` use
/// reporter trust.
pub fn lifecycle_trust_role(body: &str) -> Option<LifecycleTrustRole> {
    if body.starts_with(WORKGRAPH_ASSIGN_MARKER) || body.starts_with(WORKGRAPH_DISPATCH_MARKER) {
        Some(LifecycleTrustRole::Assigner)
    } else if body.starts_with(WORKGRAPH_RESULT_MARKER)
        || body.starts_with(WORKGRAPH_EVALUATE_MARKER)
    {
        Some(LifecycleTrustRole::Reporter)
    } else {
        None
    }
}

/// Trust role for lifecycle artifact author/editor checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleTrustRole {
    /// Assign/Dispatch — requires assigner trust.
    Assigner,
    /// Result/Evaluate — requires reporter trust.
    Reporter,
}

// ── Normalized document types ─────────────────────────────────────────────

/// A workflow definition document fetched from the configured repository file.
///
/// `source_key` is deterministic from the exact configured
/// `repository/ref/path` (see [`definition_source_key`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DefinitionDocument {
    pub source_key: String,
    pub body: String,
}

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
}

/// GitHub locator metadata carried separately from the task protocol document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubIssueLocator {
    pub source_key: String,
    pub repository_owner: String,
    pub repository_name: String,
    pub issue_number: u64,
    pub issue_node_id: String,
}

/// A normalized input for the WorkGraph projection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProjectionInput {
    RecordIssueRevision { source_key: String, revision: i64 },
    UpsertDefinition(DefinitionDocument),
    DeleteDefinition { source_key: String },
    UpsertRootIssue(RootIssueDocument),
    DeleteRootIssue { source_key: String },
    UpsertTask(TaskDocument),
    DeleteTask { source_key: String },
    UpsertLifecycleArtifact(LifecycleArtifactDocument),
    DeleteLifecycleArtifact { source_key: String },
    UpsertLocator(GitHubIssueLocator),
    DeleteLocator { source_key: String },
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphTaskBinding {
    pub source_key: String,
    pub task_id: String,
    pub task_element_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphAssignmentBinding {
    pub source_key: String,
    pub task_source_key: String,
    pub task_id: String,
    pub assignment_id: String,
    pub permitted_executors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphDispatchBinding {
    pub source_key: String,
    pub task_source_key: String,
    pub task_id: String,
    pub assignment_id: String,
    pub lease_id: String,
    pub executor_id: String,
    pub slot_id: String,
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

/// Build the deterministic `source_key` for a workflow definition from the
/// exact configured repository, ref, and path.
///
/// Format: `github:definition:{repository}:{ref}:{path}`
///
/// This is documented as the canonical identity for the definition document.
pub fn definition_source_key(repository: &str, git_ref: &str, path: &str) -> String {
    format!("github:definition:{repository}:{git_ref}:{path}")
}

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
