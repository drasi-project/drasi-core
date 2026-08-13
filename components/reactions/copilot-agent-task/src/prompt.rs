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
//! query this reaction subscribes to) — carrying the reaction-generated
//! `eventId` (`expectedEventId`) and `executionId` — and that this event **must** be
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
use crate::state::ExecutionRecord;

/// The exact `WorkGraphEvent/v1` schema the launched agent must produce.
/// Kept here (rather than only in prose) so the prompt can embed a literal,
/// checkable JSON Schema fragment instead of an informal description.
pub fn work_graph_event_v1_schema() -> Value {
    serde_json::from_str(include_str!("../schema/workgraph-event-v1.schema.json"))
        .expect("committed WorkGraphEvent/v1 schema must be valid JSON")
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
            "schemaVersion", "messageType", "routeId", "responsibilityId", "executionId",
            "expectedEventId", "requiredEventType", "taskId", "taskUrl", "agentProfile",
            "profileRef", "requestedModel", "actualModel", "state", "startedAt"
        ],
        "properties": {
            "schemaVersion": { "const": "workgraph.execution/v1" },
            "messageType": { "const": "execution" },
            "routeId": { "type": "string" },
            "responsibilityId": { "type": "string" },
            "executionId": { "type": "string" },
            "expectedEventId": { "type": "string" },
            "requiredEventType": { "const": "CompletedIssueValidation" },
            "taskId": { "type": "string" },
            "taskUrl": { "type": "string" },
            "agentProfile": { "type": "string" },
            "profileRef": { "type": "string" },
            "requestedModel": { "type": "string" },
            "actualModel": { "type": "string" },
            "state": { "const": "started" },
            "startedAt": { "type": "string", "format": "date-time" }
        },
        "additionalProperties": false
    })
}

/// Build the exact prompt sent as the Agent Task's `prompt` field.
///
/// Embeds the exact frozen target, the literal `WorkGraphEvent/v1` schema,
/// and the ordering constraint between the required completion event and any
/// `AwaitingRouting` event.
pub fn build_prompt(row: &LaunchRow, execution_id: &str, expected_event_id: &str) -> String {
    let schema = work_graph_event_v1_schema();
    let schema_json = serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string());
    let target = target_payload(row, execution_id, expected_event_id);
    let target_json = serde_json::to_string_pretty(&target).unwrap_or_else(|_| "{}".to_string());

    format!(
        r#"You are operating as the "{agent_profile}" custom agent.

## Single target

You have exactly one target. Do not discover, infer, or act on any other target.
Use every field below exactly as supplied:

```json
{target_json}
```

Reconciliation correlation: `WorkGraph-Execution: {execution_id}`

## Required completion event

When you finish this responsibility, you MUST emit exactly one `WorkGraphEvent/v1`
issue comment in exactly this format:

WorkGraphEvent/v1
```json
{{the JSON object required by the schema below}}
```

The object MUST have `eventType` set to `{required_event_type}`, `eventId` set
to the target's `eventId`, `executionId` set to the target's `executionId`,
and `contentVersion` set to the target's `contentVersion`. Copy the target's
correlation and identity fields exactly. The event's JSON Schema is:

```json
{schema_json}
```

## Ordering requirement

Post the `{required_event_type}` issue comment above and confirm that GitHub
accepted it. Only then update this Project item's `Status` to
`AwaitingRouting`. Never update the status first: downstream routing must not
observe routing readiness before the validation result is recorded.

## Task

Carry out the target responsibility using the custom agent profile pinned by
the target's `profileRef`. Do not open a pull request as part of this task.
"#,
        agent_profile = row.agent_profile,
        execution_id = execution_id,
        required_event_type = row.required_event_type,
        schema_json = schema_json,
        target_json = target_json,
    )
}

