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
//! * `WorkGraphTaskAssignmentRequest/v1`
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
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

// ── Exact marker prefixes recognized by Core ──────────────────────────────

/// Issue body prefix for WorkGraph tasks.
pub const WORKGRAPH_TASK_MARKER: &str = "WorkGraphTask/v1\n";

/// Comment body prefixes for WorkGraph lifecycle artifacts.
pub const WORKGRAPH_ASSIGNMENT_MARKER: &str = "WorkGraphTaskAssignment/v1\n";
/// The action that puts a candidate set in front of one named assigner.
///
/// It is a distinct marker, not a longer spelling of the Assignment marker:
/// `WorkGraphTaskAssignment/v1\n` and `WorkGraphTaskAssignmentRequest/v1\n`
/// diverge before either terminates, so neither can ever prefix the other.
pub const WORKGRAPH_ASSIGNMENT_REQUEST_MARKER: &str = "WorkGraphTaskAssignmentRequest/v1\n";
pub const WORKGRAPH_FORK_MARKER: &str = "WorkGraphTaskFork/v1\n";
pub const WORKGRAPH_JOIN_MARKER: &str = "WorkGraphTaskJoin/v1\n";
pub const WORKGRAPH_DISPATCH_MARKER: &str = "WorkGraphTaskDispatch/v1\n";
pub const WORKGRAPH_RESULT_MARKER: &str = "WorkGraphTaskResult/v1\n";
pub const WORKGRAPH_EVALUATION_MARKER: &str = "WorkGraphTaskEvaluation/v1\n";
pub const WORKGRAPH_ROUTE_MARKER: &str = "WorkGraphTaskRoute/v1\n";
pub const WORKGRAPH_ERROR_MARKER: &str = "WorkGraphTaskError/v1\n";

// ── Exact admission label vocabulary ──────────────────────────────────────

/// The legacy admission label. It is the selector of the implicit mapping
/// derived from the top-level `workflowDefinition` configuration block.
pub const WORKGRAPH_ADMISSION_LABEL: &str = "workgraph";

/// Reserved universal exclusion modifier. It never activates a mapping.
pub const WORKGRAPH_IGNORE_LABEL: &str = "workgraph:ignore";

/// Reserved universal exclusion modifier. It never activates a mapping.
pub const WORKGRAPH_ERROR_LABEL: &str = "workgraph:error";

/// Required prefix of every configured mapping selector label.
pub const WORKGRAPH_LABEL_PREFIX: &str = "workgraph:";

/// Reserved mapping ID of the implicit legacy `workflowDefinition` mapping.
pub const LEGACY_WORKFLOW_MAPPING_ID: &str = "workgraph";

/// Canonical accepted evaluator verdict.
pub const WORKGRAPH_EVALUATION_ACCEPTED: &str = "accepted";
/// Canonical rejected evaluator verdict.
pub const WORKGRAPH_EVALUATION_REJECTED: &str = "rejected";
/// Canonical Route action that requests another bounded worker attempt.
pub const WORKGRAPH_ROUTE_REWORK: &str = "rework";
/// Maximum human Root Issue comment body forwarded to the projector.
pub const MAX_ROOT_ISSUE_COMMENT_BODY_BYTES: usize = 64 * 1024;
/// The exact mention that opens a natural task response.
///
/// A human addresses the workflow by opening the first non-whitespace line of
/// a task Issue comment with this mention. Core authenticates and binds that
/// comment; it never interprets what the human wrote.
pub const WORKGRAPH_RESPONSE_MENTION: &str = "@workgraph";
/// Domain separator framed into every task response body digest.
///
/// This is the kernel's `derive_workgraph_response_body_digest` contract. The
/// digest binds a normalized Response to the exact raw body it came from, so
/// Core and the projector must derive it identically or the kernel refuses the
/// Response it produces.
pub const WORKGRAPH_RESPONSE_BODY_DIGEST_DOMAIN: &str = "workgraph-v1-task-response-body";
/// Maximum natural task response body forwarded to the projector.
///
/// This matches the human reply bound the Reaction already defaults to
/// (`DEFAULT_MAX_REPLY_BYTES`), so both human channels admit the same amount
/// of text. The raw body is carried verbatim and encoded as utf-8-hex
/// downstream, which still fits the 64 KiB WorkGraph body budget at this
/// bound.
pub const MAX_TASK_RESPONSE_BODY_BYTES: usize = 16 * 1024;
// The body is carried raw and hex-encoded downstream, so the widest admitted
// response must still fit the WorkGraph body budget.
const _: () = assert!(MAX_TASK_RESPONSE_BODY_BYTES * 2 <= 64 * 1024);

