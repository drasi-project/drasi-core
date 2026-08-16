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
const DETAILS_OPEN: &str = "<details>";
const DETAILS_CLOSE: &str = "</details>";
const ASSIGNMENT_SUMMARY: &str = "<summary>WorkGraph Assignment</summary>";
const RESULT_SUMMARY: &str = "<summary>WorkGraph Result</summary>";
const ASSIGNMENT_MARKER: &str = "WorkGraphAssignment/v1";
const RESULT_MARKER: &str = "WorkGraphResult/v1";
const FENCE_OPEN: &str = "```json";
const FENCE_CLOSE: &str = "```";

pub mod error_code {
    pub const INVALID_ENVELOPE: &str = "invalid-envelope";
    pub const UNSUPPORTED_VERSION: &str = "unsupported-workgraph-version";
    pub const MISSING_HUMAN_SUMMARY: &str = "missing-human-summary";
    pub const MISSING_JSON_BLOCK: &str = "missing-json-block";
    pub const MULTIPLE_JSON_BLOCKS: &str = "multiple-json-blocks";
    pub const UNTERMINATED_JSON_BLOCK: &str = "unterminated-json-block";
    pub const UNEXPECTED_TRAILING_CONTENT: &str = "unexpected-trailing-content";
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvelopeKind {
    Assignment,
    Result,
}

impl EnvelopeKind {
    fn family(self) -> &'static str {
        match self {
            Self::Assignment => ASSIGNMENT_FAMILY,
            Self::Result => RESULT_FAMILY,
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Assignment => ASSIGNMENT_MARKER,
            Self::Result => RESULT_MARKER,
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Self::Assignment => ASSIGNMENT_SUMMARY,
            Self::Result => RESULT_SUMMARY,
        }
    }
}