/// Frozen WorkGraph target supplied to the coding agent. No other issue,
/// project, actor, or correlation fields are included in the target.
pub fn target_payload(row: &LaunchRow, execution_id: &str, expected_event_id: &str) -> Value {
    json!({
        "eventId": expected_event_id,
        "projectItemNodeId": row.project_item_node_id,
        "projectOwner": row.project_owner,
        "projectNumber": row.project_number,
        "subjectType": row.subject_type,
        "subjectNodeId": row.issue_node_id,
        "repository": row.repository,
        "subjectNumber": row.issue_number,
        "actorType": row.actor_type,
        "actorId": row.actor_id,
        "routeId": row.route_id,
        "responsibilityId": row.responsibility_id,
        "executionId": execution_id,
        "contentVersion": row.issue_content_version,
        "profileRef": row.profile_ref,
    })
}

/// The `workgraph.execution/v1` envelope posted as a single, pure-JSON issue
/// comment right after the task is confirmed created.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkGraphExecutionCommentV1 {
    pub schema_version: String,
    pub message_type: String,
    pub route_id: String,
    pub responsibility_id: String,
    pub execution_id: String,
    pub expected_event_id: String,
    pub required_event_type: String,
    pub task_id: String,
    pub task_url: String,
    pub agent_profile: String,
    pub profile_ref: String,
    pub requested_model: String,
    pub actual_model: String,
    pub state: String,
    pub started_at: String,
}

pub const WORKGRAPH_EXECUTION_SCHEMA_V1: &str = "workgraph.execution/v1";