/// Derives the framed digest that binds a task response to its exact body.
///
/// Length-framed so no two `(domain, body)` pairs can collide by
/// concatenation, and prefixed exactly as the kernel writes it. This is *not*
/// a plain SHA-256 of the body.
pub fn derive_workgraph_response_body_digest(body: &str) -> String {
    let mut digest = Sha256::new();
    for part in [WORKGRAPH_RESPONSE_BODY_DIGEST_DOMAIN, body] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("sha256:{:x}", digest.finalize())
}

/// Whether a comment's first non-whitespace line opens with the WorkGraph
/// mention.
///
/// GitHub mentions are case-insensitive, so every ASCII case variant of
/// `@workgraph` addresses the protocol. The mention must end there:
/// `@workgraphs` and `@workgraph-bot` are different mentions and are not ours.
pub fn body_opens_with_workgraph_mention(body: &str) -> bool {
    let Some(first) = body
        .lines()
        .map(str::trim_start)
        .find(|line| !line.is_empty())
    else {
        return false;
    };
    let mention = WORKGRAPH_RESPONSE_MENTION;
    if first.len() < mention.len()
        || !first
            .get(..mention.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(mention))
    {
        return false;
    }
    first[mention.len()..]
        .chars()
        .next()
        .is_none_or(|next| !next.is_alphanumeric() && next != '_' && next != '-')
}
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
        || body.starts_with(WORKGRAPH_ASSIGNMENT_REQUEST_MARKER)
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
/// `Assignment`, `AssignmentRequest`, `Fork`, `Join`, and `Dispatch` use
/// assigner trust; `Result`, `Evaluation`, `Route`, and `Error` use reporter
/// trust. An AssignmentRequest routes work to an actor exactly as an
/// Assignment does — it just asks the question instead of answering it — so it
/// belongs to the same trusted writer role.
pub fn lifecycle_trust_role(body: &str) -> Option<LifecycleTrustRole> {
    if body.starts_with(WORKGRAPH_ASSIGNMENT_MARKER)
        || body.starts_with(WORKGRAPH_ASSIGNMENT_REQUEST_MARKER)
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
    /// Assignment/AssignmentRequest/Fork/Join/Dispatch — requires assigner
    /// trust.
    Assigner,
    /// Result/Evaluation/Route/Error — requires reporter trust.
    Reporter,
}

// ── Normalized document types ─────────────────────────────────────────────

/// One active admission of one configured Source label→workflow mapping on a
/// Root Issue.
///
/// A mapping activation is created the first time its selector label is
/// observed on the Issue and is retracted the moment the label is removed.
/// Re-adding the same label later creates a *new* `admission_id`, so a
/// retracted generation can never be resumed.
///
/// The three definition fields are projected verbatim from Source
/// configuration. The Source never fetches, parses, or interprets the file
/// they address: they only tell the Reaction *which* pinned definition this
/// activation belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootMappingAdmission {
    /// Configured mapping ID. The implicit legacy mapping uses
    /// [`LEGACY_WORKFLOW_MAPPING_ID`].
    pub mapping_id: String,
    /// The exact, case-sensitive selector label that activated the mapping.
    pub label: String,
    /// The admission generation ID of this mapping activation.
    pub admission_id: String,
    /// Root Issue title frozen when this mapping activation began.
    pub title: String,
    /// Root Issue body frozen when this mapping activation began.
    pub body: String,
    /// `owner/name` of the repository holding the pinned definition.
    pub definition_repository: String,
    /// The exact git ref pinning the definition.
    pub definition_ref: String,
    /// The exact repository-relative path of the definition file.
    pub definition_path: String,
}

