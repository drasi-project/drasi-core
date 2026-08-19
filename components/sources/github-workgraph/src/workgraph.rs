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

use serde::{Deserialize, Serialize};

const TASK_FAMILY: &str = "WorkGraphTask/";
const TASK_PREFIX: &str = "WorkGraphTask/v1\n\n```yaml\n";
const ASSIGNMENT_FAMILY: &str = "WorkGraphTaskAssignment/";
const ASSIGNMENT_PREFIX: &str = "WorkGraphTaskAssignment/v1\n\n```json\n";
const RESULT_FAMILY: &str = "WorkGraphTaskResult/";
const RESULT_MARKER: &str = "WorkGraphTaskResult/v1";
const RESULT_PREFIX: &str = "WorkGraphTaskResult/v1\n\n```json\n";
const ACCEPTANCE_FAMILY: &str = "WorkGraphTaskResultAcceptance/";
const ACCEPTANCE_PREFIX: &str = "WorkGraphTaskResultAcceptance/v1\n\n```json\n";
const YAML_SUFFIX: &str = "\n```\n";
const JSON_SUFFIX: &str = "\n```\n";
const SUPPORTED_VERSION: &str = "v1";
const SUPPORTED_AGENT_PROFILES: &[&str] = &["issue-validator", "issue-info-requester"];

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
    struct TaskAssignment { agent_profile: String }
    struct ResultAcceptance {
        result_comment_node_id: String,
        result_body_digest: String,
        summary: String
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkResult {
    pub task_type: TaskType,
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
    fn new(code: &'static str, message: impl Into<String>) -> Self {
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
struct ResultRoot {
    task_type: TaskType,
    outcome: Outcome,
    summary: String,
    result: serde_json::Value,
}

pub fn classify_task_body(body: &str) -> TaskClassification {
    let yaml_text = match exact_envelope(body, TASK_FAMILY, TASK_PREFIX, YAML_SUFFIX, "task") {
        Ok(text) => text,
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
    if body.starts_with(ASSIGNMENT_FAMILY) {
        return parse_json_comment(
            body,
            ASSIGNMENT_FAMILY,
            ASSIGNMENT_PREFIX,
            "task Assignment",
            error_code::INVALID_ASSIGNMENT_PAYLOAD,
            parse_assignment,
            |value| CommentClassification::Assignment(Box::new(value)),
        );
    }
    if body.starts_with(RESULT_FAMILY) {
        return parse_json_comment(
            body,
            RESULT_FAMILY,
            RESULT_PREFIX,
            "task Result",
            error_code::INVALID_RESULT_PAYLOAD,
            parse_result,
            |value| CommentClassification::Result(Box::new(value)),
        );
    }
    if body.starts_with(ACCEPTANCE_FAMILY) {
        return parse_json_comment(
            body,
            ACCEPTANCE_FAMILY,
            ACCEPTANCE_PREFIX,
            "task Result Acceptance",
            error_code::INVALID_ACCEPTANCE_PAYLOAD,
            parse_acceptance,
            |value| CommentClassification::Acceptance(Box::new(value)),
        );
    }
    CommentClassification::Ordinary
}

fn exact_envelope<'a>(
    body: &'a str,
    family: &str,
    prefix: &str,
    suffix: &str,
    kind: &str,
) -> Result<&'a str, WorkGraphError> {
    let version = body
        .split_once('\n')
        .map_or(body, |(first, _)| first)
        .strip_prefix(family)
        .unwrap_or_default();
    if version != SUPPORTED_VERSION {
        return Err(WorkGraphError::new(
            error_code::UNSUPPORTED_VERSION,
            format!("unsupported WorkGraph {kind} version '{version}', expected 'v1'"),
        ));
    }
    let Some(content) = body
        .strip_prefix(prefix)
        .and_then(|body| body.strip_suffix(suffix))
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
    Ok(content)
}

fn parse_json_comment<T, P, C>(
    body: &str,
    family: &str,
    prefix: &str,
    kind: &str,
    payload_error: &'static str,
    parse: P,
    classify: C,
) -> CommentClassification
where
    T: Serialize,
    P: FnOnce(serde_json::Value) -> Result<T, String>,
    C: FnOnce(T) -> CommentClassification,
{
    let json_text = match exact_envelope(body, family, prefix, JSON_SUFFIX, kind) {
        Ok(text) => text,
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
    match parse(value) {
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

fn parse_assignment(value: serde_json::Value) -> Result<TaskAssignment, String> {
    let assignment: TaskAssignment = json_typed(value)?;
    non_empty(&assignment.agent_profile, "agentProfile")?;
    require(
        SUPPORTED_AGENT_PROFILES.contains(&assignment.agent_profile.as_str()),
        format!(
            "agentProfile must be one of: {}",
            SUPPORTED_AGENT_PROFILES.join(", ")
        ),
    )?;
    Ok(assignment)
}

fn parse_result(value: serde_json::Value) -> Result<WorkResult, String> {
    let root: ResultRoot = json_typed(value)?;
    non_empty(&root.summary, "summary")?;
    require(
        !root.summary.contains(RESULT_MARKER),
        "summary must not contain the WorkGraphTaskResult/v1 marker",
    )?;
    let result = match root.task_type {
        TaskType::ValidateIssue => {
            let result: ValidateIssueResult = json_typed(root.result)?;
            require(
                !result.criteria.is_empty(),
                "result.criteria must contain at least one item",
            )?;
            TaskResult::ValidateIssue(result)
        }
        TaskType::RequestInfo => {
            let result: RequestInfoResult = json_typed(root.result)?;
            non_empty(
                &result.request_comment_node_id,
                "result.requestCommentNodeId",
            )?;
            TaskResult::RequestInfo(result)
        }
    };
    Ok(WorkResult {
        task_type: root.task_type,
        outcome: root.outcome,
        summary: root.summary,
        result,
    })
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
