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

//! `WorkGraphEvent/v1` schema, the launch prompt built around it, and the
//! `workgraph.execution/v1` issue-comment envelope posted after a
//! successful launch.
//!
//! # Contract
//!
//! The prompt sent to the Copilot coding-agent task instructs it that, upon
//! completing its assigned responsibility, it **must** emit exactly one
//! `WorkGraphEvent/v1` object whose `eventType` equals the row's
//! `requiredEventType` (always `CompletedIssueValidation` in the launch
//! query this reaction subscribes to) — carrying the given `eventId`
//! (`expectedEventId`) and `executionId` — and that this event **must** be
//! emitted **before** any `AwaitingRouting` event for the same issue, so a
//! downstream router never observes routing-readiness before validation
//! completion is recorded.
//!
//! The reaction itself posts a *different*, immediate envelope —
//! `workgraph.execution/v1` — as a single issue comment right after the task
//! is confirmed created. It records the launch itself (not the eventual
//! task outcome) so the router and any humans watching the issue can see,
//! from GitHub alone, that a task was launched and which correlation IDs to
//! expect back.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::row::LaunchRow;

/// The exact `WorkGraphEvent/v1` schema the launched agent must produce.
/// Kept here (rather than only in prose) so the prompt can embed a literal,
/// checkable JSON Schema fragment instead of an informal description.
pub fn work_graph_event_v1_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://drasi.io/schemas/workgraph/WorkGraphEvent-v1.json",
        "title": "WorkGraphEvent/v1",
        "type": "object",
        "required": ["schema", "eventType", "eventId", "routeId", "responsibilityId", "executionId", "issueContentVersion", "result"],
        "properties": {
            "schema": { "const": "WorkGraphEvent/v1" },
            "eventType": { "type": "string" },
            "eventId": { "type": "string" },
            "routeId": { "type": "string" },
            "responsibilityId": { "type": "string" },
            "executionId": { "type": "string" },
            "issueContentVersion": { "type": "string" },
            "result": { "enum": ["success", "failure"] },
            "summary": { "type": "string" }
        },
        "additionalProperties": false
    })
}

/// The exact `workgraph.execution/v1` schema, mirrored by hand in
/// `schema/workgraph-execution-v1.schema.json` (see
/// `tests/output_schema.rs` for the drift guard).
pub fn workgraph_execution_v1_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://drasi.io/schemas/workgraph/workgraph.execution-v1.json",
        "title": "workgraph.execution/v1",
        "description": "The single, pure-JSON issue comment the reaction posts immediately after a task is confirmed created (HTTP 201). Mirrors WorkGraphExecutionCommentV1 in src/prompt.rs — keep in sync.",
        "type": "object",
        "required": [
            "schema", "executionId", "expectedEventId", "requiredEventType", "routeId",
            "responsibilityId", "repository", "issueNumber", "taskId", "taskUrl", "model",
            "fallbackUsed", "requestedAt", "baseRef"
        ],
        "properties": {
            "schema": { "const": "workgraph.execution/v1" },
            "executionId": { "type": "string" },
            "expectedEventId": { "type": "string" },
            "requiredEventType": { "type": "string" },
            "routeId": { "type": "string" },
            "responsibilityId": { "type": "string" },
            "repository": { "type": "string" },
            "issueNumber": { "type": "integer" },
            "taskId": { "type": "string" },
            "taskUrl": { "type": "string" },
            "model": { "type": "string" },
            "fallbackUsed": { "type": "boolean" },
            "requestedAt": { "type": "string", "format": "date-time" },
            "baseRef": { "type": "string" }
        },
        "additionalProperties": false
    })
}

/// Build the exact prompt sent as the Agent Task's `prompt` field.
///
/// Embeds every correlation ID the launched agent must echo back
/// (`executionId`, `expectedEventId`, `routeId`, `responsibilityId`), the
/// literal `WorkGraphEvent/v1` schema, and the ordering constraint between
/// the required completion event and any `AwaitingRouting` event.
pub fn build_prompt(row: &LaunchRow, execution_id: &str) -> String {
    let schema = work_graph_event_v1_schema();
    let schema_json = serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string());

    format!(
        r#"You are operating as the "{agent_profile}" agent profile against {repository} issue #{issue_number} ({issue_url}).

## Correlation identifiers (do not change these; echo them back exactly)

- executionId: {execution_id}
- expectedEventId: {expected_event_id}
- routeId: {route_id}
- responsibilityId: {responsibility_id}
- issueContentVersion: {issue_content_version}
- baseRef: {base_ref}

## Required completion event

When you finish this responsibility, you MUST emit exactly one `WorkGraphEvent/v1`
object with `eventType` set to `{required_event_type}`, `eventId` set to the
`expectedEventId` above, and `executionId` set to the `executionId` above. The
event's JSON Schema is:

```json
{schema_json}
```

## Ordering requirement

The `{required_event_type}` event above MUST be emitted before any
`AwaitingRouting` event is produced for this issue. Downstream routing must
never observe a workflow that appears ready to route before this validation
result has been recorded. Do not emit `AwaitingRouting` yourself; this
ordering requirement exists solely to constrain the relative order in which
events referencing this issue are allowed to appear.

## Task

Carry out the "{agent_profile}" responsibility for this issue as configured by
the profile pinned at `{profile_path}` (blob `{profile_blob_sha}`) on ref
`{base_ref}`. Do not open a pull request as part of this task.
"#,
        agent_profile = row.agent_profile,
        repository = row.repository,
        issue_number = row.issue_number,
        issue_url = row.issue_url,
        execution_id = execution_id,
        expected_event_id = row.expected_event_id,
        route_id = row.route_id,
        responsibility_id = row.responsibility_id,
        issue_content_version = row.issue_content_version,
        base_ref = row.base_ref,
        required_event_type = row.required_event_type,
        schema_json = schema_json,
        profile_path = row
            .profile_path_and_sha()
            .map(|(p, _)| p)
            .unwrap_or(&row.profile_ref),
        profile_blob_sha = row.profile_path_and_sha().map(|(_, s)| s).unwrap_or(""),
    )
}