pub fn classify(body: &str) -> Classification {
    let Some(kind) = candidate_kind(body) else {
        return Classification::Ordinary;
    };

    let kind = match kind {
        Ok(kind) => kind,
        Err(error) => return Classification::Invalid(error),
    };
    let (summary, json_text) = match split_envelope(body, kind) {
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

    match kind {
        EnvelopeKind::Assignment => match parse_assignment(value) {
            Ok(assignment) if canonical_json(&assignment, &json_text) => {
                Classification::Assignment(Box::new(assignment))
            }
            Ok(_) => invalid(
                error_code::NON_CANONICAL_JSON,
                "the Assignment JSON must use the canonical two-space typed formatting",
            ),
            Err(message) => invalid(error_code::INVALID_ASSIGNMENT_PAYLOAD, message),
        },
        EnvelopeKind::Result => match parse_result(value) {
            Ok(result) if canonical_json(&result, &json_text) => {
                Classification::Result(Box::new(result))
            }
            Ok(_) => invalid(
                error_code::NON_CANONICAL_JSON,
                "the Result JSON must use the canonical two-space typed formatting",
            ),
            Err(message) => invalid(error_code::INVALID_RESULT_PAYLOAD, message),
        },
    }
}

fn candidate_kind(body: &str) -> Option<Result<EnvelopeKind, EnvelopeError>> {
    if body.starts_with(ASSIGNMENT_FAMILY) {
        return Some(Ok(EnvelopeKind::Assignment));
    }
    if body.starts_with(RESULT_FAMILY) {
        return Some(Ok(EnvelopeKind::Result));
    }

    let wrapper_start = body
        .find("<details")
        .or_else(|| body.find("<summary>WorkGraph "))
        .or_else(|| body.find(DETAILS_CLOSE))?;
    let wrapper = &body[wrapper_start..];
    let header = wrapper.split(FENCE_OPEN).next().unwrap_or(wrapper);
    let lines: Vec<&str> = header.split('\n').collect();
    let assignment_marker = lines
        .iter()
        .position(|line| line.trim_end_matches('\r').starts_with(ASSIGNMENT_FAMILY));
    let result_marker = lines
        .iter()
        .position(|line| line.trim_end_matches('\r').starts_with(RESULT_FAMILY));
    match (assignment_marker, result_marker) {
        (Some(assignment), Some(result)) if assignment < result => {
            return Some(Ok(EnvelopeKind::Assignment));
        }
        (Some(_), Some(_)) => return Some(Ok(EnvelopeKind::Result)),
        (Some(_), None) => return Some(Ok(EnvelopeKind::Assignment)),
        (None, Some(_)) => return Some(Ok(EnvelopeKind::Result)),
        (None, None) => {}
    }

    let assignment_summary = lines
        .iter()
        .position(|line| line.trim_end_matches('\r') == ASSIGNMENT_SUMMARY);
    let result_summary = lines
        .iter()
        .position(|line| line.trim_end_matches('\r') == RESULT_SUMMARY);
    match (assignment_summary, result_summary) {
        (Some(assignment), Some(result)) if assignment < result => {
            return Some(Ok(EnvelopeKind::Assignment));
        }
        (Some(_), Some(_)) => return Some(Ok(EnvelopeKind::Result)),
        (Some(_), None) => return Some(Ok(EnvelopeKind::Assignment)),
        (None, Some(_)) => return Some(Ok(EnvelopeKind::Result)),
        (None, None) => {}
    }

    let escaped_lines = !wrapper.contains('\n') && wrapper.contains("\\n");
    let assignment_marker = escaped_lines && wrapper.contains(ASSIGNMENT_FAMILY);
    let result_marker = escaped_lines && wrapper.contains(RESULT_FAMILY);

    match (assignment_marker, result_marker) {
        (true, false) => Some(Ok(EnvelopeKind::Assignment)),
        (false, true) => Some(Ok(EnvelopeKind::Result)),
        (true, true) => Some(envelope_err(
            error_code::INVALID_ENVELOPE,
            "a WorkGraph comment cannot contain both Assignment and Result markers",
        )),
        (false, false) => None,
    }
}

fn split_envelope(body: &str, kind: EnvelopeKind) -> Result<(String, String), EnvelopeError> {
    let invalid_format = || {
        envelope_err(
            error_code::INVALID_ENVELOPE,
            "the WorkGraph details wrapper, labels, spacing, and LF bytes must be exact",
        )
    };
    if body.contains('\r') {
        return invalid_format();
    }

    if let Some(version) = marker_version(body, kind) {
        if version != SUPPORTED_VERSION {
            let message = format!("unsupported WorkGraph version '{version}', expected 'v1'");
            return envelope_err(error_code::UNSUPPORTED_VERSION, message);
        }
    }

    let lines: Vec<&str> = body.split('\n').collect();
    if lines.first() != Some(&DETAILS_OPEN)
        || lines.get(1) != Some(&kind.summary())
        || lines.get(2) != Some(&"")
        || lines.get(3) != Some(&kind.marker())
        || lines.get(4) != Some(&"")
    {
        return invalid_format();
    }

    let Some(summary) = lines.get(5) else {
        return envelope_err(
            error_code::MISSING_HUMAN_SUMMARY,
            "a non-empty one-line human summary is required",
        );
    };
    if summary.trim().is_empty() || *summary == FENCE_OPEN {
        return envelope_err(
            error_code::MISSING_HUMAN_SUMMARY,
            "a non-empty one-line human summary is required",
        );
    }
    if lines.get(6) != Some(&"") {
        return invalid_format();
    }

    let fence_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with(FENCE_CLOSE).then_some(index))
        .collect();
    if lines.get(7) != Some(&FENCE_OPEN) {
        let code = if fence_lines.is_empty() {
            error_code::MISSING_JSON_BLOCK
        } else {
            error_code::INVALID_ENVELOPE
        };
        return envelope_err(code, "exactly one fenced ```json block is required");
    }
    if fence_lines.len() > 2 {
        return envelope_err(
            error_code::MULTIPLE_JSON_BLOCKS,
            "only one fenced JSON block is allowed in a WorkGraph comment",
        );
    }

    let Some(close) = lines[8..]
        .iter()
        .position(|line| *line == FENCE_CLOSE)
        .map(|offset| 8 + offset)
    else {
        return envelope_err(
            error_code::UNTERMINATED_JSON_BLOCK,
            "the ```json block is not terminated by an exact ``` line",
        );
    };
    if fence_lines.iter().any(|index| *index > close) {
        let message = "only one fenced block is allowed in a WorkGraph comment";
        return envelope_err(error_code::MULTIPLE_JSON_BLOCKS, message);
    }

    let tail = &lines[close + 1..];
    if tail != [DETAILS_CLOSE, ""] {
        let code = if tail.first() == Some(&DETAILS_CLOSE) {
            error_code::UNEXPECTED_TRAILING_CONTENT
        } else {
            error_code::INVALID_ENVELOPE
        };
        return envelope_err(
            code,
            "the closing fence must be followed by </details> and exactly one final LF",
        );
    }

    Ok(((*summary).to_string(), lines[8..close].join("\n")))
}

fn marker_version(body: &str, kind: EnvelopeKind) -> Option<&str> {
    body.split('\n')
        .find_map(|line| line.strip_prefix(kind.family()))
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
