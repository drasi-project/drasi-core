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

const RESULT_FAMILY: &str = "WorkGraphTaskResult/";
const RESULT_MARKER: &str = "WorkGraphTaskResult/v1";
const RESULT_PREFIX: &str = "WorkGraphTaskResult/v1\n\n```json\n";
const RESULT_SUFFIX: &str = "\n```\n";
const SUPPORTED_VERSION: &str = "v1";

pub mod error_code {
    pub const UNSUPPORTED_VERSION: &str = "unsupported-workgraph-version";
    pub const INVALID_ENVELOPE: &str = "invalid-envelope";
    pub const INVALID_JSON: &str = "invalid-json";
    pub const JSON_NOT_OBJECT: &str = "json-not-object";
    pub const NON_CANONICAL_JSON: &str = "non-canonical-json";
    pub const INVALID_ASSIGNMENT_PAYLOAD: &str = "invalid-assignment-payload";
    pub const INVALID_RESULT_PAYLOAD: &str = "invalid-result-payload";
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
        IssueValidation = "issue-validation",
        IssueRiskProfile = "issue-risk-profile",
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
    struct IssueValidationTask { validation_profile: String }
    struct IssueRiskProfileTask { risk_profile: String, dimensions: Vec<String> }
    struct IssueValidationResult { criteria: Vec<CriterionResult> }
    struct CriterionResult { criterion: String, passed: bool, evidence: String }
    struct IssueRiskProfileResult { dimensions: Vec<DimensionResult> }
    struct DimensionResult { dimension: String, score: i64, rationale: String }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum AssignmentTask {
    IssueValidation(IssueValidationTask),
    IssueRiskProfile(IssueRiskProfileTask),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TaskResult {
    IssueValidation(IssueValidationResult),
    IssueRiskProfile(IssueRiskProfileResult),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Assignment {
    pub assignment_id: String,
    pub agent_profile: String,
    pub priority: i64,
    pub task_type: TaskType,
    pub task: AssignmentTask,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkResult {
    pub assignment_id: String,
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
    Task(Box<Assignment>),
    Invalid(WorkGraphError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultClassification {
    Ordinary,
    Result(Box<WorkResult>),
    Invalid(WorkGraphError),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AssignmentRoot {
    assignment_id: String,
    agent_profile: String,
    priority: i64,
    task_type: TaskType,
    task: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResultRoot {
    assignment_id: String,
    task_type: TaskType,
    outcome: Outcome,
    summary: String,
    result: serde_json::Value,
}

pub fn classify_task_body(body: &str) -> TaskClassification {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(error) => {
            return TaskClassification::Invalid(WorkGraphError::new(
                error_code::INVALID_JSON,
                format!("task body is not valid JSON: {error}"),
            ))
        }
    };
    if !value.is_object() {
        return TaskClassification::Invalid(WorkGraphError::new(
            error_code::JSON_NOT_OBJECT,
            "task body must contain exactly one JSON object",
        ));
    }
    match parse_assignment(value) {
        Ok(assignment) if canonical_json(&assignment, body) => {
            TaskClassification::Task(Box::new(assignment))
        }
        Ok(_) => TaskClassification::Invalid(WorkGraphError::new(
            error_code::NON_CANONICAL_JSON,
            "task body must use canonical two-space typed JSON with no surrounding content",
        )),
        Err(message) => TaskClassification::Invalid(WorkGraphError::new(
            error_code::INVALID_ASSIGNMENT_PAYLOAD,
            message,
        )),
    }
}

pub fn classify_result(body: &str) -> ResultClassification {
    if !body.starts_with(RESULT_FAMILY) {
        return ResultClassification::Ordinary;
    }
    let version = body
        .split_once('\n')
        .map_or(body, |(first, _)| first)
        .strip_prefix(RESULT_FAMILY)
        .unwrap_or_default();
    if version != SUPPORTED_VERSION {
        return ResultClassification::Invalid(WorkGraphError::new(
            error_code::UNSUPPORTED_VERSION,
            format!("unsupported WorkGraph task Result version '{version}', expected 'v1'"),
        ));
    }
    let Some(json_text) = body
        .strip_prefix(RESULT_PREFIX)
        .and_then(|body| body.strip_suffix(RESULT_SUFFIX))
    else {
        return ResultClassification::Invalid(WorkGraphError::new(
            error_code::INVALID_ENVELOPE,
            "the WorkGraph task Result marker, fence, spacing, and final LF must be exact",
        ));
    };
    if json_text.is_empty() || json_text.contains("\n```") {
        return ResultClassification::Invalid(WorkGraphError::new(
            error_code::INVALID_ENVELOPE,
            "the WorkGraph task Result must contain exactly one fenced JSON object",
        ));
    }
    let value: serde_json::Value = match serde_json::from_str(json_text) {
        Ok(value) => value,
        Err(error) => {
            return ResultClassification::Invalid(WorkGraphError::new(
                error_code::INVALID_JSON,
                format!("invalid Result JSON: {error}"),
            ))
        }
    };
    if !value.is_object() {
        return ResultClassification::Invalid(WorkGraphError::new(
            error_code::JSON_NOT_OBJECT,
            "the Result JSON block must contain exactly one JSON object",
        ));
    }
    match parse_result(value) {
        Ok(result) if canonical_json(&result, json_text) => {
            ResultClassification::Result(Box::new(result))
        }
        Ok(_) => ResultClassification::Invalid(WorkGraphError::new(
            error_code::NON_CANONICAL_JSON,
            "the Result JSON must use canonical two-space typed formatting",
        )),
        Err(message) => ResultClassification::Invalid(WorkGraphError::new(
            error_code::INVALID_RESULT_PAYLOAD,
            message,
        )),
    }
}

fn canonical_json<T: Serialize>(value: &T, json_text: &str) -> bool {
    serde_json::to_string_pretty(value).is_ok_and(|canonical| canonical == json_text)
}

fn parse_assignment(value: serde_json::Value) -> Result<Assignment, String> {
    let root: AssignmentRoot = typed(value)?;
    non_empty(&root.assignment_id, "assignmentId")?;
    non_empty(&root.agent_profile, "agentProfile")?;
    require(root.priority >= 0, "priority must be an integer >= 0")?;
    let task = match root.task_type {
        TaskType::IssueValidation => {
            let task: IssueValidationTask = typed(root.task)?;
            non_empty(&task.validation_profile, "task.validationProfile")?;
            AssignmentTask::IssueValidation(task)
        }
        TaskType::IssueRiskProfile => {
            let task: IssueRiskProfileTask = typed(root.task)?;
            non_empty(&task.risk_profile, "task.riskProfile")?;
            non_empty_strings(&task.dimensions, "task.dimensions")?;
            AssignmentTask::IssueRiskProfile(task)
        }
    };
    Ok(Assignment {
        assignment_id: root.assignment_id,
        agent_profile: root.agent_profile,
        priority: root.priority,
        task_type: root.task_type,
        task,
    })
}

fn parse_result(value: serde_json::Value) -> Result<WorkResult, String> {
    let root: ResultRoot = typed(value)?;
    non_empty(&root.assignment_id, "assignmentId")?;
    non_empty(&root.summary, "summary")?;
    require(
        !root.summary.contains(RESULT_MARKER),
        "summary must not contain the WorkGraphTaskResult/v1 marker",
    )?;
    let result = match root.task_type {
        TaskType::IssueValidation => {
            let result: IssueValidationResult = typed(root.result)?;
            require(
                !result.criteria.is_empty(),
                "result.criteria must contain at least one item",
            )?;
            TaskResult::IssueValidation(result)
        }
        TaskType::IssueRiskProfile => {
            let result: IssueRiskProfileResult = typed(root.result)?;
            require(
                !result.dimensions.is_empty(),
                "result.dimensions must contain at least one item",
            )?;
            require(
                result
                    .dimensions
                    .iter()
                    .all(|dimension| (0..=100).contains(&dimension.score)),
                "result.dimensions[].score must be between 0 and 100",
            )?;
            TaskResult::IssueRiskProfile(result)
        }
    };
    Ok(WorkResult {
        assignment_id: root.assignment_id,
        task_type: root.task_type,
        outcome: root.outcome,
        summary: root.summary,
        result,
    })
}

fn typed<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, String> {
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

fn non_empty_strings(values: &[String], field: &str) -> Result<(), String> {
    require(
        !values.is_empty(),
        format!("{field} must contain at least one item"),
    )?;
    require(
        values.iter().all(|value| !value.trim().is_empty()),
        format!("{field} entries must be non-empty strings"),
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
