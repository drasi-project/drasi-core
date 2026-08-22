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

use chrono::{DateTime, Utc};
use drasi_github_workgraph::validate_task_lease;
pub use drasi_github_workgraph::{TaskLease, SUPPORTED_AGENT_PROFILES};
use serde::{Deserialize, Serialize};

const TASK_FAMILY: &str = "WorkGraphTask/";
const ASSIGNMENT_FAMILY: &str = "WorkGraphTaskAssignment/";
const RESULT_FAMILY: &str = "WorkGraphTaskResult/";
const LEASE_FAMILY: &str = "WorkGraphTaskLease/";
const LEASE_EXPIRATION_FAMILY: &str = "WorkGraphTaskLeaseExpiration/";
const ACCEPTANCE_FAMILY: &str = "WorkGraphTaskResultAcceptance/";
const YAML_FENCE: &str = "yaml";
const JSON_FENCE: &str = "json";
const FENCE_SUFFIX: &str = "\n```\n";
const V1: &str = "v1";
const V2: &str = "v2";
/// Upper bound on any opaque identifier the Source will accept from a
/// specialized comment. GitHub node IDs are far shorter; the bound only exists
/// to reject unbounded values before they reach the graph.
const MAX_ID_LEN: usize = 256;
/// Upper bound on the free-text `reason` of a Lease Expiration.
const MAX_REASON_LEN: usize = 512;
pub mod error_code {
    pub const UNSUPPORTED_VERSION: &str = "unsupported-workgraph-version";
    pub const INVALID_ENVELOPE: &str = "invalid-envelope";
    pub const INVALID_YAML: &str = "invalid-yaml";
    pub const INVALID_JSON: &str = "invalid-json";
    pub const JSON_NOT_OBJECT: &str = "json-not-object";
    pub const NON_CANONICAL_JSON: &str = "non-canonical-json";
    pub const INVALID_TASK_PAYLOAD: &str = "invalid-task-payload";
    pub const INVALID_ASSIGNMENT_PAYLOAD: &str = "invalid-assignment-payload";
    pub const INVALID_RESULT_PAYLOAD: &str = "invalid-result-payload";
    pub const INVALID_ACCEPTANCE_PAYLOAD: &str = "invalid-acceptance-payload";
    pub const INVALID_LEASE_PAYLOAD: &str = "invalid-lease-payload";
    pub const INVALID_LEASE_EXPIRATION_PAYLOAD: &str = "invalid-lease-expiration-payload";
}

macro_rules! wire_enum {
    ($(#[$doc:meta])* $name:ident { $($variant:ident = $wire:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name { $(#[serde(rename = $wire)] $variant),+ }
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $wire),+ }
            }
        }
    };
}

wire_enum! {
    TaskType {
        ValidateIssue = "validate-issue",
        RequestInfo = "request-info",
    }
}

wire_enum! {
    Outcome { Succeeded = "succeeded", Failed = "failed", Blocked = "blocked" }
}

macro_rules! strict {
    ($(struct $name:ident { $($field:ident: $ty:ty),+ $(,)? })+) => {$(
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty),+ }
    )+};
}

strict! {
    struct ValidateIssueInputs { validation_profile: String }
    struct RequestInfoInputs { validation_result_comment_node_id: String }
    struct ValidateIssueResult { criteria: Vec<CriterionResult> }
    struct CriterionResult { criterion: String, passed: bool, evidence: String }
    struct RequestInfoResult { request_comment_node_id: String }
    struct ResultAcceptance {
        result_comment_node_id: String,
        result_body_digest: String,
        summary: String
    }
}

/// Historical `WorkGraphTaskAssignment/v1` wire object: exactly `agentProfile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssignmentRootV1 {
    agent_profile: String,
}

/// Canonical `WorkGraphTaskAssignment/v2` wire object: exactly `agentProfile`
/// and `workerId`, in that order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssignmentRootV2 {
    agent_profile: String,
    worker_id: String,
}

/// Serializes to the exact canonical wire object of whichever Assignment
/// version was read, so the canonical-formatting check stays byte-exact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
enum AssignmentWire {
    V1(AssignmentRootV1),
    V2(AssignmentRootV2),
}

/// A parsed Assignment of either version. `worker_id` is `Some` exactly when
/// `version == 2`; a v1 Assignment names an agent profile but no worker queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskAssignment {
    pub version: u8,
    pub agent_profile: String,
    pub worker_id: Option<String>,
}