/// A Root Issue admitted into WorkGraph by at least one configured selector
/// label.
///
/// `workflow_mappings` is the ordered (by `mapping_id`) set of *currently
/// active* mapping admissions. One Issue may carry several simultaneously;
/// each is an independent generation.
///
/// `admission_id` is retained for compatibility with Root Issue comment
/// identity and existing persisted state. It is selected deterministically:
/// the legacy [`LEGACY_WORKFLOW_MAPPING_ID`] activation when it is active,
/// otherwise the first entry of `workflow_mappings`. New Reaction logic must
/// read `workflow_mappings` instead.
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
    /// Ordered active mapping admissions, unique by `mapping_id` and `label`.
    #[serde(default)]
    pub workflow_mappings: Vec<RootMappingAdmission>,
    /// Sorted, case-sensitive `workgraph:` labels from the source Issue.
    pub workgraph_labels: Vec<String>,
    /// False only for the exact `workgraph:ignore` or `workgraph:error` label.
    pub workgraph_include: bool,
}

impl RootIssueDocument {
    /// The deterministic legacy admission ID for a set of active mapping
    /// admissions: the legacy activation when present, otherwise the first
    /// ordered activation.
    pub fn legacy_admission_id(mappings: &[RootMappingAdmission]) -> Option<&str> {
        mappings
            .iter()
            .find(|mapping| mapping.mapping_id == LEGACY_WORKFLOW_MAPPING_ID)
            .or_else(|| mappings.first())
            .map(|mapping| mapping.admission_id.as_str())
    }

    /// The active admission of one mapping ID, if that mapping is active.
    pub fn mapping_admission(&self, mapping_id: &str) -> Option<&RootMappingAdmission> {
        self.workflow_mappings
            .iter()
            .find(|mapping| mapping.mapping_id == mapping_id)
    }

    /// Every currently active admission generation, ordered and deduplicated.
    ///
    /// A legacy document persisted before mapping admissions existed carries
    /// only [`Self::admission_id`], which is then the single active admission.
    pub fn active_admission_ids(&self) -> Vec<String> {
        if self.workflow_mappings.is_empty() {
            return vec![self.admission_id.clone()];
        }
        self.workflow_mappings
            .iter()
            .map(|mapping| mapping.admission_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
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

/// One GitHub account currently assigned to a WorkGraph task Issue.
///
/// The numeric `database_id` is the stable identity across renames and across
/// the legacy and next-generation node ID encodings; `node_id` and `login`
/// are carried so the actor catalog can be matched without a second read.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskAssignee {
    pub database_id: u64,
    pub node_id: String,
    pub login: String,
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
    /// Assignees sorted by numeric ID and deduplicated. Defaulted so a
    /// document persisted before assignee authority existed still loads.
    #[serde(default)]
    pub assignees: Vec<TaskAssignee>,
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

/// A human-authored comment on an admitted Root Issue generation.
///
/// Core authenticates and normalizes this bounded evidence but does not assign
/// workflow meaning to it. The injected projector decides whether it qualifies
/// as wait/resume evidence.
///
/// `admission_ids` is the ordered, deduplicated set of *every* mapping
/// admission that was active on the Root Issue when the comment was observed.
/// A Root Issue may carry several simultaneous mapping activations, so binding
/// a comment to one of them would make its meaning depend on which activation
/// happened to be selected as the compatibility [`Self::admission_id`]. The
/// comment stays relevant while any of its admissions is still active, and a
/// consumer matching it against a specific workflow run must require that
/// run's *own* mapping admission to be in this set.
///
/// `admission_id` is retained for identity compatibility only. It is the same
/// deterministic compatibility selection [`RootIssueDocument::admission_id`]
/// carries, and new logic must read `admission_ids` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootIssueCommentDocument {
    pub source_key: String,
    pub root_issue_id: String,
    pub admission_id: String,
    /// Ordered, deduplicated active mapping admissions at observation time.
    #[serde(default)]
    pub admission_ids: Vec<String>,
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

impl RootIssueCommentDocument {
    /// Every admission this comment was created under.
    ///
    /// A legacy document persisted before comment admission sets existed
    /// carries only [`Self::admission_id`], which is then its single admission.
    pub fn effective_admission_ids(&self) -> Vec<&str> {
        if self.admission_ids.is_empty() {
            return vec![self.admission_id.as_str()];
        }
        self.admission_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    }
}

/// The lifecycle role a natural task response speaks in.
///
/// A response always answers an open lifecycle subject: a human worker
/// answers the Dispatch it holds a lease for, a human assigner answers the
/// AssignmentRequest that named it, and a human evaluator answers a Result
/// that is still awaiting its Evaluation. A comment with no open subject is
/// not a response at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskResponseRole {
    Worker,
    Assigner,
    Evaluator,
}

impl TaskResponseRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Assigner => "assigner",
            Self::Evaluator => "evaluator",
        }
    }
}