/// The `workgraph.execution/v1` envelope posted as a single, pure-JSON issue
/// comment right after the task is confirmed created.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkGraphExecutionCommentV1 {
    /// Always `"workgraph.execution/v1"`.
    pub schema: String,
    pub execution_id: String,
    pub expected_event_id: String,
    pub required_event_type: String,
    pub route_id: String,
    pub responsibility_id: String,
    pub repository: String,
    pub issue_number: u64,
    pub task_id: String,
    pub task_url: String,
    pub model: String,
    pub fallback_used: bool,
    /// RFC 3339 timestamp of the create-task request.
    pub requested_at: String,
    pub base_ref: String,
}

pub const WORKGRAPH_EXECUTION_SCHEMA_V1: &str = "workgraph.execution/v1";

impl WorkGraphExecutionCommentV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        row: &LaunchRow,
        execution_id: &str,
        task_id: &str,
        task_url: &str,
        model: &str,
        fallback_used: bool,
        requested_at: &chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            schema: WORKGRAPH_EXECUTION_SCHEMA_V1.to_string(),
            execution_id: execution_id.to_string(),
            expected_event_id: row.expected_event_id.clone(),
            required_event_type: row.required_event_type.clone(),
            route_id: row.route_id.clone(),
            responsibility_id: row.responsibility_id.clone(),
            repository: row.repository.clone(),
            issue_number: row.issue_number,
            task_id: task_id.to_string(),
            task_url: task_url.to_string(),
            model: model.to_string(),
            fallback_used,
            requested_at: requested_at.to_rfc3339(),
            base_ref: row.base_ref.clone(),
        }
    }

    /// Render as the exact pure-JSON comment body (no markdown fencing —
    /// "trusted pure-JSON" per the reaction's contract).
    pub fn to_comment_body(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_row() -> LaunchRow {
        LaunchRow {
            repository: "drasi-project/drasi-core".to_string(),
            issue_number: 42,
            issue_url: "https://github.com/drasi-project/drasi-core/issues/42".to_string(),
            issue_node_id: "I_kwDOtest".to_string(),
            project_item_node_id: "PVTI_test".to_string(),
            route_id: "route-1".to_string(),
            responsibility_id: "resp-1".to_string(),
            issue_content_version: "deadbeef".to_string(),
            agent_profile: "issue-validator".to_string(),
            profile_ref: "profiles/issue-validator.yml@abc123sha".to_string(),
            requested_model: "gpt-5".to_string(),
            fallback_model: Some("gpt-4".to_string()),
            required_event_type: "CompletedIssueValidation".to_string(),
            expected_event_id: "evt-1".to_string(),
            base_ref: "main".to_string(),
            expected_project_status: "In Progress".to_string(),
        }
    }

    #[test]
    fn prompt_contains_all_correlation_ids() {
        let row = sample_row();
        let prompt = build_prompt(&row, "exec-123");
        for needle in [
            "exec-123",
            "evt-1",
            "route-1",
            "resp-1",
            "deadbeef",
            "main",
            "CompletedIssueValidation",
            "AwaitingRouting",
            "profiles/issue-validator.yml",
            "abc123sha",
        ] {
            assert!(prompt.contains(needle), "prompt missing '{needle}'");
        }
    }

    #[test]
    fn prompt_requires_completion_event_before_awaiting_routing() {
        let row = sample_row();
        let prompt = build_prompt(&row, "exec-123");
        let completion_pos = prompt.find("MUST be emitted before any").unwrap();
        let ordering_line = &prompt[completion_pos..];
        assert!(ordering_line.contains("AwaitingRouting"));
    }

    #[test]
    fn schema_requires_workgraph_event_v1_fields() {
        let schema = work_graph_event_v1_schema();
        let required = schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        for field in [
            "schema",
            "eventType",
            "eventId",
            "routeId",
            "responsibilityId",
            "executionId",
            "issueContentVersion",
            "result",
        ] {
            assert!(names.contains(&field), "schema missing required '{field}'");
        }
    }

    #[test]
    fn comment_envelope_is_pure_json_with_expected_fields() {
        let row = sample_row();
        let ts = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let envelope = WorkGraphExecutionCommentV1::new(
            &row,
            "exec-123",
            "task-1",
            "https://github.com/tasks/1",
            "gpt-5",
            false,
            &ts,
        );
        let body = envelope.to_comment_body();
        let parsed: Value = serde_json::from_str(&body).expect("comment body is pure JSON");
        assert_eq!(parsed["schema"], "workgraph.execution/v1");
        assert_eq!(parsed["expectedEventId"], "evt-1");
        assert_eq!(parsed["requiredEventType"], "CompletedIssueValidation");
        assert_eq!(parsed["executionId"], "exec-123");
        assert_eq!(parsed["taskId"], "task-1");
        assert_eq!(parsed["fallbackUsed"], false);
    }
}