/// Canonical `WorkGraphTaskLeaseExpiration/v1` wire object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskLeaseExpiration {
    pub lease_comment_node_id: String,
    pub lease_id: String,
    pub expired_at: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TaskInputs {
    ValidateIssue(ValidateIssueInputs),
    RequestInfo(RequestInfoInputs),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TaskResult {
    ValidateIssue(ValidateIssueResult),
    RequestInfo(RequestInfoResult),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDefinition {
    pub task_type: TaskType,
    pub inputs: TaskInputs,
}

/// A parsed Result of either version. `lease_id` is `Some` exactly when
/// `version == 2`; the field is serialized in canonical position (immediately
/// after `taskType`) and omitted entirely for v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkResult {
    #[serde(skip)]
    pub version: u8,
    pub task_type: TaskType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    pub outcome: Outcome,
    pub summary: String,
    pub result: TaskResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkGraphError {
    pub code: &'static str,
    pub message: String,
}

impl WorkGraphError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskClassification {
    Task(Box<TaskDefinition>),
    Invalid(WorkGraphError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentClassification {
    Ordinary,
    Assignment(Box<TaskAssignment>),
    Result(Box<WorkResult>),
    Acceptance(Box<ResultAcceptance>),
    Lease(Box<TaskLease>),
    LeaseExpiration(Box<TaskLeaseExpiration>),
    Invalid(WorkGraphError),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TaskRoot {
    task_type: TaskType,
    inputs: serde_yaml::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResultRootV1 {
    task_type: TaskType,
    outcome: Outcome,
    summary: String,
    result: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResultRootV2 {
    task_type: TaskType,
    lease_id: String,
    outcome: Outcome,
    summary: String,
    result: serde_json::Value,
}

pub fn classify_task_body(body: &str) -> TaskClassification {
    let yaml_text = match exact_envelope(body, TASK_FAMILY, YAML_FENCE, "task", &[V1]) {
        Ok((_, text)) => text,
        Err(error) => return TaskClassification::Invalid(error),
    };
    let root: TaskRoot = match serde_yaml::from_str(yaml_text) {
        Ok(root) => root,
        Err(error) => {
            return TaskClassification::Invalid(WorkGraphError::new(
                error_code::INVALID_YAML,
                format!("invalid task YAML: {error}"),
            ))
        }
    };
    match parse_task(root) {
        Ok(task) => TaskClassification::Task(Box::new(task)),
        Err(message) => TaskClassification::Invalid(WorkGraphError::new(
            error_code::INVALID_TASK_PAYLOAD,
            message,
        )),
    }
}

pub fn classify_comment(body: &str) -> CommentClassification {
    // Families are mutually exclusive because every marker is followed by a
    // '/', so `WorkGraphTaskLeaseExpiration/` never matches `WorkGraphTaskLease/`
    // and `WorkGraphTaskResultAcceptance/` never matches `WorkGraphTaskResult/`.
    if body.starts_with(ASSIGNMENT_FAMILY) {
        return parse_json_comment(
            body,
            ASSIGNMENT_FAMILY,
            "task Assignment",
            &[V1, V2],
            error_code::INVALID_ASSIGNMENT_PAYLOAD,
            parse_assignment,
            |wire| {
                CommentClassification::Assignment(Box::new(match wire {
                    AssignmentWire::V1(v1) => TaskAssignment {
                        version: 1,
                        agent_profile: v1.agent_profile,
                        worker_id: None,
                    },
                    AssignmentWire::V2(v2) => TaskAssignment {
                        version: 2,
                        agent_profile: v2.agent_profile,
                        worker_id: Some(v2.worker_id),
                    },
                }))
            },
        );
    }
    if body.starts_with(LEASE_EXPIRATION_FAMILY) {
        return parse_json_comment(
            body,
            LEASE_EXPIRATION_FAMILY,
            "task Lease Expiration",
            &[V1],
            error_code::INVALID_LEASE_EXPIRATION_PAYLOAD,
            |_, value| parse_lease_expiration(value),
            |value| CommentClassification::LeaseExpiration(Box::new(value)),
        );
    }
    if body.starts_with(LEASE_FAMILY) {
        return parse_json_comment(
            body,
            LEASE_FAMILY,
            "task Lease",
            &[V1],
            error_code::INVALID_LEASE_PAYLOAD,
            |_, value| parse_lease(value),
            |value| CommentClassification::Lease(Box::new(value)),
        );
    }
    if body.starts_with(RESULT_FAMILY) {
        return parse_json_comment(
            body,
            RESULT_FAMILY,
            "task Result",
            &[V1, V2],
            error_code::INVALID_RESULT_PAYLOAD,
            parse_result,
            |value| CommentClassification::Result(Box::new(value)),
        );
    }
    if body.starts_with(ACCEPTANCE_FAMILY) {
        return parse_json_comment(
            body,
            ACCEPTANCE_FAMILY,
            "task Result Acceptance",
            &[V1],
            error_code::INVALID_ACCEPTANCE_PAYLOAD,
            |_, value| parse_acceptance(value),
            |value| CommentClassification::Acceptance(Box::new(value)),
        );
    }
    CommentClassification::Ordinary
}

/// Validate the exact marker/fence/spacing envelope and return the accepted
/// version together with the fenced document body.
fn exact_envelope<'a>(
    body: &'a str,
    family: &str,
    fence: &str,
    kind: &str,
    supported: &[&'static str],
) -> Result<(&'static str, &'a str), WorkGraphError> {
    let version = body
        .split_once('\n')
        .map_or(body, |(first, _)| first)
        .strip_prefix(family)
        .unwrap_or_default();
    let Some(version) = supported
        .iter()
        .copied()
        .find(|candidate| *candidate == version)
    else {
        return Err(WorkGraphError::new(
            error_code::UNSUPPORTED_VERSION,
            format!(
                "unsupported WorkGraph {kind} version '{version}', expected {}",
                supported
                    .iter()
                    .map(|candidate| format!("'{candidate}'"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        ));
    };
    let prefix = format!("{family}{version}\n\n```{fence}\n");
    let Some(content) = body
        .strip_prefix(prefix.as_str())
        .and_then(|body| body.strip_suffix(FENCE_SUFFIX))
    else {
        return Err(WorkGraphError::new(
            error_code::INVALID_ENVELOPE,
            format!("the WorkGraph {kind} marker, fence, spacing, and final LF must be exact"),
        ));
    };
    if content.is_empty() || content.contains("\n```") {
        return Err(WorkGraphError::new(
            error_code::INVALID_ENVELOPE,
            format!("the WorkGraph {kind} must contain exactly one fenced document"),
        ));
    }
    Ok((version, content))
}

fn parse_json_comment<T, P, C>(
    body: &str,
    family: &str,
    kind: &str,
    supported: &[&'static str],
    payload_error: &'static str,
    parse: P,
    classify: C,
) -> CommentClassification
where
    T: Serialize,
    P: FnOnce(&str, serde_json::Value) -> Result<T, String>,
    C: FnOnce(T) -> CommentClassification,
{
    let (version, json_text) = match exact_envelope(body, family, JSON_FENCE, kind, supported) {
        Ok(parts) => parts,
        Err(error) => return CommentClassification::Invalid(error),
    };
    let value: serde_json::Value = match serde_json::from_str(json_text) {
        Ok(value) => value,
        Err(error) => {
            return CommentClassification::Invalid(WorkGraphError::new(
                error_code::INVALID_JSON,
                format!("invalid {kind} JSON: {error}"),
            ))
        }
    };
    if !value.is_object() {
        return CommentClassification::Invalid(WorkGraphError::new(
            error_code::JSON_NOT_OBJECT,
            format!("the {kind} JSON block must contain exactly one object"),
        ));
    }
    match parse(version, value) {
        Ok(parsed) if canonical_json(&parsed, json_text) => classify(parsed),
        Ok(_) => CommentClassification::Invalid(WorkGraphError::new(
            error_code::NON_CANONICAL_JSON,
            format!("the {kind} JSON must use canonical two-space typed formatting"),
        )),
        Err(message) => CommentClassification::Invalid(WorkGraphError::new(payload_error, message)),
    }
}

fn canonical_json<T: Serialize>(value: &T, json_text: &str) -> bool {
    serde_json::to_string_pretty(value).is_ok_and(|canonical| canonical == json_text)
}

fn parse_task(root: TaskRoot) -> Result<TaskDefinition, String> {
    let inputs = match root.task_type {
        TaskType::ValidateIssue => {
            let inputs: ValidateIssueInputs = yaml_typed(root.inputs)?;
            require(
                inputs.validation_profile == "new-issue-default",
                "inputs.validationProfile must equal 'new-issue-default'",
            )?;
            TaskInputs::ValidateIssue(inputs)
        }
        TaskType::RequestInfo => {
            let inputs: RequestInfoInputs = yaml_typed(root.inputs)?;
            non_empty(
                &inputs.validation_result_comment_node_id,
                "inputs.validationResultCommentNodeId",
            )?;
            TaskInputs::RequestInfo(inputs)
        }
    };
    Ok(TaskDefinition {
        task_type: root.task_type,
        inputs,
    })
}

fn parse_assignment(version: &str, value: serde_json::Value) -> Result<AssignmentWire, String> {
    let wire = if version == V2 {
        let v2: AssignmentRootV2 = json_typed(value)?;
        non_empty(&v2.agent_profile, "agentProfile")?;
        opaque_id(&v2.worker_id, "workerId")?;
        AssignmentWire::V2(v2)
    } else {
        let v1: AssignmentRootV1 = json_typed(value)?;
        non_empty(&v1.agent_profile, "agentProfile")?;
        AssignmentWire::V1(v1)
    };
    let agent_profile = match &wire {
        AssignmentWire::V1(v1) => &v1.agent_profile,
        AssignmentWire::V2(v2) => &v2.agent_profile,
    };
    require(
        SUPPORTED_AGENT_PROFILES.contains(&agent_profile.as_str()),
        format!(
            "agentProfile must be one of: {}",
            SUPPORTED_AGENT_PROFILES.join(", ")
        ),
    )?;
    Ok(wire)
}

fn parse_result(version: &str, value: serde_json::Value) -> Result<WorkResult, String> {
    let (task_type, lease_id, outcome, summary, raw_result) = if version == V2 {
        let root: ResultRootV2 = json_typed(value)?;
        opaque_id(&root.lease_id, "leaseId")?;
        (
            root.task_type,
            Some(root.lease_id),
            root.outcome,
            root.summary,
            root.result,
        )
    } else {
        let root: ResultRootV1 = json_typed(value)?;
        (
            root.task_type,
            None,
            root.outcome,
            root.summary,
            root.result,
        )
    };
    non_empty(&summary, "summary")?;
    require(
        !summary.contains(RESULT_FAMILY),
        format!("summary must not contain the {RESULT_FAMILY} marker"),
    )?;
    let result = match task_type {
        TaskType::ValidateIssue => {
            let result: ValidateIssueResult = json_typed(raw_result)?;
            require(
                !result.criteria.is_empty(),
                "result.criteria must contain at least one item",
            )?;
            TaskResult::ValidateIssue(result)
        }
        TaskType::RequestInfo => {
            let result: RequestInfoResult = json_typed(raw_result)?;
            non_empty(
                &result.request_comment_node_id,
                "result.requestCommentNodeId",
            )?;
            TaskResult::RequestInfo(result)
        }
    };
    Ok(WorkResult {
        version: if version == V2 { 2 } else { 1 },
        task_type,
        lease_id,
        outcome,
        summary,
        result,
    })
}

fn parse_lease(value: serde_json::Value) -> Result<TaskLease, String> {
    let lease: TaskLease = json_typed(value)?;
    validate_task_lease(&lease)?;
    Ok(lease)
}

fn parse_lease_expiration(value: serde_json::Value) -> Result<TaskLeaseExpiration, String> {
    let expiration: TaskLeaseExpiration = json_typed(value)?;
    opaque_id(&expiration.lease_comment_node_id, "leaseCommentNodeId")?;
    opaque_id(&expiration.lease_id, "leaseId")?;
    utc_timestamp(&expiration.expired_at, "expiredAt")?;
    non_empty(&expiration.reason, "reason")?;
    require(
        expiration.reason.len() <= MAX_REASON_LEN,
        format!("reason must be at most {MAX_REASON_LEN} characters"),
    )?;
    Ok(expiration)
}

fn parse_acceptance(value: serde_json::Value) -> Result<ResultAcceptance, String> {
    let acceptance: ResultAcceptance = json_typed(value)?;
    non_empty(&acceptance.result_comment_node_id, "resultCommentNodeId")?;
    non_empty(&acceptance.summary, "summary")?;
    require(
        is_sha256_digest(&acceptance.result_body_digest),
        "resultBodyDigest must be 'sha256:' followed by 64 lowercase hexadecimal characters",
    )?;
    Ok(acceptance)
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Accept a non-empty, whitespace-free opaque identifier (a UUID, a GitHub
/// GraphQL node ID, a configured worker ID, or a derived slot ID). The Source
/// never interprets the value, so it only enforces that the identifier is a
/// bounded, exactly-comparable token with no surrounding or embedded
/// whitespace — anything else would make graph joins ambiguous.
fn opaque_id(value: &str, field: &str) -> Result<(), String> {
    require(
        !value.is_empty()
            && value.len() <= MAX_ID_LEN
            && !value.chars().any(char::is_whitespace)
            && !value.chars().any(char::is_control),
        format!(
            "{field} must be a non-empty identifier of at most {MAX_ID_LEN} characters with no \
             whitespace or control characters"
        ),
    )
}

/// Accept only the canonical second-precision RFC 3339 UTC instant
/// `YYYY-MM-DDTHH:MM:SSZ`.
///
/// The form is fixed rather than merely parseable: every projected instant has
/// to be exactly comparable as a string as well as a timestamp, and a local
/// offset, a space separator, a lowercase `t`/`z`, or a fractional part would
/// all make two spellings of one instant compare unequal in a query.
fn utc_timestamp(value: &str, field: &str) -> Result<DateTime<Utc>, String> {
    let invalid = || {
        format!(
            "{field} must be a canonical UTC timestamp of the exact form \
             'YYYY-MM-DDTHH:MM:SSZ', for example '2026-08-19T22:00:00Z'"
        )
    };
    let shape_ok = value.len() == 20
        && value.ends_with('Z')
        && value.as_bytes()[10] == b'T'
        && [4, 7].iter().all(|index| value.as_bytes()[*index] == b'-')
        && [13, 16]
            .iter()
            .all(|index| value.as_bytes()[*index] == b':')
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .iter()
            .all(|index| value.as_bytes()[*index].is_ascii_digit());
    require(shape_ok, invalid())?;
    let parsed = DateTime::parse_from_rfc3339(value).map_err(|_| invalid())?;
    Ok(parsed.with_timezone(&Utc))
}

fn yaml_typed<T: serde::de::DeserializeOwned>(value: serde_yaml::Value) -> Result<T, String> {
    serde_yaml::from_value(value).map_err(|error| error.to_string())
}

fn json_typed<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn require(ok: bool, message: impl Into<String>) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn non_empty(value: &str, field: &str) -> Result<(), String> {
    require(
        !value.trim().is_empty(),
        format!("{field} must be a non-empty string"),
    )
}

pub fn comment_error_element_id(comment_node_id: &str) -> String {
    format!("workgraph-error:comment:{comment_node_id}")
}

pub fn task_error_element_id(task_node_id: &str) -> String {
    format!("workgraph-error:task:{task_node_id}")
}

pub fn status_error_element_id(subject_node_id: &str) -> String {
    format!("workgraph-error:status:{subject_node_id}")
}

/// The single stable element ID of the worker-configuration error node. There
/// is exactly one configured worker file per Source, so a malformed or missing
/// file always converges onto the same node instead of accumulating history.
pub fn worker_config_error_element_id() -> String {
    "workgraph-error:worker-config".to_string()
}

/// Stable element ID of a configured worker node.
pub fn worker_element_id(worker_id: &str) -> String {
    format!("workgraph-worker:{worker_id}")
}

/// Deterministic `slotId` for the one-based `slot_number` of `worker_id`.
pub fn slot_id(worker_id: &str, slot_number: u32) -> String {
    format!("{worker_id}/{slot_number}")
}

/// Stable element ID of a worker slot node, derived from its `slotId`.
pub fn worker_slot_element_id(slot_id: &str) -> String {
    format!("workgraph-worker-slot:{slot_id}")
}

/// Stable element ID of the derived lease identity node.
///
/// The `WorkGraphTaskLease` node itself is keyed by its own comment node ID, so
/// a `WorkGraphTaskResult/v2` — which carries only `leaseId` — cannot address
/// it. This node is the addressable lease identity that later lifecycle
/// artifacts bind to.
///
/// The key is scoped by the task the lease belongs to, not by `leaseId` alone.
/// `leaseId` is attacker-controlled free text from a comment body, while the
/// task node ID is the GitHub-assigned Issue the comment was written on. Two
/// comments can therefore only reach the same identity when they are on the
/// same task, which makes "the named task agrees" a structural property rather
/// than something a query has to re-derive. GitHub node IDs never contain a
/// colon, so the first separator always terminates the task ID and no pair of
/// distinct tasks can collide.
pub fn lease_anchor_element_id(task_node_id: &str, lease_id: &str) -> String {
    format!("workgraph-lease:{task_node_id}:{lease_id}")
}