/// A natural-language response a catalog human wrote on a WorkGraph task
/// Issue.
///
/// The comment opens with [`WORKGRAPH_RESPONSE_MENTION`] on its first
/// non-whitespace line. Core authenticates the author against the `version: 2`
/// human actor catalog and the Issue's current WorkGraph-managed assignees,
/// binds the comment to the exact task identity and open lifecycle subject it
/// answers, and fences it by revision. Core never interprets the body: what
/// the human meant is the Reaction's call.
///
/// The open subject also decides which identity may speak: a worker is matched
/// against the metadata its lease was acquired with, an assigner against the
/// exact actor its AssignmentRequest named, and an evaluator against the
/// current catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskResponseDocument {
    /// The comment node ID.
    pub source_key: String,
    /// The task Issue node ID the comment was written on.
    pub task_source_key: String,
    /// The catalog actor ID the author was authenticated as.
    pub actor_id: String,
    pub task_id: String,
    pub root_issue_id: String,
    pub workflow_run_id: String,
    /// Which open lifecycle subject this response answers.
    pub role: TaskResponseRole,
    /// The Dispatch a worker response answers. Present exactly when
    /// [`Self::role`] is [`TaskResponseRole::Worker`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_id: Option<String>,
    /// The lease a worker response holds. Present exactly when
    /// [`Self::role`] is [`TaskResponseRole::Worker`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    /// The Result an evaluator response answers. Present exactly when
    /// [`Self::role`] is [`TaskResponseRole::Evaluator`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_id: Option<String>,
    /// The AssignmentRequest an assigner response answers. Present exactly
    /// when [`Self::role`] is [`TaskResponseRole::Assigner`].
    ///
    /// Defaulted and omitted when absent so every response persisted before
    /// assigner ingress existed still loads and still serializes byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub author_database_id: u64,
    pub author_id: String,
    pub author_login: String,
    /// The comment body, carried verbatim.
    pub body: String,
    /// The kernel's `derive_workgraph_response_body_digest` over
    /// [`Self::body`]: a framed, domain-separated digest, not a plain
    /// SHA-256. It binds this document to the exact body a Response will be
    /// normalized from.
    ///
    /// This is distinct from Core's own document fingerprint, which fences
    /// revisions in the ledger and covers every field of this document.
    pub body_digest: String,
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
    UpsertTaskResponse(TaskResponseDocument),
    DeleteTaskResponse {
        source_key: String,
        task_source_key: String,
        task_id: String,
        actor_id: String,
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
    /// Candidate sets currently in front of a named assigner.
    ///
    /// Defaulted so a projector written before first-class assigners existed
    /// still produces an identical projection: no requests, no assigner
    /// subject, and no change to any other binding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignment_requests: Vec<WorkGraphAssignmentRequestBinding>,
    pub assignments: Vec<WorkGraphAssignmentBinding>,
    pub dispatches: Vec<WorkGraphDispatchBinding>,
    pub results: Vec<WorkGraphResultBinding>,
    pub evaluations: Vec<WorkGraphEvaluateBinding>,
    pub routes: Vec<WorkGraphRouteBinding>,
}