impl WorkGraphExecutionCommentV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        record: &ExecutionRecord,
        execution_id: &str,
        expected_event_id: &str,
        task_id: &str,
        task_url: &str,
        actual_model: &str,
        started_at: &chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            schema_version: WORKGRAPH_EXECUTION_SCHEMA_V1.to_string(),
            message_type: "execution".to_string(),
            route_id: record.route_id.clone(),
            responsibility_id: record.responsibility_id.clone(),
            execution_id: execution_id.to_string(),
            expected_event_id: expected_event_id.to_string(),
            required_event_type: record.required_event_type.clone(),
            task_id: task_id.to_string(),
            task_url: task_url.to_string(),
            agent_profile: record.agent_profile.clone(),
            profile_ref: record.profile_ref.clone(),
            requested_model: record.requested_model.clone(),
            actual_model: actual_model.to_string(),
            state: "started".to_string(),
            started_at: started_at.to_rfc3339(),
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
    fn sample_row() -> LaunchRow {
        LaunchRow {
            repository: "drasi-project/drasi-core".to_string(),
            issue_number: 42,
            issue_url: "https://github.com/drasi-project/drasi-core/issues/42".to_string(),
            issue_node_id: "I_kwDOtest".to_string(),
            project_item_node_id: "PVTI_test".to_string(),
            project_node_id: "PVT_test".to_string(),
            project_owner: "drasi-project".to_string(),
            project_number: 3,
            subject_type: "Issue".to_string(),
            actor_type: "Agent".to_string(),
            actor_id: "issue-validator".to_string(),
            route_id: "route-1".to_string(),
            responsibility_id: "resp-1".to_string(),
            issue_content_version: "2026-08-13T19:00:00Z".to_string(),
            agent_profile: "issue-validator".to_string(),
            profile_ref: "issue-validator@0123456789abcdef0123456789abcdef01234567".to_string(),
            requested_model: "gpt-5".to_string(),
            fallback_model: Some("gpt-4".to_string()),
            required_event_type: "CompletedIssueValidation".to_string(),
            base_ref: "main".to_string(),
            expected_project_status: "AwaitingValidation".to_string(),
        }
    }

    #[test]
    fn prompt_contains_all_correlation_ids() {
        let row = sample_row();
        let prompt = build_prompt(
            &row,
            "execution:exec-123",
            "event:execution:exec-123:CompletedIssueValidation",
        );
        for needle in [
            "execution:exec-123",
            "event:execution:exec-123:CompletedIssueValidation",
            "route-1",
            "resp-1",
            "2026-08-13T19:00:00Z",
            "CompletedIssueValidation",
            "AwaitingRouting",
            "issue-validator@0123456789abcdef0123456789abcdef01234567",
            "\"projectOwner\": \"drasi-project\"",
            "\"projectNumber\": 3",
            "\"subjectType\": \"Issue\"",
            "\"actorType\": \"Agent\"",
            "WorkGraph-Execution: execution:exec-123",
        ] {
            assert!(prompt.contains(needle), "prompt missing '{needle}'");
        }
    }

    #[test]
    fn target_payload_has_exact_frozen_fields() {
        let target = target_payload(
            &sample_row(),
            "execution:exec-123",
            "event:execution:exec-123:CompletedIssueValidation",
        );
        let actual: std::collections::BTreeSet<&str> = target
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let expected = std::collections::BTreeSet::from([
            "eventId",
            "projectItemNodeId",
            "projectOwner",
            "projectNumber",
            "subjectType",
            "subjectNodeId",
            "repository",
            "subjectNumber",
            "actorType",
            "actorId",
            "routeId",
            "responsibilityId",
            "executionId",
            "contentVersion",
            "profileRef",
        ]);
        assert_eq!(actual, expected);
    }

    #[test]
    fn prompt_requires_completion_event_before_awaiting_routing() {
        let row = sample_row();
        let prompt = build_prompt(
            &row,
            "execution:exec-123",
            "event:execution:exec-123:CompletedIssueValidation",
        );
        let completion_pos = prompt
            .find("Post the `CompletedIssueValidation` issue comment")
            .unwrap();
        let status_pos = prompt
            .find("Only then update this Project item's `Status`")
            .unwrap();
        assert!(completion_pos < status_pos);
        assert!(prompt[status_pos..].contains("`AwaitingRouting`"));
    }

    #[test]
    fn schema_requires_workgraph_event_v1_fields() {
        let schema = work_graph_event_v1_schema();
        let required = schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        for field in [
            "schemaVersion",
            "eventType",
            "eventId",
            "projectItemNodeId",
            "subjectType",
            "subjectNodeId",
            "repository",
            "subjectNumber",
            "actorType",
            "actorId",
            "routeId",
            "responsibilityId",
            "executionId",
            "contentVersion",
            "profileRef",
            "result",
            "completedAt",
        ] {
            assert!(names.contains(&field), "schema missing required '{field}'");
        }
    }

    #[test]
    fn comment_envelope_is_pure_json_with_expected_fields() {
        let row = sample_row();
        let record = ExecutionRecord::new_reserved(
            &row.route_id,
            &row.responsibility_id,
            1,
            "execution:exec-123",
            "event:execution:exec-123:CompletedIssueValidation",
            &row.required_event_type,
            &row.repository,
            row.issue_number,
            &row.issue_node_id,
            &row.agent_profile,
            &row.profile_ref,
            &row.requested_model,
            row.fallback_model.as_deref(),
        );
        let ts = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let envelope = WorkGraphExecutionCommentV1::new(
            &record,
            "execution:exec-123",
            "event:execution:exec-123:CompletedIssueValidation",
            "task-1",
            "https://github.com/tasks/1",
            "gpt-5",
            &ts,
        );
        let body = envelope.to_comment_body();
        let parsed: Value = serde_json::from_str(&body).expect("comment body is pure JSON");
        assert_eq!(parsed["schemaVersion"], "workgraph.execution/v1");
        assert_eq!(parsed["messageType"], "execution");
        assert_eq!(
            parsed["expectedEventId"],
            "event:execution:exec-123:CompletedIssueValidation"
        );
        assert_eq!(parsed["requiredEventType"], "CompletedIssueValidation");
        assert_eq!(parsed["executionId"], "execution:exec-123");
        assert_eq!(parsed["taskId"], "task-1");
        assert_eq!(parsed["requestedModel"], "gpt-5");
        assert_eq!(parsed["actualModel"], "gpt-5");
        assert_eq!(parsed["state"], "started");
    }
}
