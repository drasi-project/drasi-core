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

const ASSIGNMENT_FAMILY: &str = "WorkGraphAssignment/";
const RESULT_FAMILY: &str = "WorkGraphResult/";
const SUPPORTED_VERSION: &str = "v1";
const FENCE_OPEN: &str = "```json";
const FENCE_CLOSE: &str = "```";

pub mod error_code {
    pub const UNSUPPORTED_VERSION: &str = "unsupported-workgraph-version";
    pub const MISSING_HUMAN_SUMMARY: &str = "missing-human-summary";
    pub const MISSING_JSON_BLOCK: &str = "missing-json-block";
    pub const MULTIPLE_JSON_BLOCKS: &str = "multiple-json-blocks";
    pub const UNTERMINATED_JSON_BLOCK: &str = "unterminated-json-block";
    pub const UNEXPECTED_TRAILING_CONTENT: &str = "unexpected-trailing-content";
    pub const INVALID_JSON: &str = "invalid-json";
    pub const JSON_NOT_OBJECT: &str = "json-not-object";
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
    struct IssueValidationTask { validation_profile: String, criteria: Vec<String> }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub assignment_id: String,
    pub agent_profile: String,
    pub priority: i64,
    pub task_type: TaskType,
    pub task: AssignmentTask,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkResult {
    pub assignment_id: String,
    pub task_type: TaskType,
    pub outcome: Outcome,
    pub summary: String,
    pub result: TaskResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeError {
    pub code: &'static str,
    pub message: String,
}

impl EnvelopeError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn invalid(code: &'static str, message: impl Into<String>) -> Classification {
    Classification::Invalid(EnvelopeError::new(code, message))
}

fn envelope_err<T>(code: &'static str, message: impl Into<String>) -> Result<T, EnvelopeError> {
    Err(EnvelopeError::new(code, message))
}

fn require(ok: bool, message: impl Into<String>) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn non_empty(value: &str, field: &str) -> Result<(), String> {
    let filled = !value.trim().is_empty();
    require(filled, format!("{field} must be a non-empty string"))
}

fn typed<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    Ordinary,
    Assignment(Box<Assignment>),
    Result(Box<WorkResult>),
    Invalid(EnvelopeError),
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

pub fn classify(body: &str) -> Classification {
    let normalized = body.replace("\r\n", "\n");
    let mut lines = normalized.split('\n');
    let Some(marker) = lines.next() else {
        return Classification::Ordinary;
    };
    let (is_assignment, version) = if let Some(v) = marker.strip_prefix(ASSIGNMENT_FAMILY) {
        (true, v)
    } else if let Some(v) = marker.strip_prefix(RESULT_FAMILY) {
        (false, v)
    } else {
        return Classification::Ordinary;
    };
    if version != SUPPORTED_VERSION {
        let message = format!("unsupported WorkGraph version '{version}', expected 'v1'");
        return invalid(error_code::UNSUPPORTED_VERSION, message);
    }
    let rest: Vec<&str> = lines.collect();
    let (summary, json_text) = match split_envelope(&rest) {
        Ok(parts) => parts,
        Err(err) => return Classification::Invalid(err),
    };
    if summary.trim().is_empty() {
        let message = "a non-empty human summary is required between the marker and the JSON";
        return invalid(error_code::MISSING_HUMAN_SUMMARY, message);
    }
    let value: serde_json::Value = match serde_json::from_str(&json_text) {
        Ok(value) => value,
        Err(e) => return invalid(error_code::INVALID_JSON, format!("invalid JSON: {e}")),
    };
    if !value.is_object() {
        let message = "the fenced JSON block must contain exactly one JSON object";
        return invalid(error_code::JSON_NOT_OBJECT, message);
    }
    let parsed = if is_assignment {
        parse_assignment(value)
            .map(|a| Classification::Assignment(Box::new(a)))
            .map_err(|m| (error_code::INVALID_ASSIGNMENT_PAYLOAD, m))
    } else {
        parse_result(value)
            .map(|r| Classification::Result(Box::new(r)))
            .map_err(|m| (error_code::INVALID_RESULT_PAYLOAD, m))
    };
    parsed.unwrap_or_else(|(code, m)| Classification::Invalid(EnvelopeError::new(code, m)))
}

fn split_envelope(lines: &[&str]) -> Result<(String, String), EnvelopeError> {
    let missing = "exactly one fenced ```json block is required";
    let unterminated = "the ```json block is not terminated by a ``` line";
    let is_fence = |line: &&str| line.starts_with(FENCE_CLOSE);
    let Some(open) = lines.iter().position(is_fence) else {
        return envelope_err(error_code::MISSING_JSON_BLOCK, missing);
    };
    if lines[open] != FENCE_OPEN {
        return envelope_err(error_code::MISSING_JSON_BLOCK, missing);
    }
    let Some(close) = lines[open + 1..]
        .iter()
        .position(|line| *line == FENCE_CLOSE)
        .map(|offset| open + 1 + offset)
    else {
        return envelope_err(error_code::UNTERMINATED_JSON_BLOCK, unterminated);
    };
    let tail = &lines[close + 1..];
    if tail.iter().any(is_fence) {
        let message = "only one fenced block is allowed in a WorkGraph comment";
        return envelope_err(error_code::MULTIPLE_JSON_BLOCKS, message);
    }
    if tail.iter().any(|line| !line.trim().is_empty()) {
        let message = "only whitespace is allowed after the closing JSON fence";
        return envelope_err(error_code::UNEXPECTED_TRAILING_CONTENT, message);
    }
    Ok((lines[..open].join("\n"), lines[open + 1..close].join("\n")))
}

fn parse_assignment(value: serde_json::Value) -> Result<Assignment, String> {
    let root: AssignmentRoot = typed(value)?;
    non_empty(&root.assignment_id, "assignmentId")?;
    non_empty(&root.agent_profile, "agentProfile")?;
    require(root.priority >= 0, "priority must be an integer >= 0")?;
    let task = match root.task_type {
        TaskType::IssueValidation => {
            let task: IssueValidationTask = typed(root.task)?;
            non_empty_strings(&task.criteria, "task.criteria")?;
            AssignmentTask::IssueValidation(task)
        }
        TaskType::IssueRiskProfile => {
            let task: IssueRiskProfileTask = typed(root.task)?;
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
    let result = match root.task_type {
        TaskType::IssueValidation => {
            let result: IssueValidationResult = typed(root.result)?;
            let ok = !result.criteria.is_empty();
            require(ok, "result.criteria must contain at least one item")?;
            TaskResult::IssueValidation(result)
        }
        TaskType::IssueRiskProfile => {
            let result: IssueRiskProfileResult = typed(root.result)?;
            let ok = !result.dimensions.is_empty();
            require(ok, "result.dimensions must contain at least one item")?;
            let scored = result
                .dimensions
                .iter()
                .all(|d| (0..=100).contains(&d.score));
            require(
                scored,
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

fn non_empty_strings(values: &[String], field: &str) -> Result<(), String> {
    let some = !values.is_empty();
    require(some, format!("{field} must contain at least one item"))?;
    let filled = values.iter().all(|value| !value.trim().is_empty());
    require(filled, format!("{field} entries must be non-empty strings"))
}

pub fn encode_id_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '~') {
            encoded.push(ch);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub fn assignment_element_id(organization_node_id: &str, assignment_id: &str) -> String {
    format!(
        "workgraph-assignment:{organization_node_id}:{}",
        encode_id_component(assignment_id)
    )
}

pub fn comment_error_element_id(comment_node_id: &str) -> String {
    format!("workgraph-error:comment:{comment_node_id}")
}

pub fn status_error_element_id(subject_node_id: &str) -> String {
    format!("workgraph-error:status:{subject_node_id}")
}