/// Identity-bearing `WorkGraphTaskAssignmentRequest/v1` representation: one
/// task's candidate set put in front of exactly one named assigner.
///
/// A request is an *action*, not authority. It allocates nothing: no lease, no
/// slot, no queue entry, no attempt. Its only allocator effect is to open the
/// assigner subject a human may answer with a natural response, which the
/// Assignment that follows retires. Core does not parse the request body; the
/// trusted projector supplies these canonical fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkGraphAssignmentRequestBinding {
    pub source_key: String,
    pub task_source_key: String,
    pub root_issue_id: String,
    pub workflow_run_id: String,
    pub task_id: String,
    /// Canonical `assignment-request` typed ID.
    pub request_id: String,
    /// The single actor asked to decide. It is never one of the candidates.
    pub assigner_id: String,
    /// The executors the assigner may choose from, in canonical sorted order.
    pub candidates: Vec<String>,
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
    /// The AssignmentRequest this Assignment answers, when it is
    /// decision-bound.
    ///
    /// A legacy Assignment names no request and is unchanged: absent here, and
    /// omitted from the wire entirely. A decision-bound Assignment names the
    /// request it closes, which is what retires that request's open assigner
    /// subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The normalized assigner Response the decision was read from, when the
    /// assigner was a human. Absent for an agent assigner, which decides
    /// without a comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// The actor that decided. Present exactly when [`Self::request_id`] is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigner_id: Option<String>,
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
    /// Canonical `dispatch` ID of the Dispatch this binding represents.
    ///
    /// Defaulted so a projector written before natural task responses existed
    /// still binds; Core then has no Dispatch subject to bind a worker
    /// response to and refuses one rather than inventing an identity.
    #[serde(default)]
    pub dispatch_id: String,
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
            WORKGRAPH_ASSIGNMENT_REQUEST_MARKER,
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

    /// The AssignmentRequest marker is its own exact spelling, and neither it
    /// nor the Assignment marker can be read as the other.
    #[test]
    fn the_assignment_request_marker_is_exact_and_never_aliases_an_assignment() {
        assert_eq!(
            WORKGRAPH_ASSIGNMENT_REQUEST_MARKER,
            "WorkGraphTaskAssignmentRequest/v1\n"
        );
        let request = format!("{WORKGRAPH_ASSIGNMENT_REQUEST_MARKER}\n```json\n{{}}\n```\n");
        let assignment = format!("{WORKGRAPH_ASSIGNMENT_MARKER}\n```json\n{{}}\n```\n");
        assert!(!request.starts_with(WORKGRAPH_ASSIGNMENT_MARKER));
        assert!(!assignment.starts_with(WORKGRAPH_ASSIGNMENT_REQUEST_MARKER));
        // Near-miss spellings a spoofed body would reach for are not markers.
        for spoof in [
            "WorkGraphTaskAssignmentRequest/v2\nbody",
            "WorkGraphTaskAssignmentRequests/v1\nbody",
            "WorkGraphTaskAssignmentRequest/v1 \nbody",
            "WorkGraphTaskAssignmentRequest\nbody",
            " WorkGraphTaskAssignmentRequest/v1\nbody",
            "workgraphtaskassignmentrequest/v1\nbody",
        ] {
            assert!(!is_workgraph_lifecycle_marker(spoof), "{spoof}");
            assert_eq!(lifecycle_trust_role(spoof), None, "{spoof}");
        }
    }

    #[test]
    fn an_assignment_request_requires_the_same_trusted_writer_as_an_assignment() {
        assert_eq!(
            lifecycle_trust_role(&format!("{WORKGRAPH_ASSIGNMENT_REQUEST_MARKER}body")),
            Some(LifecycleTrustRole::Assigner)
        );
        assert_eq!(
            lifecycle_trust_role(&format!("{WORKGRAPH_ASSIGNMENT_MARKER}body")),
            lifecycle_trust_role(&format!("{WORKGRAPH_ASSIGNMENT_REQUEST_MARKER}body"))
        );
    }

    /// A human assigner speaks in its own role, spelled exactly as the kernel
    /// writes it on a normalized Response.
    #[test]
    fn the_assigner_response_role_has_the_kernels_exact_spelling() {
        assert_eq!(TaskResponseRole::Assigner.as_str(), "assigner");
        assert_eq!(
            serde_json::to_value(TaskResponseRole::Assigner).expect("serialize role"),
            serde_json::Value::String("assigner".to_string())
        );
        // The roles that existed before are untouched.
        assert_eq!(TaskResponseRole::Worker.as_str(), "worker");
        assert_eq!(TaskResponseRole::Evaluator.as_str(), "evaluator");
    }

    /// A projection and an Assignment written before first-class assigners
    /// existed serialize exactly as they always did.
    #[test]
    fn assigner_fields_never_reach_a_projection_that_did_not_opt_into_them() {
        let legacy = WorkGraphAssignmentBinding {
            source_key: "IC_assignment".to_string(),
            task_source_key: "I_task".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: derive_workgraph_id("workflow-run", &["run"]),
            task_id: derive_workgraph_id("task", &["task"]),
            assignment_id: derive_workgraph_id("assignment", &["assignment"]),
            permitted_executors: vec!["build-agent".to_string()],
            request_id: None,
            response_id: None,
            assigner_id: None,
        };
        let projection = WorkGraphAllocatorProjection {
            assignments: vec![legacy.clone()],
            ..WorkGraphAllocatorProjection::default()
        };
        let body = serde_json::to_string(&projection).expect("serialize projection");
        for absent in [
            "assignmentRequests",
            "requestId",
            "responseId",
            "assignerId",
        ] {
            assert!(!body.contains(absent), "{absent} must not appear: {body}");
        }
        // ...and a projector that never learned the new fields still parses.
        let restored: WorkGraphAllocatorProjection =
            serde_json::from_str(&body).expect("parse legacy projection");
        assert_eq!(restored, projection);
        assert!(restored.assignment_requests.is_empty());
        assert_eq!(restored.assignments[0], legacy);
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

    /// Fixed cross-component vectors for the task response body digest.
    ///
    /// The kernel derives this digest as
    /// `sha256:framed_sha256(["workgraph-v1-task-response-body", body])` and
    /// refuses a Response whose `bodyDigest` differs. These literals are the
    /// shared contract: Core, the kernel, and the projector must all produce
    /// them, so a change on any side fails here instead of drifting silently.
    const RESPONSE_BODY_DIGEST_VECTORS: [(&str, &str); 3] = [
        (
            "",
            "sha256:24b9c212b4ac4f20f1aa5736811c87160cb443ee40bd38e39e719c5e89621743",
        ),
        (
            "@workgraph ready",
            "sha256:26e307a3254e75d9de0f7d884d58fe199a6af7eeaec7cd00f00e21f10b8d63c3",
        ),
        (
            "@workgraph looks good, shipping it",
            "sha256:e85beef21c44c265825ca0b6fc461e6f580debac902577978ae005a9f1994467",
        ),
    ];

    #[test]
    fn task_response_body_digest_matches_the_shared_fixed_vectors() {
        for (body, expected) in RESPONSE_BODY_DIGEST_VECTORS {
            assert_eq!(
                derive_workgraph_response_body_digest(body),
                expected,
                "digest drifted for {body:?}"
            );
        }
    }

    #[test]
    fn the_response_body_digest_is_domain_separated_not_a_plain_sha256() {
        let body = "@workgraph ready";
        let plain = format!("sha256:{:x}", Sha256::digest(body.as_bytes()));
        assert_ne!(derive_workgraph_response_body_digest(body), plain);
        // Framing is what stops a body from impersonating the domain prefix.
        assert_ne!(
            derive_workgraph_response_body_digest(body),
            derive_workgraph_response_body_digest(&format!(
                "{WORKGRAPH_RESPONSE_BODY_DIGEST_DOMAIN}{body}"
            ))
        );
        assert_eq!(
            WORKGRAPH_RESPONSE_BODY_DIGEST_DOMAIN,
            "workgraph-v1-task-response-body"
        );
    }

    #[test]
    fn the_workgraph_mention_is_case_insensitive_with_an_exact_boundary() {
        // GitHub mentions are case-insensitive, so every ASCII case variant
        // addresses the protocol.
        for body in [
            "@workgraph ready",
            "@WorkGraph ready",
            "@WORKGRAPH ready",
            "@wOrKgRaPh ready",
            "   @workgraph indented",
            "\n\n@workgraph after blank lines",
            "@workgraph",
            "@workgraph, with punctuation",
            "@workgraph:done",
            "@workgraph\nmore text",
        ] {
            assert!(
                body_opens_with_workgraph_mention(body),
                "{body:?} must open with the mention"
            );
        }

        // A longer mention is a different account, and the mention must open
        // the first non-whitespace line.
        for body in [
            "@workgraphs ready",
            "@workgraph-bot ready",
            "@workgraph_bot ready",
            "@workgraphbot",
            "@WORKGRAPHS ready",
            "not a mention",
            "context first\n@workgraph later",
            "",
            "   ",
        ] {
            assert!(
                !body_opens_with_workgraph_mention(body),
                "{body:?} must not open with the mention"
            );
        }
    }

    #[test]
    fn the_task_response_body_bound_matches_the_human_reply_default() {
        // The same bound the Reaction defaults to for a human reply, so the
        // two human channels never disagree about how much a person may say.
        assert_eq!(MAX_TASK_RESPONSE_BODY_BYTES, 16 * 1024);
    }
}
