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

use crate::agents::validate_agent_id;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const TASK_FAMILY: &str = "WorkGraphTask/";
const ASSIGNMENT_FAMILY: &str = "WorkGraphTaskAssignment/";
const RESULT_FAMILY: &str = "WorkGraphTaskResult/";
const FEEDBACK_FAMILY: &str = "WorkGraphTaskFeedback/";
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
    pub const INVALID_FEEDBACK_PAYLOAD: &str = "invalid-feedback-payload";
    pub const INVALID_ACCEPTANCE_PAYLOAD: &str = "invalid-acceptance-payload";
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
        WorkflowTask = "workflow-task",
    }
}

wire_enum! {
    Outcome { Succeeded = "succeeded", Failed = "failed", Blocked = "blocked" }
}

wire_enum! {
    WorkflowJoin { All = "all" }
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
    struct TaskFeedback {
        result_comment_node_id: String,
        result_body_digest: String,
        feedback: String
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowChildDefinition {
    pub branch_id: String,
    pub operation: String,
    pub agent: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowTaskInputs {
    pub workflow_id: String,
    pub workflow_run_id: String,
    pub step_id: String,
    pub definition_commit: String,
    pub definition_digest: String,
    pub generation: u64,
    pub operation: String,
    pub agent: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join: Option<WorkflowJoin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_child_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<WorkflowChildDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssignmentRootV1 {
    agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskAssignment {
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TaskInputs {
    ValidateIssue(ValidateIssueInputs),
    RequestInfo(RequestInfoInputs),
    WorkflowTask(WorkflowTaskInputs),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TaskResult {
    ValidateIssue(ValidateIssueResult),
    RequestInfo(RequestInfoResult),
    WorkflowTask(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDefinition {
    pub task_type: TaskType,
    pub inputs: TaskInputs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkResult {
    pub task_type: TaskType,
    pub lease_id: String,
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
    Feedback(Box<TaskFeedback>),
    Acceptance(Box<ResultAcceptance>),
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
    lease_id: String,
    outcome: Outcome,
    summary: String,
    result: serde_json::Value,
}

pub fn classify_task_body(body: &str) -> TaskClassification {
    let (version, yaml_text) =
        match exact_envelope(body, TASK_FAMILY, YAML_FENCE, "task", &[V1, V2]) {
            Ok(envelope) => envelope,
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
    match parse_task(version, root) {
        Ok(task) => TaskClassification::Task(Box::new(task)),
        Err(message) => TaskClassification::Invalid(WorkGraphError::new(
            error_code::INVALID_TASK_PAYLOAD,
            message,
        )),
    }
}

pub fn classify_comment(body: &str) -> CommentClassification {
    if body.starts_with(ASSIGNMENT_FAMILY) {
        return parse_json_comment(
            body,
            ASSIGNMENT_FAMILY,
            "task Assignment",
            &[V1],
            error_code::INVALID_ASSIGNMENT_PAYLOAD,
            parse_assignment,
            |wire| {
                CommentClassification::Assignment(Box::new(TaskAssignment {
                    agent_id: wire.agent_id,
                }))
            },
        );
    }
    if body.starts_with(RESULT_FAMILY) {
        return parse_json_comment(
            body,
            RESULT_FAMILY,
            "task Result",
            &[V1],
            error_code::INVALID_RESULT_PAYLOAD,
            parse_result,
            |value| CommentClassification::Result(Box::new(value)),
        );
    }
    if body.starts_with(FEEDBACK_FAMILY) {
        return parse_json_comment(
            body,
            FEEDBACK_FAMILY,
            "task Feedback",
            &[V1],
            error_code::INVALID_FEEDBACK_PAYLOAD,
            |_, value| parse_feedback(value),
            |value| CommentClassification::Feedback(Box::new(value)),
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

fn parse_task(version: &str, root: TaskRoot) -> Result<TaskDefinition, String> {
    let inputs = match (version, root.task_type) {
        (V1, TaskType::ValidateIssue) => {
            let inputs: ValidateIssueInputs = yaml_typed(root.inputs)?;
            require(
                inputs.validation_profile == "new-issue-default",
                "inputs.validationProfile must equal 'new-issue-default'",
            )?;
            TaskInputs::ValidateIssue(inputs)
        }
        (V1, TaskType::RequestInfo) => {
            let inputs: RequestInfoInputs = yaml_typed(root.inputs)?;
            non_empty(
                &inputs.validation_result_comment_node_id,
                "inputs.validationResultCommentNodeId",
            )?;
            TaskInputs::RequestInfo(inputs)
        }
        (V2, TaskType::WorkflowTask) => {
            let inputs: WorkflowTaskInputs = yaml_typed(root.inputs)?;
            validate_workflow_task_inputs(&inputs)?;
            TaskInputs::WorkflowTask(inputs)
        }
        (V1, TaskType::WorkflowTask) => {
            return Err("taskType 'workflow-task' requires WorkGraphTask/v2".to_string())
        }
        (V2, _) => return Err("WorkGraphTask/v2 requires taskType 'workflow-task'".to_string()),
        _ => return Err(format!("unsupported task version '{version}'")),
    };
    Ok(TaskDefinition {
        task_type: root.task_type,
        inputs,
    })
}

fn parse_assignment(_: &str, value: serde_json::Value) -> Result<AssignmentRootV1, String> {
    let wire: AssignmentRootV1 = json_typed(value)?;
    validate_agent_id(&wire.agent_id, "agentId")?;
    Ok(wire)
}

fn parse_result(version: &str, value: serde_json::Value) -> Result<WorkResult, String> {
    debug_assert_eq!(version, V1);
    let root: ResultRootV1 = json_typed(value)?;
    opaque_id(&root.lease_id, "leaseId")?;
    let (task_type, lease_id, outcome, summary, raw_result) = (
        root.task_type,
        root.lease_id,
        root.outcome,
        root.summary,
        root.result,
    );
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
        TaskType::WorkflowTask => {
            require(
                raw_result.is_object(),
                "result for taskType 'workflow-task' must be an object",
            )?;
            TaskResult::WorkflowTask(raw_result)
        }
    };
    Ok(WorkResult {
        task_type,
        lease_id,
        outcome,
        summary,
        result,
    })
}

fn validate_workflow_task_inputs(inputs: &WorkflowTaskInputs) -> Result<(), String> {
    validate_agent_id(&inputs.workflow_id, "inputs.workflowId")?;
    opaque_id(&inputs.workflow_run_id, "inputs.workflowRunId")?;
    validate_agent_id(&inputs.step_id, "inputs.stepId")?;
    opaque_id(&inputs.definition_commit, "inputs.definitionCommit")?;
    require(
        is_sha256_digest(&inputs.definition_digest),
        "inputs.definitionDigest must be 'sha256:' followed by 64 lowercase hexadecimal characters",
    )?;
    require(
        inputs.generation > 0,
        "inputs.generation must be greater than zero",
    )?;
    validate_agent_id(&inputs.operation, "inputs.operation")?;
    validate_agent_id(&inputs.agent, "inputs.agent")?;
    if let Some(branch_id) = &inputs.branch_id {
        validate_agent_id(branch_id, "inputs.branchId")?;
    }

    match (
        inputs.join,
        inputs.expected_child_count,
        inputs.children.is_empty(),
    ) {
        (None, None, true) => {}
        (Some(WorkflowJoin::All), Some(expected), false) => {
            require(
                inputs.branch_id.is_none(),
                "composite inputs.branchId must be absent",
            )?;
            require(
                expected >= 2,
                "inputs.expectedChildCount must be at least two for join 'all'",
            )?;
            require(
                usize::try_from(expected).is_ok_and(|value| value == inputs.children.len()),
                "inputs.expectedChildCount must equal the number of children",
            )?;
            let mut branch_ids = BTreeSet::new();
            let mut agents = BTreeSet::new();
            for child in &inputs.children {
                validate_agent_id(&child.branch_id, "inputs.children[].branchId")?;
                validate_agent_id(&child.operation, "inputs.children[].operation")?;
                validate_agent_id(&child.agent, "inputs.children[].agent")?;
                require(
                    branch_ids.insert(&child.branch_id),
                    "inputs.children branchId values must be unique",
                )?;
                require(
                    agents.insert(&child.agent),
                    "inputs.children agent values must be unique",
                )?;
            }
            require(
                !agents.contains(&inputs.agent),
                "composite inputs.agent must differ from every child agent",
            )?;
        }
        _ => {
            return Err(
                "inputs.join, inputs.expectedChildCount, and inputs.children must either all \
                 describe one composite task or all be absent"
                    .to_string(),
            )
        }
    }
    Ok(())
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

fn parse_feedback(value: serde_json::Value) -> Result<TaskFeedback, String> {
    let feedback: TaskFeedback = json_typed(value)?;
    non_empty(&feedback.result_comment_node_id, "resultCommentNodeId")?;
    require(
        is_sha256_digest(&feedback.result_body_digest),
        "resultBodyDigest must be 'sha256:' followed by 64 lowercase hexadecimal characters",
    )?;
    non_empty(&feedback.feedback, "feedback")?;
    Ok(feedback)
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Accept a non-empty, whitespace-free opaque identifier (a UUID, a GitHub
/// GraphQL node ID or a derived Lease identity). The Source
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

/// The single stable element ID of the agent-configuration error node. There
/// is exactly one configured agent file per Source, so a malformed or missing
/// file always converges onto the same node instead of accumulating history.
pub fn agent_config_error_element_id() -> String {
    "workgraph-error:agent-config".to_string()
}

/// Stable element ID of a configured agent node.
pub fn agent_element_id(agent_id: &str) -> String {
    format!("workgraph-agent:{agent_id}")
}

/// Deterministic `slotId` for the one-based `slot_number` of `agent_id`.
pub fn slot_id(agent_id: &str, slot_number: u32) -> String {
    format!("{agent_id}/{slot_number}")
}

/// Stable element ID of an agent slot node, derived from its `slotId`.
pub fn agent_slot_element_id(slot_id: &str) -> String {
    format!("workgraph-agent-slot:{slot_id}")
}

pub fn lease_element_id(task_node_id: &str, lease_id: &str) -> String {
    format!("workgraph-lease:{task_node_id}:{lease_id}")
}
