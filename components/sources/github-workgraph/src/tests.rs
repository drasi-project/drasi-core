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

use crate::agent_client::{AgentFileClient, AgentFileError};
use crate::agent_sync::push_touches_agent_file;
use crate::agents::{
    error_code as agent_error_code, parse_agent_file, parse_iso8601_duration_seconds,
    AgentFileContent, AgentFileLocation,
};
use crate::config::{
    AgentConfig, GitHubWorkGraphSourceConfig, LeaseTrust, RepositoryFilter, TaskIssueType,
    TrustedIdentity, WebhookConfig, DEFAULT_AGENT_API_BASE_URL,
};
use crate::descriptor::GitHubWorkGraphSourceDescriptor;
use crate::lease_ledger::{
    AgentRuntime, AllocationArtifact, AllocationDelta, AllocationEvent, AllocationState, Allocator,
};
use crate::mapping::{
    agent_changes, allocation_changes, AgentProjection, Conversion, Converter, NODE_LABELS,
    RELATION_LABELS,
};
use crate::vnext::{
    LifecycleArtifactDocument, PreparedProjection, PreparedProjectionCommit, ProjectionInput,
    TaskDocument, VNextAllocatorProjection, VNextAssignmentBinding, VNextDispatchBinding,
    VNextTaskBinding, WorkGraphProjector,
};
use crate::webhook::verify_signature;
use crate::workgraph::{
    classify_comment, classify_task_body, error_code, CommentClassification, Outcome,
    TaskClassification, TaskInputs, TaskType, WorkflowJoin,
};
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use drasi_core::evaluation::context::QueryPartEvaluationContext;
use drasi_core::evaluation::functions::FunctionRegistry;
use drasi_core::evaluation::variable_value::VariableValue;
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_core::query::{ContinuousQuery, QueryBuilder};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::{CapacityPolicy, WalError, WalProvider, WriteAheadLogConfig};
use drasi_lib::{DurabilityConfig, MemoryStateStoreProvider};
use drasi_plugin_sdk::prelude::SourcePluginDescriptor;
use drasi_query_cypher::CypherParser;
use drasi_wal_redb::RedbWalProvider;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;

const TASK_TYPE_ID: &str = "IT_test";
const TASK_TYPE_NAME: &str = "WorkGraphTask";
const VALIDATION_TASK: &str = r#"WorkGraphTask/v1

```yaml
taskType: validate-issue
inputs:
  validationProfile: new-issue-default
```
"#;
const REQUEST_INFO_TASK: &str = r#"WorkGraphTask/v1

```yaml
taskType: request-info
inputs:
  validationResultCommentNodeId: IC_validation_result
```
"#;
const WORKFLOW_COMPOSITE_TASK: &str = r#"WorkGraphTask/v2

```yaml
taskType: workflow-task
inputs:
  workflowId: issue-lifecycle
  workflowRunId: run-001
  stepId: parallel-validation
  definitionCommit: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  definitionDigest: sha256:0000000000000000000000000000000000000000000000000000000000000000
  generation: 1
  operation: evaluate-validation
  agent: issue-validation-evaluator
  inputs:
    issueNodeId: I_parent
  join: all
  expectedChildCount: 2
  children:
    - branchId: title
      operation: validate-title
      agent: issue-title-validator
      inputs:
        field: title
    - branchId: body
      operation: validate-body
      agent: issue-body-validator
      inputs:
        field: body
```
"#;
const WORKFLOW_BRANCH_TASK: &str = r#"WorkGraphTask/v2

```yaml
taskType: workflow-task
inputs:
  workflowId: issue-lifecycle
  workflowRunId: run-001
  stepId: parallel-validation
  definitionCommit: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  definitionDigest: sha256:0000000000000000000000000000000000000000000000000000000000000000
  generation: 1
  operation: validate-title
  agent: issue-title-validator
  inputs:
    field: title
  branchId: title
```
"#;
const WORKFLOW_FLOW_STYLE_TASK: &str = r#"WorkGraphTask/v2

```yaml
taskType: workflow-task
inputs: {
  "workflowId": "issue-lifecycle",
  "workflowRunId": "run-001",
  "stepId": "parallel-validation",
  "definitionCommit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "definitionDigest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
  "generation": 1,
  "operation": "validate-title",
  "agent": "issue-title-validator",
  "inputs": {
    "field": "title",
    "rule": "non-empty"
  },
  "branchId": "title"
}
```
"#;
const ASSIGNMENT: &str = r#"WorkGraphTaskAssignment/v1

```json
{
  "agentId": "issue-validator"
}
```
"#;
const INFO_REQUEST_ASSIGNMENT: &str = r#"WorkGraphTaskAssignment/v1

```json
{
  "agentId": "issue-info-requester"
}
```
"#;
const RESULT: &str = r#"WorkGraphTaskResult/v1

```json
{
  "taskType": "validate-issue",
  "leaseId": "00000000-0000-7000-8000-000000000001",
  "outcome": "succeeded",
  "summary": "Validated the issue.",
  "result": {
    "criteria": [
      {
        "criterion": "Acceptance criteria",
        "passed": true,
        "evidence": "Present."
      }
    ]
  }
}
```
"#;
const REQUEST_INFO_RESULT: &str = r#"WorkGraphTaskResult/v1

```json
{
  "taskType": "request-info",
  "leaseId": "00000000-0000-7000-8000-000000000002",
  "outcome": "succeeded",
  "summary": "Requested the missing information.",
  "result": {
    "requestCommentNodeId": "IC_request"
  }
}
```
"#;
const WORKFLOW_RESULT: &str = r#"WorkGraphTaskResult/v1

```json
{
  "taskType": "workflow-task",
  "leaseId": "00000000-0000-7000-8000-000000000003",
  "outcome": "succeeded",
  "summary": "Selected the next viable workflow outcome.",
  "result": {
    "decision": "triage"
  }
}
```
"#;
const ACCEPTANCE: &str = r#"WorkGraphTaskResultAcceptance/v1

```json
{
  "resultCommentNodeId": "IC_result",
  "resultBodyDigest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
  "summary": "Accepted the current result revision."
}
```
"#;

fn acceptance_body(result_comment_node_id: &str, result_body_digest: &str) -> String {
    format!(
        "WorkGraphTaskResultAcceptance/v1\n\n```json\n{{\n  \"resultCommentNodeId\": \
         \"{result_comment_node_id}\",\n  \"resultBodyDigest\": \"{result_body_digest}\",\n  \
         \"summary\": \"Accepted the current result revision.\"\n}}\n```\n"
    )
}

fn assignment_body(agent_id: &str) -> String {
    format!("WorkGraphTaskAssignment/v1\n\n```json\n{{\n  \"agentId\": \"{agent_id}\"\n}}\n```\n")
}

fn result_body(lease_id: &str) -> String {
    RESULT.replace("00000000-0000-7000-8000-000000000001", lease_id)
}

fn task_type() -> TaskIssueType {
    TaskIssueType {
        id: TASK_TYPE_ID.to_string(),
        name: TASK_TYPE_NAME.to_string(),
    }
}

fn org() -> Value {
    json!({"login":"acme","id":42,"node_id":"O_1"})
}

fn repo(name: &str) -> Value {
    json!({
        "node_id": format!("R_{name}"), "id": 7, "name": name,
        "full_name": format!("acme/{name}"), "owner":{"login":"acme"},
        "url":format!("https://api.github.com/repos/acme/{name}"),
        "html_url": format!("https://github.com/acme/{name}"), "private":false,
        "archived":false, "fork":false, "visibility":"public"
    })
}

fn issue(id: &str, body: &str, typed: bool, state: &str) -> Value {
    json!({
        "node_id": id, "id": 42, "number":42, "title":"Work item", "body":body,
        "state":state, "state_reason": if state == "closed" { json!("completed") } else { Value::Null },
        "locked":false, "created_at":"2026-01-01T00:00:00Z",
        "updated_at":"2026-01-02T00:00:00Z",
        "closed_at": if state == "closed" { json!("2026-01-02T00:00:00Z") } else { Value::Null },
        "labels":[], "assignees":[], "html_url":format!("https://github.com/acme/widgets/issues/{id}"),
        "repository_url":"https://api.github.com/repos/acme/widgets",
        "type": if typed { json!({"node_id":TASK_TYPE_ID,"name":TASK_TYPE_NAME}) } else { Value::Null },
        "user":{"login":"ada","node_id":"U_ada","id":1,"type":"User"}
    })
}

fn issue_event(action: &str, item: Value) -> Value {
    json!({"action":action,"organization":org(),"repository":repo("widgets"),"issue":item})
}

fn comment_event(action: &str, body: &str, state: &str, typed: bool, id: &str) -> Value {
    json!({
        "action":action, "organization":org(), "repository":repo("widgets"),
        "issue":issue("I_task", VALIDATION_TASK, typed, state),
        "comment":{
            "node_id":id,"id":9001,"body":body,
            "created_at":"2026-01-03T00:00:00Z","updated_at":"2026-01-03T00:00:00Z",
            "html_url":"https://github.com/acme/widgets/issues/42#issuecomment-9001",
            "user":{"login":"bot","node_id":"U_bot","id":2,"type":"Bot"}
        }
    })
}

fn sub_issue_event(action: &str, child: Value, mut parent: Value) -> Value {
    parent["repository_url"] = json!("https://api.github.com/repos/acme/parents");
    let child_database_id = child["id"].clone();
    let mut payload = if action.starts_with("parent_issue_") {
        json!({
            "action":action, "organization":org(), "repository":repo("widgets"),
            "parent_issue":parent, "parent_issue_repo":repo("parents"),
            "sub_issue":child
        })
    } else {
        json!({
            "action":action, "organization":org(), "repository":repo("parents"),
            "parent_issue":parent, "sub_issue":child, "sub_issue_repo":repo("widgets")
        })
    };
    if action.ends_with("_removed") {
        payload["sub_issue_id"] = child_database_id;
    }
    payload
}

fn lease_trust() -> LeaseTrust {
    LeaseTrust {
        dispatchers: vec![TrustedIdentity {
            id: "U_bot".to_string(),
            login: "bot".to_string(),
        }],
        reporters: vec![TrustedIdentity {
            id: "U_bot".to_string(),
            login: "bot".to_string(),
        }],
    }
}

fn convert(event: &str, payload: &Value) -> Vec<SourceChange> {
    convert_full(event, payload).changes
}

fn convert_full(event: &str, payload: &Value) -> Conversion {
    let trust = lease_trust();
    Converter::new("gh", "acme", &task_type(), 1)
        .with_lease_trust(&trust)
        .convert(event, payload)
        .unwrap()
        .unwrap()
}

/// Convert with no configured trust at all, to prove the fail-closed default.
fn convert_untrusted(event: &str, payload: &Value) -> Vec<SourceChange> {
    Converter::new("gh", "acme", &task_type(), 1)
        .convert(event, payload)
        .unwrap()
        .unwrap()
        .changes
}

async fn task_path_query() -> ContinuousQuery {
    let registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(registry.clone()));
    QueryBuilder::new(
        "MATCH (task:WorkGraphTask)-[:TASK_FOR]->(parent:GitHubIssue) \
         RETURN task.nodeId AS taskId, parent.nodeId AS parentId",
        parser,
    )
    .with_function_registry(registry)
    .build()
    .await
}

async fn task_node_query() -> ContinuousQuery {
    let registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(registry.clone()));
    QueryBuilder::new(
        "MATCH (task:WorkGraphTask) RETURN task.nodeId AS taskId",
        parser,
    )
    .with_function_registry(registry)
    .build()
    .await
}

async fn accepted_result_query() -> ContinuousQuery {
    let registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(registry.clone()));
    QueryBuilder::new(
        "MATCH (assignment:WorkGraphTaskAssignment)-[:ASSIGNMENT_FOR]->\
         (task:WorkGraphTask)<-[:RESULT_FOR]-(result:WorkGraphTaskResult)\
         <-[:ACCEPTS_RESULT]-(acceptance:WorkGraphTaskResultAcceptance) \
         WHERE acceptance.resultBodyDigest = result.bodyDigest \
         RETURN task.nodeId AS taskId, assignment.agentId AS agentId, \
         result.sourceCommentNodeId AS resultId, acceptance.sourceCommentNodeId AS acceptanceId",
        parser,
    )
    .with_function_registry(registry)
    .build()
    .await
}

async fn task_parent_query(parent_id: &str) -> ContinuousQuery {
    let registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(registry.clone()));
    QueryBuilder::new(
        format!(
            "MATCH (task:WorkGraphTask)-[:TASK_FOR]->(parent:GitHubIssue) \
             WHERE parent.nodeId = '{parent_id}' \
             RETURN task.nodeId AS taskId"
        ),
        parser,
    )
    .with_function_registry(registry)
    .build()
    .await
}

async fn generic_issue_query(predicate: Option<&str>) -> ContinuousQuery {
    let registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(registry.clone()));
    let query = match predicate {
        Some(predicate) => format!(
            "MATCH (issue:GitHubIssue) WHERE {predicate} \
             RETURN issue.nodeId AS issueId"
        ),
        None => "MATCH (issue:GitHubIssue) RETURN issue.nodeId AS issueId".to_string(),
    };
    QueryBuilder::new(query, parser)
        .with_function_registry(registry)
        .build()
        .await
}

async fn process_changes(
    query: &ContinuousQuery,
    changes: Vec<SourceChange>,
) -> Vec<QueryPartEvaluationContext> {
    let mut results = Vec::new();
    for change in changes {
        results.extend(query.process_source_change(change).await.unwrap());
    }
    results
}

fn additions(results: &[QueryPartEvaluationContext]) -> usize {
    results
        .iter()
        .filter(|result| matches!(result, QueryPartEvaluationContext::Adding { .. }))
        .count()
}

fn removals(results: &[QueryPartEvaluationContext]) -> usize {
    results
        .iter()
        .filter(|result| matches!(result, QueryPartEvaluationContext::Removing { .. }))
        .count()
}

fn label(change: &SourceChange) -> &str {
    let metadata = match change {
        SourceChange::Delete { metadata } => metadata,
        SourceChange::Insert { element } | SourceChange::Update { element } => match element {
            Element::Node { metadata, .. } | Element::Relation { metadata, .. } => metadata,
        },
        SourceChange::Future { .. } => panic!("unexpected future change"),
    };
    &metadata.labels[0]
}

fn id(change: &SourceChange) -> &str {
    &change.get_reference().element_id
}

fn relation_endpoints<'a>(
    changes: &'a [SourceChange],
    relation_label: &str,
    relation_id: &str,
) -> (&'a str, &'a str) {
    changes
        .iter()
        .find_map(|change| match change {
            SourceChange::Insert {
                element:
                    Element::Relation {
                        metadata,
                        in_node,
                        out_node,
                        ..
                    },
            }
            | SourceChange::Update {
                element:
                    Element::Relation {
                        metadata,
                        in_node,
                        out_node,
                        ..
                    },
            } if metadata.labels[0].as_ref() == relation_label
                && metadata.reference.element_id.as_ref() == relation_id =>
            {
                Some((in_node.element_id.as_ref(), out_node.element_id.as_ref()))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing relation {relation_label} {relation_id}"))
}

fn is_insert(change: &SourceChange) -> bool {
    matches!(change, SourceChange::Insert { .. })
}

fn is_update(change: &SourceChange) -> bool {
    matches!(change, SourceChange::Update { .. })
}

fn is_delete(change: &SourceChange) -> bool {
    matches!(change, SourceChange::Delete { .. })
}

fn property<'a>(changes: &'a [SourceChange], node_label: &str, key: &str) -> &'a ElementValue {
    changes
        .iter()
        .find_map(|change| match change {
            SourceChange::Insert {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            }
            | SourceChange::Update {
                element:
                    Element::Node {
                        metadata,
                        properties,
                    },
            } if metadata.labels[0].as_ref() == node_label => properties.get(key),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing {node_label}.{key}"))
}

fn has_property(changes: &[SourceChange], node_label: &str, key: &str) -> bool {
    changes.iter().any(|change| match change {
        SourceChange::Insert {
            element:
                Element::Node {
                    metadata,
                    properties,
                },
        }
        | SourceChange::Update {
            element:
                Element::Node {
                    metadata,
                    properties,
                },
        } if metadata.labels[0].as_ref() == node_label => properties.get(key).is_some(),
        _ => false,
    })
}

#[test]
fn task_envelopes_accept_only_strict_work_definitions() {
    for body in [
        VALIDATION_TASK,
        REQUEST_INFO_TASK,
        WORKFLOW_COMPOSITE_TASK,
        WORKFLOW_BRANCH_TASK,
        WORKFLOW_FLOW_STYLE_TASK,
    ] {
        assert!(matches!(
            classify_task_body(body),
            TaskClassification::Task(_)
        ));
    }
    for body in [
        &format!("{VALIDATION_TASK}\n"),
        "WorkGraphTask/v1\n\n```yaml\ntaskType: validate-issue\ninputs:\n  validationProfile: other\n```\n",
        "WorkGraphTask/v1\n\n```yaml\ntaskType: validate-issue\nagentId: issue-validator\ninputs:\n  validationProfile: new-issue-default\n```\n",
        "WorkGraphTask/v1\n\n```yaml\ntaskType: validate-issue\ninputs:\n  validationProfile: new-issue-default\n---\ntaskType: request-info\ninputs:\n  validationResultCommentNodeId: IC_result\n```\n",
        "WorkGraphTask/v1\n\n```yaml\ntaskType: workflow-task\ninputs: {}\n```\n",
        "WorkGraphTask/v2\n\n```yaml\ntaskType: validate-issue\ninputs:\n  validationProfile: new-issue-default\n```\n",
        &WORKFLOW_COMPOSITE_TASK.replace("expectedChildCount: 2", "expectedChildCount: 3"),
        &WORKFLOW_COMPOSITE_TASK.replace("branchId: body", "branchId: title"),
        &WORKFLOW_COMPOSITE_TASK.replace(
            "agent: issue-body-validator",
            "agent: issue-title-validator",
        ),
        &WORKFLOW_COMPOSITE_TASK.replace(
            "agent: issue-validation-evaluator",
            "agent: issue-title-validator",
        ),
        &WORKFLOW_COMPOSITE_TASK.replace(
            "  join: all",
            "  branchId: nested-composite\n  join: all",
        ),
        "WorkGraphTask/v2\n\n```yaml\n{}\n```\n",
        "prose\n{}",
    ] {
        assert!(matches!(
            classify_task_body(body),
            TaskClassification::Invalid(_)
        ));
    }
}

#[test]
fn workflow_task_v2_preserves_the_complete_direct_child_manifest() {
    let TaskClassification::Task(task) = classify_task_body(WORKFLOW_COMPOSITE_TASK) else {
        panic!("workflow composite must parse");
    };
    let TaskInputs::WorkflowTask(inputs) = task.inputs else {
        panic!("workflow composite must use workflow inputs");
    };

    assert_eq!(task.task_type, TaskType::WorkflowTask);
    assert_eq!(inputs.workflow_id, "issue-lifecycle");
    assert_eq!(inputs.generation, 1);
    assert_eq!(inputs.join, Some(WorkflowJoin::All));
    assert_eq!(inputs.expected_child_count, Some(2));
    assert_eq!(inputs.children.len(), 2);
    assert_eq!(inputs.children[0].branch_id, "title");
    assert_eq!(inputs.children[1].agent, "issue-body-validator");
}

#[test]
fn specialized_comment_grammars_are_mutually_exclusive() {
    assert!(matches!(
        classify_comment(ASSIGNMENT),
        CommentClassification::Assignment(_)
    ));
    assert!(matches!(
        classify_comment(INFO_REQUEST_ASSIGNMENT),
        CommentClassification::Assignment(_)
    ));
    assert!(matches!(
        classify_comment(RESULT),
        CommentClassification::Result(_)
    ));
    assert!(matches!(
        classify_comment(REQUEST_INFO_RESULT),
        CommentClassification::Result(_)
    ));
    assert!(matches!(
        classify_comment(WORKFLOW_RESULT),
        CommentClassification::Result(_)
    ));
    assert!(matches!(
        classify_comment(ACCEPTANCE),
        CommentClassification::Acceptance(_)
    ));
    assert!(matches!(
        classify_comment("WorkGraphTaskResult/v1\n\n```json\n{}\n```"),
        CommentClassification::Invalid(_)
    ));
    assert!(matches!(
        classify_comment("WorkGraphTaskAssignment/v1\n\n```json\n{\"agentId\":\"issue-validator\"}\n```\n"),
        CommentClassification::Invalid(error)
            if error.code == error_code::NON_CANONICAL_JSON
    ));
    assert!(matches!(
        classify_comment("WorkGraphTaskAssignment/v1\n\n```json\n{\n  \"agentId\": \"issue-risk-profiler\"\n}\n```\n"),
        CommentClassification::Assignment(assignment)
            if assignment.agent_id == "issue-risk-profiler"
    ));
    for invalid_agent_id in ["", "a/b", "has space", "agent@name"] {
        assert!(matches!(
            classify_comment(&assignment_body(invalid_agent_id)),
            CommentClassification::Invalid(error)
                if error.code == error_code::INVALID_ASSIGNMENT_PAYLOAD
        ));
    }
    assert!(matches!(
        classify_comment(&assignment_body(&"a".repeat(64))),
        CommentClassification::Assignment(_)
    ));
    assert!(matches!(
        classify_comment(&assignment_body(&"a".repeat(65))),
        CommentClassification::Invalid(error)
            if error.code == error_code::INVALID_ASSIGNMENT_PAYLOAD
    ));
    assert!(matches!(
        classify_comment("WorkGraphTaskResultAcceptance/v1\n\n```json\n{\n  \"resultCommentNodeId\": \"IC_result\",\n  \"resultBodyDigest\": \"sha256:ABC\",\n  \"summary\": \"Accepted.\"\n}\n```\n"),
        CommentClassification::Invalid(error)
            if error.code == error_code::INVALID_ACCEPTANCE_PAYLOAD
    ));
    assert!(matches!(
        classify_comment("WorkGraphTaskResultAcceptance/v2\n\n```json\n{}\n```\n"),
        CommentClassification::Invalid(error) if error.code == error_code::UNSUPPORTED_VERSION
    ));
    assert!(matches!(
        classify_comment("prefix WorkGraphTaskResult/v1"),
        CommentClassification::Ordinary
    ));
}

#[test]
fn typed_issue_emits_task_not_github_issue() {
    let changes = convert(
        "issues",
        &issue_event("opened", issue("I_task", VALIDATION_TASK, true, "open")),
    );
    assert!(changes.iter().any(|change| {
        label(change) == "WorkGraphTask" && id(change) == "I_task" && is_update(change)
    }));
    assert!(changes.iter().any(|change| {
        label(change) == "IN_REPOSITORY"
            && id(change) == "IN_REPOSITORY:I_task:R_widgets"
            && is_update(change)
    }));
    assert!(!changes.iter().any(|change| label(change) == "GitHubIssue"));
    assert_eq!(
        property(&changes, "WorkGraphTask", "taskType"),
        &ElementValue::from(&json!("validate-issue"))
    );
}

#[test]
fn workflow_task_v2_projects_the_runtime_manifest() {
    let changes = convert(
        "issues",
        &issue_event(
            "opened",
            issue("I_workflow_task", WORKFLOW_COMPOSITE_TASK, true, "open"),
        ),
    );

    assert_eq!(
        property(&changes, "WorkGraphTask", "taskType"),
        &ElementValue::from(&json!("workflow-task"))
    );
    assert_eq!(
        property(&changes, "WorkGraphTask", "inputs"),
        &ElementValue::from(&json!({
            "workflowId": "issue-lifecycle",
            "workflowRunId": "run-001",
            "stepId": "parallel-validation",
            "definitionCommit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "definitionDigest":
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "generation": 1,
            "operation": "evaluate-validation",
            "agent": "issue-validation-evaluator",
            "inputs": {"issueNodeId": "I_parent"},
            "join": "all",
            "expectedChildCount": 2,
            "children": [
                {
                    "branchId": "title",
                    "operation": "validate-title",
                    "agent": "issue-title-validator",
                    "inputs": {"field": "title"}
                },
                {
                    "branchId": "body",
                    "operation": "validate-body",
                    "agent": "issue-body-validator",
                    "inputs": {"field": "body"}
                }
            ]
        }))
    );
}

#[test]
fn contract_task_fixture_projects_canonical_task_parent_and_repository() {
    let task = issue_event("opened", issue("I_task", VALIDATION_TASK, true, "open"));
    let task_changes = Converter::new("gh", "acme", &task_type(), 42)
        .convert("issues", &task)
        .unwrap()
        .unwrap()
        .changes;
    for (key, expected) in [
        ("nodeId", json!("I_task")),
        ("databaseId", json!(42)),
        ("number", json!(42)),
        ("title", json!("Work item")),
        ("body", json!(VALIDATION_TASK)),
        ("state", json!("open")),
        ("authorLogin", json!("ada")),
        ("authorId", json!("U_ada")),
        ("repositoryNameWithOwner", json!("acme/widgets")),
        ("issueTypeId", json!(TASK_TYPE_ID)),
        ("issueTypeName", json!(TASK_TYPE_NAME)),
        ("taskType", json!("validate-issue")),
        ("inputs", json!({"validationProfile": "new-issue-default"})),
    ] {
        assert_eq!(
            property(&task_changes, "WorkGraphTask", key),
            &ElementValue::from(&expected),
            "unexpected WorkGraphTask.{key}"
        );
    }
    assert_eq!(
        relation_endpoints(
            &task_changes,
            "IN_REPOSITORY",
            "IN_REPOSITORY:I_task:R_widgets",
        ),
        ("I_task", "R_widgets")
    );
    assert!(!task_changes
        .iter()
        .any(|change| label(change) == "GitHubIssue"));
    assert!(task_changes.iter().all(|change| {
        let metadata = match change {
            SourceChange::Delete { metadata } => metadata,
            SourceChange::Insert { element } | SourceChange::Update { element } => {
                element.get_metadata()
            }
            SourceChange::Future { .. } => return false,
        };
        metadata.effective_from == 42
    }));

    let parent_changes = convert(
        "sub_issues",
        &sub_issue_event(
            "sub_issue_added",
            issue("I_task", VALIDATION_TASK, true, "open"),
            issue("I_parent", "Parent", false, "open"),
        ),
    );
    assert_eq!(
        relation_endpoints(&parent_changes, "TASK_FOR", "TASK_FOR:42"),
        ("I_task", "I_parent")
    );
}

#[test]
fn contract_assignment_fixture_projects_exact_v1_trust_and_relations() {
    let conversion = convert_full(
        "issue_comment",
        &comment_event("created", ASSIGNMENT, "open", true, "IC_assignment"),
    );
    let changes = &conversion.changes;
    for (key, expected) in [
        ("version", json!(1)),
        ("agentId", json!("issue-validator")),
        ("sourceCommentNodeId", json!("IC_assignment")),
        ("trusted", json!(true)),
        (
            "bodyDigest",
            json!(format!(
                "sha256:{}",
                hex::encode(Sha256::digest(ASSIGNMENT))
            )),
        ),
    ] {
        assert_eq!(
            property(changes, "WorkGraphTaskAssignment", key),
            &ElementValue::from(&expected),
            "unexpected Assignment.{key}"
        );
    }
    assert_eq!(
        relation_endpoints(changes, "COMMENT_ON", "COMMENT_ON:IC_assignment:I_task",),
        ("IC_assignment", "I_task")
    );
    assert_eq!(
        relation_endpoints(
            changes,
            "ASSIGNMENT_FOR",
            "ASSIGNMENT_FOR:IC_assignment:I_task",
        ),
        ("IC_assignment", "I_task")
    );
    assert_eq!(
        relation_endpoints(
            changes,
            "ASSIGNED_TO",
            "ASSIGNED_TO:IC_assignment:workgraph-agent:issue-validator",
        ),
        ("IC_assignment", "workgraph-agent:issue-validator")
    );
    assert!(matches!(
        conversion.allocation,
        Some(AllocationEvent::Comment {
            ref comment_node_id,
            ref task_node_id,
            artifact: Some(AllocationArtifact::Assignment {
                trusted: true,
                task_type: TaskType::ValidateIssue,
                ref agent_id,
                ref created_at,
            }),
        }) if comment_node_id == "IC_assignment"
            && task_node_id == "I_task"
            && agent_id == "issue-validator"
            && created_at == "2026-01-03T00:00:00Z"
    ));

    let untrusted = Converter::new("gh", "acme", &task_type(), 1)
        .convert(
            "issue_comment",
            &comment_event("created", ASSIGNMENT, "open", true, "IC_untrusted"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        property(&untrusted.changes, "WorkGraphTaskAssignment", "trusted"),
        &ElementValue::Bool(false)
    );
    assert!(matches!(
        untrusted.allocation,
        Some(AllocationEvent::Comment {
            artifact: Some(AllocationArtifact::Assignment { trusted: false, .. }),
            ..
        })
    ));

    let mut edited = comment_event("edited", ASSIGNMENT, "open", true, "IC_untrusted_edit");
    edited["sender"] = json!({
        "login": "mallory", "node_id": "U_mallory", "id": 3, "type": "User"
    });
    edited["changes"] = json!({"body": {"from": "ordinary"}});
    let edited = convert_full("issue_comment", &edited);
    assert!(matches!(
        edited.allocation,
        Some(AllocationEvent::Comment {
            artifact: Some(AllocationArtifact::Assignment { trusted: false, .. }),
            ..
        })
    ));

    for (id, body) in [
        (
            "IC_missing",
            "WorkGraphTaskAssignment/v1\n\n```json\n{}\n```\n",
        ),
        (
            "IC_legacy",
            "WorkGraphTaskAssignment/v1\n\n```json\n{\n  \"agentProfile\": \"issue-validator\",\n  \"workerId\": \"validator-1\"\n}\n```\n",
        ),
        (
            "IC_extra",
            "WorkGraphTaskAssignment/v1\n\n```json\n{\n  \"agentId\": \"issue-validator\",\n  \"queuePriority\": 1\n}\n```\n",
        ),
    ] {
        let invalid = convert_full(
            "issue_comment",
            &comment_event("created", body, "open", true, id),
        );
        assert!(invalid
            .changes
            .iter()
            .any(|change| label(change) == "WorkGraphError"));
        assert!(!invalid
            .changes
            .iter()
            .any(|change| label(change) == "WorkGraphTaskAssignment"));
        assert!(matches!(
            invalid.allocation,
            Some(AllocationEvent::Comment { artifact: None, .. })
        ));
    }
}

#[test]
fn contract_result_fixture_projects_exact_v1_relation_and_reporter_trust() {
    let conversion = convert_full(
        "issue_comment",
        &comment_event("created", RESULT, "open", true, "IC_result"),
    );
    for (key, expected) in [
        ("version", json!(1)),
        ("taskType", json!("validate-issue")),
        ("leaseId", json!("00000000-0000-7000-8000-000000000001")),
        ("outcome", json!("succeeded")),
        ("summary", json!("Validated the issue.")),
        (
            "result",
            json!({"criteria": [{
                "criterion": "Acceptance criteria",
                "passed": true,
                "evidence": "Present."
            }]}),
        ),
        ("trusted", json!(false)),
    ] {
        assert_eq!(
            property(&conversion.changes, "WorkGraphTaskResult", key),
            &ElementValue::from(&expected),
            "unexpected Result.{key}"
        );
    }
    assert_eq!(
        relation_endpoints(
            &conversion.changes,
            "RESULT_FOR",
            "RESULT_FOR:IC_result:I_task",
        ),
        ("IC_result", "I_task")
    );
    assert!(matches!(
        conversion.allocation,
        Some(AllocationEvent::Comment {
            artifact: Some(AllocationArtifact::Result {
                reporter_trusted: true,
                task_type: TaskType::ValidateIssue,
                ref lease_id,
                outcome: Outcome::Succeeded,
                ..
            }),
            ..
        }) if lease_id == "00000000-0000-7000-8000-000000000001"
    ));

    let untrusted = Converter::new("gh", "acme", &task_type(), 1)
        .convert(
            "issue_comment",
            &comment_event("created", RESULT, "open", true, "IC_untrusted"),
        )
        .unwrap()
        .unwrap();
    assert!(matches!(
        untrusted.allocation,
        Some(AllocationEvent::Comment {
            artifact: Some(AllocationArtifact::Result {
                reporter_trusted: false,
                ..
            }),
            ..
        })
    ));
}

#[test]
fn generic_issue_preserves_labels_and_projects_ordered_workgraph_namespaces() {
    let mut item = issue("I_generic", "ordinary", false, "OpEn");
    item["labels"] = json!([
        {"name":"status:New","node_id":"L_1"},
        {"name":"ordinary","node_id":"L_2"},
        {"name":"workgraph:ignore","node_id":"L_3"},
        {"name":"Status:not-matched","node_id":"L_4"},
        {"name":"status:awaiting-Triage","node_id":"L_5"},
        {"name":"workgraph:Error","node_id":"L_6"}
    ]);
    let changes = convert("issues", &issue_event("opened", item));

    assert_eq!(
        property(&changes, "GitHubIssue", "labels"),
        &ElementValue::from(&json!([
            "status:New",
            "ordinary",
            "workgraph:ignore",
            "Status:not-matched",
            "status:awaiting-Triage",
            "workgraph:Error"
        ]))
    );
    assert_eq!(
        property(&changes, "GitHubIssue", "labelDetails"),
        &ElementValue::from(&json!([
            {"name":"status:New","nodeId":"L_1"},
            {"name":"ordinary","nodeId":"L_2"},
            {"name":"workgraph:ignore","nodeId":"L_3"},
            {"name":"Status:not-matched","nodeId":"L_4"},
            {"name":"status:awaiting-Triage","nodeId":"L_5"},
            {"name":"workgraph:Error","nodeId":"L_6"}
        ]))
    );
    assert_eq!(
        property(&changes, "GitHubIssue", "statusLabels"),
        &ElementValue::from(&json!(["status:New", "status:awaiting-Triage"]))
    );
    assert_eq!(
        property(&changes, "GitHubIssue", "currentStatus"),
        &ElementValue::String(Arc::from("error"))
    );
    assert_eq!(
        property(&changes, "GitHubIssue", "workgraphLabels"),
        &ElementValue::from(&json!(["workgraph:ignore", "workgraph:Error"]))
    );
    assert_eq!(
        property(&changes, "GitHubIssue", "workgraphInclude"),
        &ElementValue::Bool(false)
    );
}

#[test]
fn generic_issue_current_status_covers_zero_and_one_status_label() {
    for labels in [json!([]), json!([{"name":"ordinary","node_id":"L_1"}])] {
        let mut item = issue("I_generic", "ordinary", false, "open");
        item["labels"] = labels;
        let changes = convert("issues", &issue_event("opened", item));
        assert_eq!(
            property(&changes, "GitHubIssue", "statusLabels"),
            &ElementValue::from(&json!([]))
        );
        assert_eq!(
            property(&changes, "GitHubIssue", "workgraphLabels"),
            &ElementValue::from(&json!([]))
        );
        assert_eq!(
            property(&changes, "GitHubIssue", "currentStatus"),
            &ElementValue::String(Arc::from("none"))
        );
        assert_eq!(
            property(&changes, "GitHubIssue", "workgraphInclude"),
            &ElementValue::Bool(true)
        );
    }

    let mut item = issue("I_generic", "ordinary", false, "open");
    item["labels"] = json!([
        {"name":"ordinary","node_id":"L_1"},
        {"name":"status:Awaiting-Triage","node_id":"L_2"}
    ]);
    let changes = convert("issues", &issue_event("opened", item));
    assert_eq!(
        property(&changes, "GitHubIssue", "statusLabels"),
        &ElementValue::from(&json!(["status:Awaiting-Triage"]))
    );
    assert_eq!(
        property(&changes, "GitHubIssue", "currentStatus"),
        &ElementValue::String(Arc::from("status:Awaiting-Triage"))
    );
}

#[test]
fn generic_issue_workgraph_include_uses_only_exact_exclusion_labels() {
    for (names, expected_labels, expected_include) in [
        (vec![], vec![], true),
        (vec!["workgraph:custom"], vec!["workgraph:custom"], true),
        (vec!["workgraph:ignore"], vec!["workgraph:ignore"], false),
        (vec!["workgraph:error"], vec!["workgraph:error"], false),
        (
            vec!["workgraph:ignore", "workgraph:error"],
            vec!["workgraph:ignore", "workgraph:error"],
            false,
        ),
    ] {
        let mut item = issue("I_generic", "ordinary", false, "open");
        item["labels"] = Value::Array(
            names
                .iter()
                .enumerate()
                .map(|(index, name)| json!({"name":name,"node_id":format!("L_{index}")}))
                .collect(),
        );
        let changes = convert("issues", &issue_event("opened", item));
        assert_eq!(
            property(&changes, "GitHubIssue", "workgraphLabels"),
            &ElementValue::from(&json!(expected_labels))
        );
        assert_eq!(
            property(&changes, "GitHubIssue", "workgraphInclude"),
            &ElementValue::Bool(expected_include)
        );
    }
}

#[test]
fn issue_derived_nodes_emit_boolean_is_open_from_normalized_state() {
    for state in ["open", "OpEn", "OPEN"] {
        let changes = convert(
            "issues",
            &issue_event("labeled", issue("I_generic", "ordinary", false, state)),
        );
        assert_eq!(
            property(&changes, "GitHubIssue", "state"),
            &ElementValue::String(Arc::from("open"))
        );
        assert_eq!(
            property(&changes, "GitHubIssue", "isOpen"),
            &ElementValue::Bool(true)
        );
    }

    for (state, expected) in [("OPEN", true), ("closed", false), ("unknown", false)] {
        let changes = convert(
            "issues",
            &issue_event("edited", issue("I_task", VALIDATION_TASK, true, state)),
        );
        assert_eq!(
            property(&changes, "WorkGraphTask", "isOpen"),
            &ElementValue::Bool(expected)
        );
    }
}

#[test]
fn issue_state_and_state_reason_are_lowercase_without_inventing_reason() {
    let mut item = issue("I_generic", "ordinary", false, "open");
    item["state"] = json!("OpEn");
    item["state_reason"] = json!("NoT_PlAnNeD");
    let changes = convert("issues", &issue_event("opened", item));
    assert_eq!(
        property(&changes, "GitHubIssue", "state"),
        &ElementValue::from(&json!("open"))
    );
    assert_eq!(
        property(&changes, "GitHubIssue", "stateReason"),
        &ElementValue::from(&json!("not_planned"))
    );

    let mut absent = issue("I_absent", "ordinary", false, "open");
    absent.as_object_mut().unwrap().remove("state_reason");
    let changes = convert("issues", &issue_event("opened", absent));
    assert!(!has_property(&changes, "GitHubIssue", "stateReason"));

    let null = issue("I_null", "ordinary", false, "open");
    let changes = convert("issues", &issue_event("opened", null));
    assert_eq!(
        property(&changes, "GitHubIssue", "stateReason"),
        &ElementValue::Null
    );
}

#[test]
fn task_state_is_retained_on_close_and_reopen() {
    let mut closed_task = issue("I_task", VALIDATION_TASK, true, "closed");
    closed_task["state"] = json!("CLOSED");
    closed_task["state_reason"] = json!("COMPLETED");
    let closed = convert("issues", &issue_event("closed", closed_task));
    assert!(closed
        .iter()
        .any(|change| label(change) == "WorkGraphTask" && is_update(change)));
    assert_eq!(
        property(&closed, "WorkGraphTask", "state"),
        &ElementValue::from(&json!("closed"))
    );
    assert_eq!(
        property(&closed, "WorkGraphTask", "stateReason"),
        &ElementValue::from(&json!("completed"))
    );

    let mut reopened_issue = issue("I_task", VALIDATION_TASK, true, "open");
    reopened_issue["title"] = json!("Reopened task");
    reopened_issue["labels"] = json!([
        {"name":"reopened-label","node_id":"L_reopened"}
    ]);
    let reopened = convert("issues", &issue_event("reopened", reopened_issue));
    assert!(reopened
        .iter()
        .any(|change| label(change) == "WorkGraphTask" && is_update(change)));
    assert!(reopened.iter().any(|change| {
        label(change) == "IN_REPOSITORY"
            && id(change) == "IN_REPOSITORY:I_task:R_widgets"
            && is_update(change)
    }));
    assert_eq!(
        property(&reopened, "WorkGraphTask", "title"),
        &ElementValue::from(&json!("Reopened task"))
    );
    assert_eq!(
        property(&reopened, "WorkGraphTask", "labels"),
        &ElementValue::from(&json!(["reopened-label"]))
    );
}

#[test]
fn contract_task_and_assignment_lifecycle_emit_allocator_retractions() {
    for action in ["closed", "deleted"] {
        let conversion = convert_full(
            "issues",
            &issue_event(action, issue("I_task", VALIDATION_TASK, true, "closed")),
        );
        assert!(matches!(
            conversion.allocation,
            Some(AllocationEvent::TaskCancelled { ref task_node_id })
                if task_node_id == "I_task"
        ));
        assert!(conversion.changes.iter().any(|change| {
            label(change) == "WorkGraphTask"
                && if action == "closed" {
                    is_update(change)
                } else {
                    is_delete(change)
                }
        }));
    }

    let revised_body = assignment_body("validator-2");
    let mut revision = comment_event("edited", &revised_body, "open", true, "IC_assignment");
    revision["changes"] = json!({"body": {"from": ASSIGNMENT}});
    let revision = convert_full("issue_comment", &revision);
    assert!(revision.changes.iter().any(|change| {
        id(change) == "ASSIGNED_TO:IC_assignment:workgraph-agent:issue-validator"
            && label(change) == "ASSIGNED_TO"
            && is_delete(change)
    }));
    assert_eq!(
        relation_endpoints(
            &revision.changes,
            "ASSIGNED_TO",
            "ASSIGNED_TO:IC_assignment:workgraph-agent:validator-2",
        ),
        ("IC_assignment", "workgraph-agent:validator-2")
    );
    assert!(matches!(
        revision.allocation,
        Some(AllocationEvent::Comment {
            artifact: Some(AllocationArtifact::Assignment {
                trusted: true,
                ref agent_id,
                ..
            }),
            ..
        }) if agent_id == "validator-2"
    ));

    let malformed = "WorkGraphTaskAssignment/v1\n\n```json\n{}\n```\n";
    let mut invalid = comment_event("edited", malformed, "open", true, "IC_assignment");
    invalid["changes"] = json!({"body": {"from": revised_body}});
    let invalid = convert_full("issue_comment", &invalid);
    assert!(invalid
        .changes
        .iter()
        .any(|change| label(change) == "WorkGraphTaskAssignment" && is_delete(change)));
    assert!(invalid
        .changes
        .iter()
        .any(|change| label(change) == "WorkGraphError" && is_insert(change)));
    assert!(matches!(
        invalid.allocation,
        Some(AllocationEvent::Comment { artifact: None, .. })
    ));

    let deleted = convert_full(
        "issue_comment",
        &comment_event("deleted", ASSIGNMENT, "open", true, "IC_assignment"),
    );
    for expected in [
        "ASSIGNMENT_FOR",
        "ASSIGNED_TO",
        "COMMENT_ON",
        "WorkGraphTaskAssignment",
    ] {
        assert!(deleted
            .changes
            .iter()
            .any(|change| label(change) == expected && is_delete(change)));
    }
    assert!(matches!(
        deleted.allocation,
        Some(AllocationEvent::Comment { artifact: None, .. })
    ));
}

#[test]
fn retained_task_edits_update_properties_and_repository_relation() {
    let mut payload = issue_event("edited", issue("I_task", REQUEST_INFO_TASK, true, "open"));
    payload["changes"] = json!({"body":{"from":VALIDATION_TASK}});
    let changes = convert("issues", &payload);
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphTask" && is_update(change)));
    assert!(changes.iter().any(|change| {
        label(change) == "IN_REPOSITORY"
            && id(change) == "IN_REPOSITORY:I_task:R_widgets"
            && is_update(change)
    }));
    assert_eq!(
        property(&changes, "WorkGraphTask", "taskType"),
        &ElementValue::from(&json!("request-info"))
    );
}

#[test]
fn task_delete_removes_task_repository_and_parent_relation() {
    let changes = convert(
        "issues",
        &issue_event("deleted", issue("I_task", VALIDATION_TASK, true, "closed")),
    );
    for (expected_id, expected_label) in [
        ("TASK_FOR:42", "TASK_FOR"),
        ("IN_REPOSITORY:I_task:R_widgets", "IN_REPOSITORY"),
        ("I_task", "WorkGraphTask"),
    ] {
        assert!(changes.iter().any(|change| {
            id(change) == expected_id && label(change) == expected_label && is_delete(change)
        }));
    }
}

#[test]
fn issue_type_transitions_replace_node_kinds() {
    let mut to_task = issue_event("typed", issue("I_task", VALIDATION_TASK, true, "open"));
    to_task["type"] = json!({"node_id":TASK_TYPE_ID,"name":TASK_TYPE_NAME});
    let changes = convert("issues", &to_task);
    let transition: Vec<_> = changes
        .iter()
        .filter(|change| {
            matches!(
                label(change),
                "GitHubIssue" | "WorkGraphTask" | "IN_REPOSITORY"
            )
        })
        .map(|change| {
            (
                label(change),
                is_insert(change),
                is_update(change),
                is_delete(change),
            )
        })
        .collect();
    assert_eq!(
        transition,
        vec![
            ("WorkGraphTask", false, true, false),
            ("IN_REPOSITORY", false, true, false),
        ]
    );

    let mut from_task = issue_event("untyped", issue("I_task", "ordinary", false, "open"));
    from_task["type"] = json!({"node_id":TASK_TYPE_ID,"name":TASK_TYPE_NAME});
    let changes = convert("issues", &from_task);
    let transition: Vec<_> = changes
        .iter()
        .filter(|change| {
            matches!(
                label(change),
                "TASK_FOR" | "WorkGraphTask" | "GitHubIssue" | "IN_REPOSITORY"
            )
        })
        .map(|change| {
            (
                label(change),
                is_insert(change),
                is_update(change),
                is_delete(change),
            )
        })
        .collect();
    assert_eq!(
        transition,
        vec![
            ("TASK_FOR", false, false, true),
            ("GitHubIssue", false, true, false),
            ("IN_REPOSITORY", false, true, false),
        ]
    );
}

#[tokio::test]
async fn untyped_transition_nulls_all_task_only_properties() {
    let query = generic_issue_query(Some("issue.taskType = 'validate-issue'")).await;
    let generic_query = generic_issue_query(None).await;
    let task = issue_event("opened", issue("I_task", VALIDATION_TASK, true, "open"));
    process_changes(&query, convert("issues", &task)).await;
    process_changes(&generic_query, convert("issues", &task)).await;

    let mut untyped = issue_event("untyped", issue("I_task", "ordinary", false, "open"));
    untyped["type"] = json!({"node_id":TASK_TYPE_ID,"name":TASK_TYPE_NAME});
    let changes = convert("issues", &untyped);
    for key in ["taskType", "inputs", "issueTypeId", "issueTypeName"] {
        assert_eq!(
            property(&changes, "GitHubIssue", key),
            &ElementValue::Null,
            "{key} was not cleared"
        );
    }
    let results = process_changes(&query, changes).await;
    assert_eq!(additions(&results), 0);
    assert_eq!(removals(&results), 0);

    let first_generic = process_changes(&generic_query, convert("issues", &untyped)).await;
    assert_eq!(additions(&first_generic), 1);
    assert_eq!(removals(&first_generic), 0);
    let repeated = process_changes(&query, convert("issues", &untyped)).await;
    assert_eq!(additions(&repeated), 0);
    assert_eq!(removals(&repeated), 0);
    let repeated_generic = process_changes(&generic_query, convert("issues", &untyped)).await;
    assert_eq!(additions(&repeated_generic), 0);
    assert_eq!(removals(&repeated_generic), 0);
}

#[test]
fn unrelated_typed_and_untyped_events_only_update_generic_issue() {
    for action in ["typed", "untyped"] {
        let mut payload = issue_event(action, issue("I_generic", "ordinary", false, "open"));
        payload["type"] = json!({"node_id":"IT_unrelated","name":"Feature"});
        if action == "typed" {
            payload["issue"]["type"] = payload["type"].clone();
        }
        let changes = convert("issues", &payload);
        assert!(changes
            .iter()
            .any(|change| label(change) == "GitHubIssue" && is_update(change)));
        assert!(!changes
            .iter()
            .any(|change| { matches!(label(change), "WorkGraphTask" | "TASK_FOR") }));
    }
}

#[test]
fn body_edits_switch_between_task_and_error() {
    let mut malformed = issue_event("edited", issue("I_task", "{}", true, "open"));
    malformed["changes"] = json!({"body":{"from":VALIDATION_TASK}});
    let changes = convert("issues", &malformed);
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphTask" && is_delete(change)));
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphError"));
    assert_eq!(
        property(&changes, "WorkGraphError", "isOpen"),
        &ElementValue::Bool(true)
    );

    let mut repaired = issue_event("edited", issue("I_task", VALIDATION_TASK, true, "open"));
    repaired["changes"] = json!({"body":{"from":"{}"}});
    let changes = convert("issues", &repaired);
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphError" && is_delete(change)));
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphTask"));
}

#[tokio::test]
async fn malformed_body_repair_preserves_task_for_identity() {
    let query = task_path_query().await;
    let child = issue("I_task", VALIDATION_TASK, true, "open");
    let parent = issue("I_parent", "parent", false, "open");
    let initial = process_changes(
        &query,
        convert(
            "sub_issues",
            &sub_issue_event("sub_issue_added", child, parent),
        ),
    )
    .await;
    assert_eq!(additions(&initial), 1);

    let mut malformed = issue_event("edited", issue("I_task", "{}", true, "open"));
    malformed["changes"] = json!({"body":{"from":VALIDATION_TASK}});
    let malformed_changes = convert("issues", &malformed);
    assert!(!malformed_changes
        .iter()
        .any(|change| label(change) == "TASK_FOR"));
    let malformed_results = process_changes(&query, malformed_changes).await;
    assert_eq!(removals(&malformed_results), 1);

    let mut repaired = issue_event("edited", issue("I_task", VALIDATION_TASK, true, "open"));
    repaired["changes"] = json!({"body":{"from":"{}"}});
    let repaired_changes = convert("issues", &repaired);
    assert!(!repaired_changes
        .iter()
        .any(|change| label(change) == "TASK_FOR"));
    let repaired_results = process_changes(&query, repaired_changes).await;
    assert_eq!(additions(&repaired_results), 1);
    assert_eq!(removals(&repaired_results), 0);

    let repeated = process_changes(&query, convert("issues", &repaired)).await;
    assert_eq!(additions(&repeated), 0);
    assert_eq!(removals(&repeated), 0);
}

#[tokio::test]
async fn malformed_link_reparent_then_repair_uses_latest_parent() {
    let old_query = task_parent_query("I_parent_1").await;
    let new_query = task_parent_query("I_parent_2").await;
    for parent_id in ["I_parent_1", "I_parent_2"] {
        let mut parent = issue(parent_id, "parent", false, "open");
        parent["repository_url"] = json!("https://api.github.com/repos/acme/parents");
        let opened = json!({
            "action":"opened","organization":org(),
            "repository":repo("parents"),"issue":parent
        });
        process_changes(&old_query, convert("issues", &opened)).await;
        process_changes(&new_query, convert("issues", &opened)).await;
    }

    let malformed = issue("I_task", "{}", true, "open");
    let linked = sub_issue_event(
        "sub_issue_added",
        malformed.clone(),
        issue("I_parent_1", "parent", false, "open"),
    );
    let linked_changes = convert("sub_issues", &linked);
    let linked_relation = linked_changes
        .iter()
        .find(|change| label(change) == "TASK_FOR")
        .expect("malformed child still links");
    assert_eq!(id(linked_relation), "TASK_FOR:42");
    assert!(linked_changes
        .iter()
        .any(|change| label(change) == "WorkGraphError"));
    process_changes(&old_query, convert("sub_issues", &linked)).await;
    process_changes(&new_query, convert("sub_issues", &linked)).await;

    let reparented = sub_issue_event(
        "sub_issue_added",
        malformed,
        issue("I_parent_2", "parent", false, "open"),
    );
    let reparented_changes = convert("sub_issues", &reparented);
    let relation = reparented_changes
        .iter()
        .find(|change| label(change) == "TASK_FOR")
        .expect("malformed reparent relation");
    assert_eq!(id(relation), "TASK_FOR:42");
    let SourceChange::Update {
        element: Element::Relation { out_node, .. },
    } = relation
    else {
        panic!("reparent uses an idempotent relation update");
    };
    assert_eq!(out_node.element_id.as_ref(), "I_parent_2");
    process_changes(&old_query, convert("sub_issues", &reparented)).await;
    process_changes(&new_query, convert("sub_issues", &reparented)).await;

    let mut repaired = issue_event("edited", issue("I_task", VALIDATION_TASK, true, "open"));
    repaired["changes"] = json!({"body":{"from":"{}"}});
    let repaired_changes = convert("issues", &repaired);
    assert!(!repaired_changes
        .iter()
        .any(|change| label(change) == "TASK_FOR"));
    let old_results = process_changes(&old_query, convert("issues", &repaired)).await;
    let new_results = process_changes(&new_query, convert("issues", &repaired)).await;
    assert_eq!(additions(&old_results), 0);
    assert_eq!(removals(&old_results), 0);
    assert_eq!(additions(&new_results), 1);
    assert_eq!(removals(&new_results), 0);

    let repeated = process_changes(&new_query, convert("issues", &repaired)).await;
    assert_eq!(additions(&repeated), 0);
    assert_eq!(removals(&repeated), 0);
}

#[test]
fn sub_issue_add_upserts_task_parent_and_relations() {
    let payload = sub_issue_event(
        "sub_issue_added",
        issue("I_task", VALIDATION_TASK, true, "closed"),
        issue("I_parent", "parent", false, "open"),
    );
    let changes = convert("sub_issues", &payload);
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphTask" && is_update(change)));
    assert!(changes.iter().any(|change| {
        label(change) == "IN_REPOSITORY"
            && id(change) == "IN_REPOSITORY:I_task:R_widgets"
            && is_update(change)
    }));
    assert!(changes.iter().any(|change| {
        label(change) == "GitHubIssue" && id(change) == "I_parent" && is_update(change)
    }));
    assert!(changes.iter().any(|change| {
        label(change) == "TASK_FOR" && id(change) == "TASK_FOR:42" && is_update(change)
    }));
}

#[test]
fn sub_issue_delivery_variants_are_stable_and_removal_is_payload_only() {
    let child = issue("I_task", VALIDATION_TASK, true, "open");
    let parent = issue("I_parent", "parent", false, "open");
    for action in ["sub_issue_added", "parent_issue_added"] {
        let changes = convert(
            "sub_issues",
            &sub_issue_event(action, child.clone(), parent.clone()),
        );
        assert!(changes
            .iter()
            .any(|change| id(change) == "TASK_FOR:42" && is_update(change)));
    }

    for action in ["sub_issue_removed", "parent_issue_removed"] {
        let changes = convert(
            "sub_issues",
            &sub_issue_event(action, child.clone(), parent.clone()),
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(id(&changes[0]), "TASK_FOR:42");
        assert!(is_delete(&changes[0]));
    }
}

#[test]
fn schema_minimal_sub_issue_payloads_never_fail_conversion() {
    let child = issue("I_task", VALIDATION_TASK, true, "open");
    let mut parent = issue("I_parent", "parent", false, "open");
    parent["repository_url"] = json!("https://api.github.com/repos/acme/parents");
    let fixtures = [
        json!({
            "action":"parent_issue_added","organization":org(),
            "repository":repo("widgets"),"sub_issue":child
        }),
        json!({
            "action":"parent_issue_removed","organization":org(),
            "repository":repo("widgets"),"sub_issue":issue(
                "I_task", VALIDATION_TASK, true, "open"
            )
        }),
        json!({
            "action":"sub_issue_added","organization":org(),
            "repository":repo("parents"),"parent_issue":parent
        }),
        json!({
            "action":"sub_issue_removed","organization":org(),
            "sub_issue_id":42,
            "repository":repo("parents"),"parent_issue":issue(
                "I_parent", "parent", false, "open"
            )
        }),
    ];
    for (index, fixture) in fixtures.into_iter().enumerate() {
        let changes = Converter::new("gh", "acme", &task_type(), 1)
            .convert("sub_issues", &fixture)
            .expect("schema-valid asymmetric payload must not produce a 422")
            .unwrap()
            .changes;
        match index {
            0 => assert!(changes
                .iter()
                .any(|change| label(change) == "WorkGraphTask" && is_update(change))),
            1 => {
                assert_eq!(changes.len(), 1);
                assert_eq!(id(&changes[0]), "TASK_FOR:42");
                assert!(is_delete(&changes[0]));
            }
            2 => assert!(changes.is_empty()),
            3 => {
                assert_eq!(changes.len(), 1);
                assert_eq!(id(&changes[0]), "TASK_FOR:42");
                assert!(is_delete(&changes[0]));
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn sub_issue_removal_without_optional_child_identity_is_noop() {
    let mut parent = issue("I_parent", "parent", false, "open");
    parent["repository_url"] = json!("https://api.github.com/repos/acme/parents");
    let payload = json!({
        "action":"sub_issue_removed","organization":org(),
        "repository":repo("parents"),"parent_issue_id":42,
        "parent_issue":parent
    });
    let changes = Converter::new("gh", "acme", &task_type(), 1)
        .convert("sub_issues", &payload)
        .expect("schema-valid child-less removal must not produce a 422")
        .unwrap()
        .changes;
    assert!(changes.is_empty());
}

#[test]
fn optional_sub_issue_repositories_do_not_gate_nodes_or_task_for() {
    let child = issue("I_task", VALIDATION_TASK, true, "open");
    let mut parent = issue("I_parent", "parent", false, "open");
    parent["repository_url"] = json!("https://api.github.com/repos/acme/parents");

    let parent_action = json!({
        "action":"parent_issue_added","organization":org(),
        "repository":repo("widgets"),"sub_issue":child,"parent_issue":parent
    });
    let changes = convert("sub_issues", &parent_action);
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphTask" && is_update(change)));
    assert!(changes.iter().any(|change| {
        label(change) == "TASK_FOR" && id(change) == "TASK_FOR:42" && is_update(change)
    }));
    assert!(!changes.iter().any(|change| {
        label(change) == "IN_REPOSITORY" && id(change) == "IN_REPOSITORY:I_parent:R_widgets"
    }));

    let mut sub_parent = issue("I_parent", "parent", false, "open");
    sub_parent["repository_url"] = json!("https://api.github.com/repos/acme/parents");
    let sub_action = json!({
        "action":"sub_issue_added","organization":org(),
        "repository":repo("parents"),"sub_issue":issue(
            "I_task", VALIDATION_TASK, true, "open"
        ),"parent_issue":sub_parent
    });
    let changes = convert("sub_issues", &sub_action);
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphTask" && is_update(change)));
    assert!(changes.iter().any(|change| {
        label(change) == "TASK_FOR" && id(change) == "TASK_FOR:42" && is_update(change)
    }));
    assert!(!changes.iter().any(|change| {
        label(change) == "IN_REPOSITORY" && id(change) == "IN_REPOSITORY:I_task:R_parents"
    }));

    for action in ["parent_issue_removed", "sub_issue_removed"] {
        let payload = if action.starts_with("parent_issue_") {
            json!({
                "action":action,"organization":org(),
                "sub_issue_id":42,
                "repository":repo("widgets"),
                "sub_issue":issue("I_task", VALIDATION_TASK, true, "open")
            })
        } else {
            json!({
                "action":action,"organization":org(),
                "sub_issue_id":42,
                "repository":repo("parents"),
                "parent_issue":issue("I_parent", "parent", false, "open"),
                "sub_issue":issue("I_task", VALIDATION_TASK, true, "open")
            })
        };
        let changes = convert("sub_issues", &payload);
        assert!(changes.iter().any(|change| {
            label(change) == "TASK_FOR" && id(change) == "TASK_FOR:42" && is_delete(change)
        }));
    }
}

#[test]
fn same_repository_url_allows_authoritative_top_level_fallback() {
    let child = issue("I_task", VALIDATION_TASK, true, "open");
    let parent = issue("I_parent", "parent", false, "open");
    let payload = json!({
        "action":"sub_issue_added","organization":org(),
        "repository":repo("widgets"),"sub_issue":child,"parent_issue":parent
    });
    let changes = convert("sub_issues", &payload);
    assert!(changes.iter().any(|change| {
        label(change) == "IN_REPOSITORY"
            && id(change) == "IN_REPOSITORY:I_task:R_widgets"
            && is_update(change)
    }));
}

#[test]
fn sub_issue_and_issue_delivery_order_converge_on_same_ids() {
    let child = issue("I_task", VALIDATION_TASK, true, "open");
    let parent = issue("I_parent", "parent", false, "open");
    let issue_changes = convert("issues", &issue_event("opened", child.clone()));
    let sub_changes = convert(
        "sub_issues",
        &sub_issue_event("sub_issue_added", child, parent),
    );
    assert!(issue_changes.iter().any(|change| id(change) == "I_task"));
    assert!(sub_changes.iter().any(|change| id(change) == "I_task"));
    assert!(sub_changes.iter().any(|change| id(change) == "TASK_FOR:42"));
}

#[tokio::test]
async fn task_delivery_orders_and_repeats_emit_one_add_and_no_removal() {
    for action in ["opened", "typed"] {
        for sub_issue_first in [true, false] {
            let query = task_path_query().await;
            let child = issue("I_task", VALIDATION_TASK, true, "open");
            let parent = issue("I_parent", "parent", false, "open");
            let sub_issue = sub_issue_event("sub_issue_added", child.clone(), parent.clone());
            let mut issue_delivery = issue_event(action, child.clone());
            if action == "typed" {
                let generic = issue_event("opened", issue("I_task", "ordinary", false, "open"));
                assert_eq!(
                    additions(&process_changes(&query, convert("issues", &generic)).await),
                    0
                );
                issue_delivery["type"] = json!({"node_id":TASK_TYPE_ID,"name":TASK_TYPE_NAME});
            }

            let deliveries = if sub_issue_first {
                [
                    ("sub_issues", sub_issue.clone()),
                    ("issues", issue_delivery.clone()),
                ]
            } else {
                [
                    ("issues", issue_delivery.clone()),
                    ("sub_issues", sub_issue.clone()),
                ]
            };
            let mut all_results = Vec::new();
            for (event, payload) in &deliveries {
                all_results.extend(process_changes(&query, convert(event, payload)).await);
            }
            assert_eq!(
                additions(&all_results),
                1,
                "{action}, sub_issue_first={sub_issue_first}"
            );
            assert_eq!(removals(&all_results), 0);

            let mut repeated = Vec::new();
            for (event, payload) in deliveries {
                repeated.extend(process_changes(&query, convert(event, &payload)).await);
            }
            assert_eq!(additions(&repeated), 0);
            assert_eq!(removals(&repeated), 0);
        }
    }
}

#[tokio::test]
async fn parent_open_and_sub_issue_orders_emit_one_add() {
    for sub_issue_first in [true, false] {
        let query = generic_issue_query(None).await;
        let child = issue("I_task", VALIDATION_TASK, true, "open");
        let mut parent = issue("I_parent", "parent", false, "open");
        parent["repository_url"] = json!("https://api.github.com/repos/acme/parents");
        let sub_issue = sub_issue_event("sub_issue_added", child, parent.clone());
        let parent_open = json!({
            "action":"opened","organization":org(),
            "repository":repo("parents"),"issue":parent
        });
        let deliveries = if sub_issue_first {
            [
                ("sub_issues", sub_issue.clone()),
                ("issues", parent_open.clone()),
            ]
        } else {
            [
                ("issues", parent_open.clone()),
                ("sub_issues", sub_issue.clone()),
            ]
        };
        let mut first = Vec::new();
        for (event, payload) in &deliveries {
            first.extend(process_changes(&query, convert(event, payload)).await);
        }
        assert_eq!(additions(&first), 1);
        assert_eq!(removals(&first), 0);

        let mut repeated = Vec::new();
        for (event, payload) in deliveries {
            repeated.extend(process_changes(&query, convert(event, &payload)).await);
        }
        assert_eq!(additions(&repeated), 0);
        assert_eq!(removals(&repeated), 0);
    }
}

#[tokio::test]
async fn update_on_missing_task_node_produces_exactly_one_add() {
    for action in ["opened", "typed"] {
        let query = task_node_query().await;
        if action == "typed" {
            let generic = issue_event("opened", issue("I_task", "ordinary", false, "open"));
            process_changes(&query, convert("issues", &generic)).await;
        }
        let mut payload = issue_event(action, issue("I_task", VALIDATION_TASK, true, "open"));
        if action == "typed" {
            payload["type"] = json!({"node_id":TASK_TYPE_ID,"name":TASK_TYPE_NAME});
        }
        let first = process_changes(&query, convert("issues", &payload)).await;
        assert_eq!(additions(&first), 1);
        assert_eq!(removals(&first), 0);
        let repeated = process_changes(&query, convert("issues", &payload)).await;
        assert_eq!(additions(&repeated), 0);
        assert_eq!(removals(&repeated), 0);
    }
}

#[tokio::test]
async fn parent_transition_reuses_one_relation_identity() {
    let query = task_path_query().await;
    let child = issue("I_task", VALIDATION_TASK, true, "open");
    let old_parent = issue("I_parent_1", "parent", false, "open");
    let new_parent = issue("I_parent_2", "parent", false, "open");
    let initial = convert(
        "sub_issues",
        &sub_issue_event("sub_issue_added", child.clone(), old_parent),
    );
    assert_eq!(additions(&process_changes(&query, initial).await), 1);

    let reparent = convert(
        "sub_issues",
        &sub_issue_event("sub_issue_added", child, new_parent),
    );
    let relation = reparent
        .iter()
        .find(|change| label(change) == "TASK_FOR")
        .expect("reparent relation update");
    assert_eq!(id(relation), "TASK_FOR:42");
    let SourceChange::Update {
        element: Element::Relation {
            in_node, out_node, ..
        },
    } = relation
    else {
        panic!("reparent must update the stable relation");
    };
    assert_eq!(in_node.element_id.as_ref(), "I_task");
    assert_eq!(out_node.element_id.as_ref(), "I_parent_2");
    let results = process_changes(&query, reparent).await;
    assert_eq!(additions(&results), 1);
    assert_eq!(removals(&results), 1);
}

#[tokio::test]
async fn asymmetric_removal_uses_required_child_database_id() {
    let query = task_path_query().await;
    let child = issue("I_task", VALIDATION_TASK, true, "open");
    let mut parent = issue("I_parent", "parent", false, "open");
    parent["repository_url"] = json!("https://api.github.com/repos/acme/parents");
    let initial = convert(
        "sub_issues",
        &sub_issue_event("sub_issue_added", child, parent.clone()),
    );
    assert_eq!(additions(&process_changes(&query, initial).await), 1);

    let removal = json!({
        "action":"sub_issue_removed","organization":org(),
        "repository":repo("parents"),"parent_issue_id":42,
        "parent_issue":parent,"sub_issue_id":42
    });
    let changes = convert("sub_issues", &removal);
    assert_eq!(changes.len(), 1);
    assert_eq!(id(&changes[0]), "TASK_FOR:42");
    assert!(is_delete(&changes[0]));
    let results = process_changes(&query, changes).await;
    assert_eq!(additions(&results), 0);
    assert_eq!(removals(&results), 1);
}

#[test]
fn task_transfer_obeys_repository_filter() {
    let filter = RepositoryFilter::new("acme", &["widgets".to_string()]).unwrap();
    let mut payload = issue_event("transferred", issue("I_old", VALIDATION_TASK, true, "open"));
    payload["changes"] = json!({
        "new_issue":issue("I_new", VALIDATION_TASK, true, "open"),
        "new_repository":repo("excluded")
    });
    let changes = Converter::new("gh", "acme", &task_type(), 1)
        .with_repository_filter(&filter)
        .convert("issues", &payload)
        .unwrap()
        .unwrap()
        .changes;
    assert!(changes
        .iter()
        .any(|change| id(change) == "I_old" && is_delete(change)));
    assert!(!changes.iter().any(|change| id(change) == "I_new"));
}

#[test]
fn task_close_respects_filter_while_delete_still_tombstones() {
    let filter = RepositoryFilter::new("acme", &["included".to_string()]).unwrap();
    let issue_type = task_type();
    let converter = Converter::new("gh", "acme", &issue_type, 1).with_repository_filter(&filter);
    let closed = issue_event("closed", issue("I_task", VALIDATION_TASK, true, "closed"));
    assert!(converter.convert("issues", &closed).unwrap().is_none());
    let deleted = issue_event("deleted", issue("I_task", VALIDATION_TASK, true, "closed"));
    assert!(converter
        .convert("issues", &deleted)
        .unwrap()
        .unwrap()
        .changes
        .iter()
        .any(|change| label(change) == "WorkGraphTask" && is_delete(change)));
}

#[test]
fn task_comments_continue_while_closed() {
    for state in ["open", "closed"] {
        let ordinary = convert(
            "issue_comment",
            &comment_event("created", "ordinary", state, true, "IC_plain"),
        );
        assert!(ordinary
            .iter()
            .any(|change| { label(change) == "GitHubIssueComment" && id(change) == "IC_plain" }));
        let result = convert(
            "issue_comment",
            &comment_event("created", RESULT, state, true, "IC_result"),
        );
        assert!(result
            .iter()
            .any(|change| { label(change) == "WorkGraphTaskResult" && id(change) == "IC_result" }));
        assert!(result.iter().any(|change| label(change) == "COMMENT_ON"));
        assert!(result.iter().any(|change| label(change) == "RESULT_FOR"));
        assert!(!result.iter().any(|change| label(change) == "WorkGraphTask"));
    }
}

#[tokio::test]
async fn task_assignment_result_acceptance_path_requires_matching_result_revision() {
    let query = accepted_result_query().await;
    let mut observed = Vec::new();
    for (event, payload) in [
        (
            "issues",
            issue_event("opened", issue("I_task", VALIDATION_TASK, true, "open")),
        ),
        (
            "issue_comment",
            comment_event("created", ASSIGNMENT, "open", true, "IC_assignment"),
        ),
        (
            "issue_comment",
            comment_event("created", RESULT, "open", true, "IC_result"),
        ),
        (
            "issue_comment",
            comment_event("created", ACCEPTANCE, "open", true, "IC_acceptance"),
        ),
    ] {
        let changes = convert(event, &payload);
        if event == "issue_comment" {
            assert!(!changes
                .iter()
                .any(|change| label(change) == "GitHubIssueComment"));
        }
        observed.extend(process_changes(&query, changes).await);
    }
    assert_eq!(additions(&observed), 0, "stale digest must not match");

    let result_digest = format!("sha256:{}", hex::encode(Sha256::digest(RESULT)));
    let current_acceptance = acceptance_body("IC_result", &result_digest);
    let mut edit = comment_event("edited", &current_acceptance, "open", true, "IC_acceptance");
    edit["changes"] = json!({"body":{"from":ACCEPTANCE}});
    let changes = convert("issue_comment", &edit);
    assert!(changes
        .iter()
        .any(|change| { label(change) == "WorkGraphTaskResultAcceptance" && is_update(change) }));
    assert!(additions(&process_changes(&query, changes).await) > 0);
}

#[test]
fn specialized_comments_emit_only_their_node_and_relations() {
    for state in ["open", "closed"] {
        for (body, node_label, relation_label, id) in [
            (
                ASSIGNMENT,
                "WorkGraphTaskAssignment",
                "ASSIGNMENT_FOR",
                "IC_assignment",
            ),
            (RESULT, "WorkGraphTaskResult", "RESULT_FOR", "IC_result"),
            (
                ACCEPTANCE,
                "WorkGraphTaskResultAcceptance",
                "ACCEPTS_RESULT",
                "IC_acceptance",
            ),
        ] {
            let changes = convert(
                "issue_comment",
                &comment_event("created", body, state, true, id),
            );
            assert!(changes
                .iter()
                .any(|change| label(change) == node_label && is_insert(change)));
            assert!(changes
                .iter()
                .any(|change| label(change) == relation_label && is_insert(change)));
            assert!(changes
                .iter()
                .any(|change| label(change) == "COMMENT_ON" && is_insert(change)));
            assert!(!changes
                .iter()
                .any(|change| label(change) == "GitHubIssueComment"));
            if node_label == "WorkGraphTaskResult" {
                assert_eq!(
                    property(&changes, node_label, "bodyDigest"),
                    &ElementValue::from(&json!(format!(
                        "sha256:{}",
                        hex::encode(Sha256::digest(RESULT))
                    )))
                );
            }
        }
    }
}

#[test]
fn assignment_agent_ids_map_exactly() {
    for (body, expected_agent_id, id) in [
        (ASSIGNMENT, "issue-validator", "IC_validator_assignment"),
        (
            INFO_REQUEST_ASSIGNMENT,
            "issue-info-requester",
            "IC_info_assignment",
        ),
    ] {
        let changes = convert(
            "issue_comment",
            &comment_event("created", body, "open", true, id),
        );
        assert_eq!(
            property(&changes, "WorkGraphTaskAssignment", "agentId"),
            &ElementValue::from(&json!(expected_agent_id))
        );
        assert!(changes
            .iter()
            .any(|change| label(change) == "ASSIGNMENT_FOR" && is_insert(change)));
    }
}

#[test]
fn specialized_markers_on_ordinary_issues_remain_generic_comments() {
    for (body, id) in [
        (ASSIGNMENT, "IC_assignment"),
        (RESULT, "IC_result"),
        (ACCEPTANCE, "IC_acceptance"),
    ] {
        let changes = convert(
            "issue_comment",
            &comment_event("created", body, "open", false, id),
        );
        assert!(changes
            .iter()
            .any(|change| label(change) == "GitHubIssueComment"));
        assert!(!changes.iter().any(|change| {
            matches!(
                label(change),
                "WorkGraphTaskAssignment" | "WorkGraphTaskResult" | "WorkGraphTaskResultAcceptance"
            )
        }));
    }
}

#[test]
fn result_marker_on_ordinary_issue_stays_ordinary() {
    let changes = convert(
        "issue_comment",
        &comment_event("created", RESULT, "open", false, "IC_ordinary"),
    );
    assert!(changes
        .iter()
        .any(|change| label(change) == "GitHubIssueComment"));
    assert!(!changes
        .iter()
        .any(|change| label(change) == "WorkGraphTaskResult"));
}

#[test]
fn malformed_marked_result_emits_error() {
    for (id, body) in [
        (
            "IC_bad_assignment",
            "WorkGraphTaskAssignment/v1\n\n```json\n{}\n```\n",
        ),
        (
            "IC_bad_result",
            "WorkGraphTaskResult/v1\n\n```json\n{}\n```\n",
        ),
        (
            "IC_bad_acceptance",
            "WorkGraphTaskResultAcceptance/v1\n\n```json\n{}\n```\n",
        ),
    ] {
        let changes = convert(
            "issue_comment",
            &comment_event("created", body, "closed", true, id),
        );
        assert!(changes
            .iter()
            .any(|change| label(change) == "WorkGraphError"));
        assert!(!changes
            .iter()
            .any(|change| label(change) == "GitHubIssueComment"));
    }
}

#[test]
fn comment_edits_replace_ordinary_result_and_error_nodes() {
    let mut to_result = comment_event("edited", RESULT, "closed", true, "IC_edit");
    to_result["changes"] = json!({"body":{"from":"ordinary"}});
    let changes = convert("issue_comment", &to_result);
    assert!(changes
        .iter()
        .any(|change| { label(change) == "GitHubIssueComment" && is_delete(change) }));
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphTaskResult" && is_insert(change)));

    let mut to_error = comment_event(
        "edited",
        "WorkGraphTaskResult/v1\nbad",
        "closed",
        true,
        "IC_edit",
    );
    to_error["changes"] = json!({"body":{"from":RESULT}});
    let changes = convert("issue_comment", &to_error);
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphTaskResult" && is_delete(change)));
    assert!(changes
        .iter()
        .any(|change| label(change) == "WorkGraphError" && is_insert(change)));
}

#[test]
fn edits_reclassify_all_specialized_comment_kinds_and_delete_old_relations() {
    for (from, to, old_label, old_relation, new_label, new_relation) in [
        (
            ASSIGNMENT,
            RESULT,
            "WorkGraphTaskAssignment",
            "ASSIGNMENT_FOR",
            "WorkGraphTaskResult",
            "RESULT_FOR",
        ),
        (
            RESULT,
            ACCEPTANCE,
            "WorkGraphTaskResult",
            "RESULT_FOR",
            "WorkGraphTaskResultAcceptance",
            "ACCEPTS_RESULT",
        ),
    ] {
        let mut edit = comment_event("edited", to, "closed", true, "IC_edit");
        edit["changes"] = json!({"body":{"from":from}});
        let changes = convert("issue_comment", &edit);
        assert!(changes
            .iter()
            .any(|change| label(change) == old_label && is_delete(change)));
        assert!(changes
            .iter()
            .any(|change| label(change) == old_relation && is_delete(change)));
        assert!(changes
            .iter()
            .any(|change| label(change) == new_label && is_insert(change)));
        assert!(changes
            .iter()
            .any(|change| label(change) == new_relation && is_insert(change)));
        assert!(!changes
            .iter()
            .any(|change| label(change) == "GitHubIssueComment"));
    }
}

#[test]
fn deleting_acceptance_removes_target_and_task_relations_without_touching_task() {
    let changes = convert(
        "issue_comment",
        &comment_event("deleted", ACCEPTANCE, "closed", true, "IC_acceptance"),
    );
    for expected in [
        "ACCEPTS_RESULT",
        "COMMENT_ON",
        "WorkGraphTaskResultAcceptance",
    ] {
        assert!(changes
            .iter()
            .any(|change| label(change) == expected && is_delete(change)));
    }
    assert!(!changes
        .iter()
        .any(|change| label(change) == "WorkGraphTask"));
}

#[test]
fn comment_delete_removes_result_relations_and_node() {
    let changes = convert(
        "issue_comment",
        &comment_event("deleted", RESULT, "closed", true, "IC_result"),
    );
    for expected in ["RESULT_FOR", "COMMENT_ON", "WorkGraphTaskResult"] {
        assert!(changes
            .iter()
            .any(|change| label(change) == expected && is_delete(change)));
    }
}

#[test]
fn ordinary_and_result_comment_updates_and_deletes_work_in_every_task_state() {
    for state in ["open", "closed"] {
        let mut ordinary_edit = comment_event("edited", "ordinary after", state, true, "IC_plain");
        ordinary_edit["changes"] = json!({"body":{"from":"ordinary before"}});
        let changes = convert("issue_comment", &ordinary_edit);
        assert!(changes
            .iter()
            .any(|change| label(change) == "GitHubIssueComment" && is_update(change)));
        let changes = convert(
            "issue_comment",
            &comment_event("deleted", "ordinary after", state, true, "IC_plain"),
        );
        assert!(changes
            .iter()
            .any(|change| label(change) == "GitHubIssueComment" && is_delete(change)));

        let mut result_edit = comment_event("edited", RESULT, state, true, "IC_result");
        result_edit["changes"] = json!({"body":{"from":RESULT}});
        let changes = convert("issue_comment", &result_edit);
        assert!(changes
            .iter()
            .any(|change| label(change) == "WorkGraphTaskResult" && is_update(change)));
        let changes = convert(
            "issue_comment",
            &comment_event("deleted", RESULT, state, true, "IC_result"),
        );
        assert!(changes
            .iter()
            .any(|change| label(change) == "WorkGraphTaskResult" && is_delete(change)));
    }
}

#[test]
fn close_and_result_order_both_produce_complete_changes() {
    let close = convert(
        "issues",
        &issue_event("closed", issue("I_task", VALIDATION_TASK, true, "closed")),
    );
    let result = convert(
        "issue_comment",
        &comment_event("created", RESULT, "closed", true, "IC_result"),
    );
    assert!(close.iter().any(|change| label(change) == "WorkGraphTask"));
    assert!(result
        .iter()
        .any(|change| label(change) == "WorkGraphTaskResult"));
}

#[test]
fn multiple_results_have_independent_comment_identities() {
    let first = convert(
        "issue_comment",
        &comment_event("created", RESULT, "open", true, "IC_1"),
    );
    let second = convert(
        "issue_comment",
        &comment_event("created", RESULT, "open", true, "IC_2"),
    );
    assert!(first
        .iter()
        .any(|change| { label(change) == "WorkGraphTaskResult" && id(change) == "IC_1" }));
    assert!(second
        .iter()
        .any(|change| { label(change) == "WorkGraphTaskResult" && id(change) == "IC_2" }));
}

#[test]
fn generic_issues_remain_open_only() {
    let closed = convert(
        "issues",
        &issue_event("closed", issue("I_generic", "ordinary", false, "closed")),
    );
    assert!(closed
        .iter()
        .any(|change| label(change) == "GitHubIssue" && is_delete(change)));
    assert!(Converter::new("gh", "acme", &task_type(), 1)
        .convert(
            "issues",
            &issue_event("edited", issue("I_generic", "ordinary", false, "closed"))
        )
        .unwrap()
        .is_none());
}

#[test]
fn excluded_task_addition_is_ignored_but_removal_converges() {
    let filter = RepositoryFilter::new("acme", &["parents".to_string()]).unwrap();
    let child = issue("I_task", VALIDATION_TASK, true, "open");
    let parent = issue("I_parent", "parent", false, "open");
    let issue_type = task_type();
    let converter = Converter::new("gh", "acme", &issue_type, 1).with_repository_filter(&filter);
    assert!(converter
        .convert(
            "sub_issues",
            &sub_issue_event("sub_issue_added", child.clone(), parent.clone())
        )
        .unwrap()
        .is_none());
    let mut parent_without_child_repo = issue("I_parent", "parent", false, "open");
    parent_without_child_repo["repository_url"] =
        json!("https://api.github.com/repos/acme/parents");
    let no_optional_repo = json!({
        "action":"sub_issue_added","organization":org(),
        "repository":repo("parents"),"sub_issue":child.clone(),
        "parent_issue":parent_without_child_repo
    });
    assert!(converter
        .convert("sub_issues", &no_optional_repo)
        .unwrap()
        .is_none());
    let removal = converter
        .convert(
            "sub_issues",
            &sub_issue_event("sub_issue_removed", child, parent),
        )
        .unwrap()
        .unwrap()
        .changes;
    assert_eq!(id(&removal[0]), "TASK_FOR:42");
}

#[test]
fn config_requires_exact_task_type_id_and_name() {
    let mut config = GitHubWorkGraphSourceConfig {
        organization: "acme".to_string(),
        task_issue_type: task_type(),
        repositories: vec![],
        agent_config: None,
        lease_trust: None,
        workflow_definition: None,
        webhook: WebhookConfig {
            secret: "secret".to_string(),
            lease_validation_token: "validation-token".to_string(),
            ..WebhookConfig::default()
        },
        durability: DurabilityConfig {
            enabled: true,
            capacity_policy: CapacityPolicy::RejectIncoming,
            ..DurabilityConfig::default()
        },
    };
    assert!(config.validate().is_ok());
    config.task_issue_type.id.clear();
    assert!(config.validate().is_err());
    config.task_issue_type = task_type();
    config.task_issue_type.name = " WorkGraphTask".to_string();
    assert!(config.validate().is_err());
    config.task_issue_type = task_type();
    config.webhook.lease_validation_token = config.webhook.secret.clone();
    assert_eq!(
        config.validate().unwrap_err().to_string(),
        "webhook.leaseValidationToken must differ from webhook.secret"
    );
    config.webhook.lease_validation_token = "distinct-validation-token".to_string();
    assert!(config.validate().is_ok());
}

#[tokio::test]
async fn descriptor_rejects_secret_references_that_resolve_to_the_same_value() {
    struct TestSecretResolver;

    #[async_trait::async_trait]
    impl drasi_plugin_sdk::resolver::ValueResolver for TestSecretResolver {
        async fn resolve_to_string(
            &self,
            value: &drasi_plugin_sdk::ConfigValue<String>,
        ) -> Result<String, drasi_plugin_sdk::resolver::ResolverError> {
            match value {
                drasi_plugin_sdk::ConfigValue::Secret { name } if name.starts_with("same-") => {
                    Ok("shared-secret-value".to_string())
                }
                drasi_plugin_sdk::ConfigValue::Secret { name } => Ok(format!("resolved-{name}")),
                _ => Err(drasi_plugin_sdk::resolver::ResolverError::WrongResolverType),
            }
        }
    }

    drasi_plugin_sdk::resolver::register_secret_resolver(Arc::new(TestSecretResolver));
    let config = |secret: &str, token: &str| {
        json!({
            "organization": "acme",
            "taskIssueType": {"id": "IT_test", "name": "WorkGraphTask"},
            "webhook": {
                "secret": {"kind": "Secret", "name": secret},
                "leaseValidationToken": {"kind": "Secret", "name": token}
            },
            "durability": {
                "enabled": true,
                "maxEvents": 1000,
                "capacityPolicy": "RejectIncoming"
            }
        })
    };
    let descriptor = GitHubWorkGraphSourceDescriptor;

    descriptor
        .create_source(
            "distinct-secrets",
            &config("webhook-signing", "lease-validation"),
            false,
        )
        .await
        .expect("distinct resolved secrets must be accepted");
    let error = match descriptor
        .create_source(
            "equal-secrets",
            &config("same-webhook", "same-validation"),
            false,
        )
        .await
    {
        Ok(_) => panic!("equal resolved secrets must be rejected"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "webhook.leaseValidationToken must differ from webhook.secret"
    );
    assert!(!error.to_string().contains("shared-secret-value"));
}

#[test]
fn descriptor_exposes_task_type_and_graph_schema() {
    let descriptor = GitHubWorkGraphSourceDescriptor;
    let schema = descriptor.config_schema_json();
    assert_eq!(descriptor.config_version(), "3.0.0");
    assert!(schema.contains("taskIssueType"));
    assert!(schema.contains("agentConfig"));
    assert!(!schema.contains("workflowDefinition"));
    assert!(!schema.contains("workerConfig"));
    assert!(NODE_LABELS.contains(&"WorkGraphTask"));
    assert!(NODE_LABELS.contains(&"WorkGraphTaskAssignment"));
    assert!(NODE_LABELS.contains(&"WorkGraphTaskResult"));
    assert!(NODE_LABELS.contains(&"WorkGraphTaskResultAcceptance"));
    assert!(NODE_LABELS.contains(&"WorkGraphTaskArtifact"));
    assert!(NODE_LABELS.contains(&"WorkGraphAgent"));
    assert!(NODE_LABELS.contains(&"WorkGraphAgentSlot"));
    assert!(RELATION_LABELS.contains(&"ASSIGNMENT_FOR"));
    assert!(RELATION_LABELS.contains(&"RESULT_FOR"));
    assert!(RELATION_LABELS.contains(&"ACCEPTS_RESULT"));
    assert!(RELATION_LABELS.contains(&"ASSIGNED_TO"));
    assert!(RELATION_LABELS.contains(&"HAS_SLOT"));
    assert!(RELATION_LABELS.contains(&"TASK_FOR"));
    assert!(RELATION_LABELS.contains(&"ARTIFACT_FOR"));
    assert!(
        serde_json::from_value::<crate::descriptor::GitHubWorkGraphSourceConfigDto>(json!({
            "organization": "acme",
            "taskIssueType": {"id": "IT_test", "name": "WorkGraphTask"},
            "workerConfig": {
                "repository": "acme/widgets",
                "ref": "main",
                "path": ".github/workgraph/workers.yaml",
                "token": "legacy-token"
            },
            "webhook": {
                "secret": "webhook-secret",
                "leaseValidationToken": "validation-token"
            }
        }))
        .is_err()
    );
}

#[test]
fn signature_verification_remains_strict() {
    let body = br#"{"action":"opened"}"#;
    assert!(verify_signature(
        b"secret",
        body,
        "sha256=d42142b53efbc7cf5cd20b6e074eb33707e0de3b368f698e6d6f6c824ffb8d37"
    )
    .is_ok());
    assert!(verify_signature(b"secret", body, "sha256=00").is_err());
}

#[test]
fn trust_finalization_only_mutates_protocol_artifacts() {
    let mut ordinary = convert(
        "issue_comment",
        &comment_event(
            "created",
            "Ordinary task comment.",
            "open",
            true,
            "IC_ordinary",
        ),
    );
    assert!(node_property_opt(&ordinary, "IC_ordinary", "trusted").is_none());
    crate::mapping::set_artifact_trusted(&mut ordinary, "IC_ordinary", false);
    assert!(node_property_opt(&ordinary, "IC_ordinary", "trusted").is_none());

    let mut assignment = convert(
        "issue_comment",
        &comment_event("created", ASSIGNMENT, "open", true, "IC_assignment"),
    );
    crate::mapping::set_artifact_trusted(&mut assignment, "IC_assignment", true);
    assert_eq!(
        node_property(&assignment, "IC_assignment", "trusted"),
        &ElementValue::Bool(true)
    );
}

const AGENT_FILE: &str = "version: 1\nagents:\n  - agentId: issue-validator\n    slots: 2\n    leaseDuration: PT15M\n  - agentId: \
                           issue-info-requester\n    slots: \
                           1\n    leaseDuration: PT15M\n";

fn agent_location() -> AgentFileLocation {
    AgentFileLocation {
        repository: "acme/widgets".to_string(),
        r#ref: "main".to_string(),
        path: ".github/workgraph/agents.yaml".to_string(),
    }
}

fn agent_content(text: &str) -> AgentFileContent {
    AgentFileContent {
        text: text.to_string(),
        oid: "blob-oid".to_string(),
    }
}

fn project_agents(text: &str) -> Vec<SourceChange> {
    project_agents_with(text, &BTreeMap::new(), &BTreeMap::new())
}

fn project_agents_with(
    text: &str,
    retiring: &BTreeMap<String, BTreeSet<u32>>,
    removed: &BTreeMap<String, BTreeSet<u32>>,
) -> Vec<SourceChange> {
    let file = parse_agent_file(text).expect("agent file must parse");
    let content = agent_content(text);
    agent_changes(
        "gh",
        1,
        &agent_location(),
        &AgentProjection::Loaded {
            file: &file,
            content: &content,
        },
        retiring,
        removed,
    )
}

fn node_property<'a>(changes: &'a [SourceChange], node_id: &str, key: &str) -> &'a ElementValue {
    changes
        .iter()
        .find_map(|change| match change {
            SourceChange::Insert {
                element: Element::Node { properties, .. },
            }
            | SourceChange::Update {
                element: Element::Node { properties, .. },
            } if id(change) == node_id => properties.get(key),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing {node_id}.{key}"))
}

fn node_property_opt<'a>(
    changes: &'a [SourceChange],
    node_id: &str,
    key: &str,
) -> Option<&'a ElementValue> {
    changes.iter().find_map(|change| match change {
        SourceChange::Insert {
            element: Element::Node { properties, .. },
        }
        | SourceChange::Update {
            element: Element::Node { properties, .. },
        } if id(change) == node_id => properties.get(key),
        _ => None,
    })
}

fn ids_with_label<'a>(changes: &'a [SourceChange], wanted: &str) -> Vec<&'a str> {
    changes
        .iter()
        .filter(|change| label(change) == wanted)
        .map(id)
        .collect()
}

#[test]
fn agent_file_accepts_only_the_strict_version_one_grammar() {
    let file = parse_agent_file(AGENT_FILE).expect("valid agent file");
    assert_eq!(file.version, 1);
    assert_eq!(file.agents.len(), 2);
    assert_eq!(file.agents[0].agent_id, "issue-validator");
    assert_eq!(file.agents[0].slots, 2);
    assert_eq!(file.agents[0].lease_duration, "PT15M");
    assert_eq!(file.agents[0].lease_duration_seconds, 900);
    assert_eq!(
        file.agents[0].slot_ids(),
        vec!["issue-validator/1", "issue-validator/2"]
    );

    for (body, expected) in [
        // Zero agents must never become a silently empty pool.
        ("version: 1\nagents: []\n", agent_error_code::INVALID_AGENT_FILE_PAYLOAD),
        // Unsupported and missing versions.
        (
            "version: 2\nagents:\n  - agentId: w\n    slots: 1\n    leaseDuration: PT1M\n",
            agent_error_code::INVALID_AGENT_FILE_PAYLOAD,
        ),
        ("agents: []\n", agent_error_code::INVALID_AGENT_FILE_YAML),
        // Unknown field.
        (
            "version: 1\nagents:\n  - agentId: w\n    slots: 1\n    leaseDuration: PT1M\n    extra: nope\n",
            agent_error_code::INVALID_AGENT_FILE_YAML,
        ),
        // Wrong types.
        (
            "version: 1\nagents:\n  - agentId: w\n    slots: two\n    leaseDuration: PT1M\n",
            agent_error_code::INVALID_AGENT_FILE_YAML,
        ),
        // The legacy worker/profile entry is not an alias.
        (
            "version: 1\nagents:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: PT1M\n",
            agent_error_code::INVALID_AGENT_FILE_YAML,
        ),
        // Non-positive and unsafe slot counts.
        (
            "version: 1\nagents:\n  - agentId: w\n    slots: 0\n    leaseDuration: PT1M\n",
            agent_error_code::INVALID_AGENT_FILE_PAYLOAD,
        ),
        (
            "version: 1\nagents:\n  - agentId: w\n    slots: 17\n    leaseDuration: PT1M\n",
            agent_error_code::INVALID_AGENT_FILE_PAYLOAD,
        ),
        // Empty and slot-ambiguous agent IDs.
        (
            "version: 1\nagents:\n  - agentId: ''\n    slots: 1\n    leaseDuration: PT1M\n",
            agent_error_code::INVALID_AGENT_FILE_PAYLOAD,
        ),
        (
            "version: 1\nagents:\n  - agentId: a/b\n    slots: 1\n    leaseDuration: PT1M\n",
            agent_error_code::INVALID_AGENT_FILE_PAYLOAD,
        ),
        // Invalid, non-positive, and unsafe durations.
        (
            "version: 1\nagents:\n  - agentId: w\n    slots: 1\n    leaseDuration: 15m\n",
            agent_error_code::INVALID_AGENT_FILE_PAYLOAD,
        ),
        (
            "version: 1\nagents:\n  - agentId: w\n    slots: 1\n    leaseDuration: PT0S\n",
            agent_error_code::INVALID_AGENT_FILE_PAYLOAD,
        ),
        (
            "version: 1\nagents:\n  - agentId: w\n    slots: 1\n    leaseDuration: P2D\n",
            agent_error_code::INVALID_AGENT_FILE_PAYLOAD,
        ),
        // Duplicate agent IDs, and therefore duplicate derived slot IDs.
        (
            "version: 1\nagents:\n  - agentId: w\n    slots: 1\n    leaseDuration: PT1M\n  - agentId: w\n    slots: 1\n    leaseDuration: PT1M\n",
            agent_error_code::INVALID_AGENT_FILE_PAYLOAD,
        ),
    ] {
        let error = parse_agent_file(body).expect_err("agent file must be rejected");
        assert_eq!(error.code, expected, "unexpected code for: {body}");
    }
    let arbitrary = parse_agent_file(
        "version: 1\nagents:\n  - agentId: issue-risk-profiler\n    slots: 1\n    leaseDuration: PT1M\n",
    )
    .expect("custom agent profile names are configuration-defined");
    assert_eq!(arbitrary.agents[0].agent_id, "issue-risk-profiler");
    let maximum_agents = format!(
        "version: 1\nagents:\n{}",
        (0..64)
            .map(|index| format!(
                "  - agentId: agent-{index}\n    slots: 1\n    leaseDuration: PT1S\n"
            ))
            .collect::<String>()
    );
    assert_eq!(
        parse_agent_file(&maximum_agents)
            .expect("64 agents is the configured maximum")
            .agents
            .len(),
        64
    );
    let too_many_agents =
        format!("{maximum_agents}  - agentId: agent-64\n    slots: 1\n    leaseDuration: PT1S\n");
    assert_eq!(
        parse_agent_file(&too_many_agents).unwrap_err().code,
        agent_error_code::INVALID_AGENT_FILE_PAYLOAD
    );
    assert_eq!(
        parse_agent_file(&"x".repeat(crate::agents::MAX_AGENT_FILE_BYTES as usize + 1))
            .unwrap_err()
            .code,
        agent_error_code::AGENT_FILE_TOO_LARGE
    );
    assert_eq!(
        parse_agent_file(
            "version: 1\r\nagents:\r\n  - agentId: issue-validator\r\n    slots: 1\r\n    leaseDuration: PT1M\r\n"
        )
        .unwrap_err()
        .code,
        agent_error_code::INVALID_AGENT_FILE_PAYLOAD
    );
}

#[test]
fn iso8601_lease_durations_reject_ambiguous_and_malformed_forms() {
    for (text, expected) in [
        ("PT15M", Some(900)),
        ("PT1H", Some(3600)),
        ("PT1H30M", Some(5400)),
        ("PT90S", Some(90)),
        ("P1D", Some(86_400)),
        ("P1DT1H", Some(90_000)),
    ] {
        assert_eq!(parse_iso8601_duration_seconds(text), expected, "{text}");
    }
    for text in [
        "", "P", "PT", "15M", "PT15", "P1Y", "P1W", "P1M", "PT1.5M", "PT-5M", "PT15m", "PTM",
        "PT1M1H", "PT1M1M", "P1DT", "PT1S1S",
    ] {
        assert_eq!(parse_iso8601_duration_seconds(text), None, "{text}");
    }
}

#[test]
fn agent_file_location_validates_repository_ref_and_path() {
    assert!(agent_location().validate().is_ok());
    for (repository, git_ref, path) in [
        ("widgets", "main", ".github/workgraph/agents.yaml"),
        ("acme/a/b", "main", ".github/workgraph/agents.yaml"),
        ("acme/widgets", "", ".github/workgraph/agents.yaml"),
        ("acme/widgets", "ma in", ".github/workgraph/agents.yaml"),
        ("acme/widgets", "main", "/absolute.yaml"),
        ("acme/widgets", "main", "../escape.yaml"),
        ("acme/widgets", "main", "a//b.yaml"),
        ("acme/widgets", "main", ""),
    ] {
        let location = AgentFileLocation {
            repository: repository.to_string(),
            r#ref: git_ref.to_string(),
            path: path.to_string(),
        };
        assert!(
            location.validate().is_err(),
            "expected rejection for {repository}@{git_ref}:{path}"
        );
    }
    let location = agent_location();
    assert_eq!(location.owner(), "acme");
    assert_eq!(location.name(), "widgets");
    assert_eq!(location.expression(), "main:.github/workgraph/agents.yaml");
    assert!(location.matches_push("acme/widgets", "refs/heads/main"));
    assert!(location.matches_push("ACME/Widgets", "main"));
    assert!(!location.matches_push("acme/widgets", "refs/heads/other"));
    assert!(!location.matches_push("acme/other", "refs/heads/main"));
}

#[test]
fn agents_project_stable_nodes_slots_and_relations() {
    let changes = project_agents(AGENT_FILE);

    assert_eq!(
        ids_with_label(&changes, "WorkGraphAgent"),
        vec![
            "workgraph-agent:issue-validator",
            "workgraph-agent:issue-info-requester"
        ]
    );
    assert_eq!(
        ids_with_label(&changes, "WorkGraphAgentSlot"),
        vec![
            "workgraph-agent-slot:issue-validator/1",
            "workgraph-agent-slot:issue-validator/2",
            "workgraph-agent-slot:issue-info-requester/1",
        ]
    );
    assert_eq!(
        ids_with_label(&changes, "HAS_SLOT"),
        vec![
            "HAS_SLOT:workgraph-agent:issue-validator:workgraph-agent-slot:issue-validator/1",
            "HAS_SLOT:workgraph-agent:issue-validator:workgraph-agent-slot:issue-validator/2",
            "HAS_SLOT:workgraph-agent:issue-info-requester:workgraph-agent-slot:issue-info-requester/1",
        ]
    );

    let agent = "workgraph-agent:issue-validator";
    assert_eq!(
        node_property(&changes, agent, "agentId"),
        &ElementValue::from(&json!("issue-validator"))
    );
    assert!(node_property_opt(&changes, agent, "agentProfile").is_none());
    assert!(node_property_opt(&changes, agent, "workerId").is_none());
    assert_eq!(
        node_property(&changes, agent, "configuredSlotCount"),
        &ElementValue::Integer(2)
    );
    assert_eq!(
        node_property(&changes, agent, "queueDepth"),
        &ElementValue::Integer(0)
    );
    assert_eq!(
        node_property(&changes, agent, "activeLeaseCount"),
        &ElementValue::Integer(0)
    );
    assert_eq!(
        node_property(&changes, agent, "availableSlotCount"),
        &ElementValue::Integer(2)
    );
    assert_eq!(
        node_property(&changes, agent, "leaseDuration"),
        &ElementValue::from(&json!("PT15M"))
    );
    assert_eq!(
        node_property(&changes, agent, "leaseDurationSeconds"),
        &ElementValue::Integer(900)
    );
    assert_eq!(
        node_property(&changes, agent, "agentFileVersion"),
        &ElementValue::Integer(1)
    );
    // Configuration provenance travels with every projected agent.
    assert_eq!(
        node_property(&changes, agent, "configRepository"),
        &ElementValue::from(&json!("acme/widgets"))
    );
    assert_eq!(
        node_property(&changes, agent, "configRef"),
        &ElementValue::from(&json!("main"))
    );
    assert_eq!(
        node_property(&changes, agent, "configPath"),
        &ElementValue::from(&json!(".github/workgraph/agents.yaml"))
    );
    assert_eq!(
        node_property(&changes, agent, "configBlobOid"),
        &ElementValue::from(&json!("blob-oid"))
    );
    assert_eq!(
        node_property(&changes, agent, "configDigest"),
        &ElementValue::from(&json!(format!(
            "sha256:{}",
            hex::encode(Sha256::digest(AGENT_FILE))
        )))
    );

    let slot = "workgraph-agent-slot:issue-validator/2";
    assert_eq!(
        node_property(&changes, slot, "slotId"),
        &ElementValue::from(&json!("issue-validator/2"))
    );
    assert_eq!(
        node_property(&changes, slot, "slotNumber"),
        &ElementValue::Integer(2)
    );
    assert_eq!(
        node_property(&changes, slot, "agentId"),
        &ElementValue::from(&json!("issue-validator"))
    );
    assert_eq!(
        node_property(&changes, slot, "enabled"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&changes, slot, "retiring"),
        &ElementValue::Bool(false)
    );
    assert_eq!(
        relation_endpoints(
            &changes,
            "HAS_SLOT",
            "HAS_SLOT:workgraph-agent:issue-validator:workgraph-agent-slot:issue-validator/2",
        ),
        (
            "workgraph-agent:issue-validator",
            "workgraph-agent-slot:issue-validator/2",
        )
    );
    assert_eq!(
        node_property(
            &changes,
            "workgraph-agent:issue-info-requester",
            "availableSlotCount",
        ),
        &ElementValue::Integer(1)
    );

    // A valid configuration always clears any previous configuration error.
    assert!(changes
        .iter()
        .any(|change| is_delete(change) && id(change) == "workgraph-error:agent-config"));

    // Re-projecting the same file is byte-identical, so redelivery converges.
    let repeat = project_agents(AGENT_FILE);
    assert_eq!(changes.len(), repeat.len());
    for (first, second) in changes.iter().zip(repeat.iter()) {
        assert_eq!(id(first), id(second));
        assert_eq!(label(first), label(second));
    }
}

#[test]
fn capacity_reduction_retires_excess_slots_without_deleting_them() {
    let reduced = "version: 1\nagents:\n  - agentId: issue-validator\n    slots: 1\n    leaseDuration: PT15M\n";
    let retiring = BTreeMap::from([("issue-validator".to_string(), BTreeSet::from([2, 3]))]);
    let changes = project_agents_with(reduced, &retiring, &BTreeMap::new());

    // Every previously materialized slot stays addressable so an in-flight
    // Lease keeps a valid LEASES_SLOT target.
    assert_eq!(
        ids_with_label(&changes, "WorkGraphAgentSlot"),
        vec![
            "workgraph-agent-slot:issue-validator/1",
            "workgraph-agent-slot:issue-validator/2",
            "workgraph-agent-slot:issue-validator/3",
        ]
    );
    assert!(!changes
        .iter()
        .any(|change| is_delete(change) && label(change) == "WorkGraphAgentSlot"));

    assert_eq!(
        node_property(
            &changes,
            "workgraph-agent-slot:issue-validator/1",
            "enabled"
        ),
        &ElementValue::Bool(true)
    );
    for retired in [
        "workgraph-agent-slot:issue-validator/2",
        "workgraph-agent-slot:issue-validator/3",
    ] {
        assert_eq!(
            node_property(&changes, retired, "enabled"),
            &ElementValue::Bool(false)
        );
        assert_eq!(
            node_property(&changes, retired, "retiring"),
            &ElementValue::Bool(true)
        );
    }
    assert_eq!(
        node_property(
            &changes,
            "workgraph-agent:issue-validator",
            "configuredSlotCount"
        ),
        &ElementValue::Integer(1)
    );

    // Growing back re-enables the same stable slot identities.
    let grown = project_agents_with(AGENT_FILE, &retiring, &BTreeMap::new());
    assert_eq!(
        node_property(&grown, "workgraph-agent-slot:issue-validator/2", "enabled"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&grown, "workgraph-agent-slot:issue-validator/3", "retiring"),
        &ElementValue::Bool(true)
    );
}

#[test]
fn removed_agents_are_deleted_with_their_slots_and_relations() {
    let single = "version: 1\nagents:\n  - agentId: issue-validator\n    slots: 1\n    leaseDuration: PT15M\n";
    let removed = BTreeMap::from([("issue-info-requester".to_string(), BTreeSet::from([1, 2]))]);
    let changes = project_agents_with(single, &BTreeMap::new(), &removed);

    for deleted in [
        "workgraph-agent:issue-info-requester",
        "workgraph-agent-slot:issue-info-requester/1",
        "workgraph-agent-slot:issue-info-requester/2",
        "HAS_SLOT:workgraph-agent:issue-info-requester:workgraph-agent-slot:issue-info-requester/1",
        "HAS_SLOT:workgraph-agent:issue-info-requester:workgraph-agent-slot:issue-info-requester/2",
    ] {
        assert!(
            changes
                .iter()
                .any(|change| is_delete(change) && id(change) == deleted),
            "missing delete for {deleted}"
        );
    }
    assert!(changes
        .iter()
        .any(|change| !is_delete(change) && id(change) == "workgraph-agent:issue-validator"));
}

#[test]
fn rejected_agent_config_emits_an_error_and_never_an_empty_pool() {
    let error = parse_agent_file("version: 1\nagents: []\n").expect_err("must reject");
    let changes = agent_changes(
        "gh",
        1,
        &agent_location(),
        &AgentProjection::Rejected(&error),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    assert_eq!(changes.len(), 1);
    assert_eq!(id(&changes[0]), "workgraph-error:agent-config");
    assert_eq!(label(&changes[0]), "WorkGraphError");
    assert_eq!(
        node_property(&changes, "workgraph-error:agent-config", "errorKind"),
        &ElementValue::from(&json!("invalid-workgraph-agent-config"))
    );
    assert_eq!(
        node_property(&changes, "workgraph-error:agent-config", "errorCode"),
        &ElementValue::from(&json!(agent_error_code::INVALID_AGENT_FILE_PAYLOAD))
    );
    assert_eq!(
        node_property(&changes, "workgraph-error:agent-config", "configPath"),
        &ElementValue::from(&json!(".github/workgraph/agents.yaml"))
    );
    // A rejected configuration must not delete or rewrite the agent pool.
    assert!(!changes
        .iter()
        .any(|change| label(change) == "WorkGraphAgent" || label(change) == "WorkGraphAgentSlot"));
}

fn instant(minute: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, minute, 0).unwrap()
}

fn assignment_event(comment: &str, task: &str, agent: &str, created_at: &str) -> AllocationEvent {
    assignment_event_with(
        comment,
        task,
        agent,
        created_at,
        true,
        TaskType::ValidateIssue,
    )
}

fn assignment_event_with(
    comment: &str,
    task: &str,
    agent: &str,
    created_at: &str,
    trusted: bool,
    task_type: TaskType,
) -> AllocationEvent {
    AllocationEvent::Comment {
        comment_node_id: comment.into(),
        task_node_id: task.into(),
        artifact: Some(AllocationArtifact::Assignment {
            trusted,
            task_type,
            agent_id: agent.into(),
            created_at: created_at.into(),
        }),
    }
}

fn result_event(comment: &str, task: &str, lease_id: &str) -> AllocationEvent {
    result_event_with(comment, task, lease_id, true, TaskType::ValidateIssue)
}

fn result_event_with(
    comment: &str,
    task: &str,
    lease_id: &str,
    reporter_trusted: bool,
    task_type: TaskType,
) -> AllocationEvent {
    AllocationEvent::Comment {
        comment_node_id: comment.into(),
        task_node_id: task.into(),
        artifact: Some(AllocationArtifact::Result {
            reporter_trusted,
            task_type,
            lease_id: lease_id.into(),
            outcome: Outcome::Succeeded,
            body_digest: format!("sha256:{comment}"),
        }),
    }
}

fn feedback_event(comment: &str, task: &str) -> AllocationEvent {
    feedback_event_for(comment, task, "result")
}

fn feedback_event_for(comment: &str, task: &str, result: &str) -> AllocationEvent {
    AllocationEvent::Comment {
        comment_node_id: comment.into(),
        task_node_id: task.into(),
        artifact: Some(AllocationArtifact::Feedback {
            reporter_trusted: true,
            result_comment_node_id: result.into(),
            result_body_digest: format!("sha256:{result}"),
            body_digest: "sha256:feedback".into(),
        }),
    }
}

fn acceptance_event(comment: &str, task: &str) -> AllocationEvent {
    acceptance_event_for(comment, task, "result")
}

fn acceptance_event_for(comment: &str, task: &str, result: &str) -> AllocationEvent {
    AllocationEvent::Comment {
        comment_node_id: comment.into(),
        task_node_id: task.into(),
        artifact: Some(AllocationArtifact::Acceptance {
            reporter_trusted: true,
            result_comment_node_id: result.into(),
            result_body_digest: format!("sha256:{result}"),
            body_digest: "sha256:acceptance".into(),
        }),
    }
}

fn agent_file(text: &str) -> crate::agents::AgentFile {
    parse_agent_file(text).unwrap()
}

fn assert_agent_counts(
    state: &AllocationState,
    delta: &AllocationDelta,
    agent_id: &str,
    queue_depth: usize,
    active_lease_count: usize,
    available_slot_count: usize,
) {
    let runtime = &state.agent_runtime()[agent_id];
    assert_eq!(runtime.queue_depth, queue_depth);
    assert_eq!(runtime.active_lease_count, active_lease_count);
    assert_eq!(runtime.available_slot_count, available_slot_count);

    let changes = allocation_changes("gh", 42, delta, &state.agent_runtime());
    let agent = format!("workgraph-agent:{agent_id}");
    assert_eq!(
        node_property(&changes, &agent, "queueDepth"),
        &ElementValue::Integer(queue_depth as i64)
    );
    assert_eq!(
        node_property(&changes, &agent, "activeLeaseCount"),
        &ElementValue::Integer(active_lease_count as i64)
    );
    assert_eq!(
        node_property(&changes, &agent, "availableSlotCount"),
        &ElementValue::Integer(available_slot_count as i64)
    );
}

#[test]
fn allocator_orders_queue_and_slots_and_fills_capacity() {
    let now = instant(0);
    let mut state = AllocationState::default();
    let agents = agent_file(
        "version: 1\nagents:\n  - agentId: z\n    slots: 2\n    leaseDuration: PT1M\n  - agentId: a\n    slots: 2\n    leaseDuration: PT1M\n",
    );
    state.sync_agents(&agents, now);
    for (index, agent) in ["a", "z", "a", "z"].into_iter().enumerate() {
        let allocated = state.apply(
            assignment_event(
                &format!("blocker-{index}"),
                &format!("blocker-task-{index}"),
                agent,
                "2025-12-31T23:59:00Z",
            ),
            now,
        );
        assert_eq!(allocated.started.len(), 1);
    }
    for event in [
        assignment_event("a-late", "task-a-late", "a", "2026-01-01T00:02:00Z"),
        assignment_event("z-b", "task-z-b", "z", "2026-01-01T00:01:00Z"),
        assignment_event("a-early", "task-a-early", "a", "2026-01-01T00:01:00Z"),
        assignment_event("z-a", "task-z-a", "z", "2026-01-01T00:01:00Z"),
        assignment_event("duplicate", "task-a-early", "z", "2026-01-01T00:00:00Z"),
    ] {
        assert!(state.apply(event, now).started.is_empty());
    }
    let mut started = Vec::new();
    for index in 0..4 {
        started.extend(
            state
                .apply(
                    AllocationEvent::TaskCancelled {
                        task_node_id: format!("blocker-task-{index}"),
                    },
                    instant(2),
                )
                .started,
        );
    }
    let allocation: Vec<_> = started
        .iter()
        .map(|lease| (lease.slot_id.as_str(), lease.task_node_id.as_str()))
        .collect();
    assert_eq!(
        allocation,
        [
            ("a/1", "task-a-early"),
            ("z/1", "task-z-a"),
            ("a/2", "task-a-late"),
            ("z/2", "task-z-b"),
        ]
    );
    assert_eq!(
        started[0].lease_id,
        hex::encode(Sha256::digest(b"task-a-early\0a-early\0\x31"))
    );
    assert!(started.iter().all(|lease| {
        lease.acquired_at == "2026-01-01T00:02:00.000Z"
            && lease.expires_at == "2026-01-01T00:03:00.000Z"
    }));
    assert_eq!(state.active_leases().count(), 4);
    assert_eq!(state.agent_runtime()["a"].available_slot_count, 0);
    assert_eq!(state.agent_runtime()["z"].active_lease_count, 2);
    assert_eq!(
        state
            .active_leases()
            .map(|lease| lease.task_node_id.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        4
    );
    assert_eq!(
        state
            .active_leases()
            .map(|lease| lease.slot_id.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        4
    );
}

#[test]
fn allocator_contract_capacity_gates_leases_not_queue_and_projects_counters() {
    let now = instant(0);
    let mut state = AllocationState::default();
    let synced = state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 2\n    leaseDuration: PT15M\n",
        ),
        now,
    );
    assert_agent_counts(&state, &synced, "agent", 0, 0, 2);

    let first = state.apply(
        assignment_event("assignment-a", "task-a", "agent", "2026-01-01T00:00:00Z"),
        now,
    );
    assert_eq!(first.started[0].slot_id, "agent/1");
    assert_agent_counts(&state, &first, "agent", 0, 1, 1);

    let second = state.apply(
        assignment_event("assignment-b", "task-b", "agent", "2026-01-01T00:01:00Z"),
        now,
    );
    assert_eq!(second.started[0].slot_id, "agent/2");
    assert_agent_counts(&state, &second, "agent", 0, 2, 0);

    let queued = state.apply(
        assignment_event("assignment-c", "task-c", "agent", "2026-01-01T00:02:00Z"),
        now,
    );
    assert!(queued.started.is_empty());
    assert_agent_counts(&state, &queued, "agent", 1, 2, 0);

    let repeated = state.apply(
        assignment_event("assignment-c", "task-c", "agent", "2026-01-01T00:02:00Z"),
        now,
    );
    assert!(repeated.trusted);
    assert!(repeated.started.is_empty());
    let runtime = &state.agent_runtime()["agent"];
    assert_eq!(
        (
            runtime.queue_depth,
            runtime.active_lease_count,
            runtime.available_slot_count,
        ),
        (1, 2, 0)
    );
}

#[test]
fn allocator_contract_closed_task_assignment_is_not_admitted() {
    let now = instant(0);
    let mut state = AllocationState::default();
    state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        now,
    );
    let conversion = convert_full(
        "issue_comment",
        &comment_event("created", ASSIGNMENT, "closed", true, "assignment"),
    );
    assert!(conversion.changes.iter().any(|change| {
        label(change) == "WorkGraphTaskAssignment"
            && id(change) == "assignment"
            && is_insert(change)
    }));

    let delta = state.apply(
        conversion
            .allocation
            .expect("closed task comment must retract allocator state"),
        now,
    );
    assert!(!delta.trusted);
    assert!(delta.started.is_empty());
    assert_eq!(
        state.agent_runtime()["agent"],
        AgentRuntime {
            configured: true,
            configured_slots: 1,
            queue_depth: 0,
            active_lease_count: 0,
            available_slot_count: 1,
            retiring_slots: BTreeSet::new(),
        }
    );
}

#[test]
fn allocator_contract_nonallocatable_assignment_matrix_is_fail_closed() {
    let mut base = AllocationState::default();
    base.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        instant(0),
    );

    let untrusted = Converter::new("gh", "acme", &task_type(), 1)
        .convert(
            "issue_comment",
            &comment_event(
                "created",
                &assignment_body("agent"),
                "open",
                true,
                "untrusted",
            ),
        )
        .unwrap()
        .unwrap()
        .allocation
        .unwrap();
    let mut state = base.clone();
    let delta = state.apply(untrusted, instant(0));
    assert!(!delta.trusted);
    assert!(delta.started.is_empty());
    assert_eq!(state.agent_runtime()["agent"].queue_depth, 0);

    let malformed = convert_full(
        "issue_comment",
        &comment_event(
            "created",
            "WorkGraphTaskAssignment/v1\n\n```json\n{}\n```\n",
            "open",
            true,
            "malformed",
        ),
    )
    .allocation
    .unwrap();
    let mut state = base.clone();
    let delta = state.apply(malformed, instant(0));
    assert!(!delta.trusted);
    assert!(delta.started.is_empty());
    assert_eq!(state.agent_runtime()["agent"].queue_depth, 0);

    let closed = convert_full(
        "issue_comment",
        &comment_event(
            "created",
            &assignment_body("agent"),
            "closed",
            true,
            "closed",
        ),
    )
    .allocation
    .unwrap();
    let mut state = base.clone();
    let delta = state.apply(closed, instant(0));
    assert!(!delta.trusted);
    assert!(delta.started.is_empty());
    assert_eq!(state.agent_runtime()["agent"].queue_depth, 0);

    let cross_task_type = assignment_event_with(
        "cross-task-type",
        "task",
        "agent",
        "2026-01-01T00:00:00Z",
        true,
        TaskType::RequestInfo,
    );
    let mut state = base.clone();
    let delta = state.apply(cross_task_type, instant(0));
    assert!(delta.trusted);
    assert_eq!(delta.started.len(), 1);
    assert_eq!(delta.started[0].task_type, TaskType::RequestInfo);

    let mut state = base;
    let delta = state.apply(
        assignment_event("unknown-agent", "task", "missing", "2026-01-01T00:00:00Z"),
        instant(0),
    );
    assert!(!delta.trusted);
    assert!(delta.started.is_empty());
    assert_eq!(state.agent_runtime()["agent"].queue_depth, 0);
}

#[test]
fn allocator_contract_assignment_revision_never_reuses_a_lease() {
    let mut state = AllocationState::default();
    state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent-a\n    slots: 1\n    leaseDuration: PT15M\n  - agentId: \
             agent-b\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        instant(0),
    );
    let first = state.apply(
        assignment_event("assignment", "task", "agent-a", "2026-01-01T00:00:00Z"),
        instant(0),
    );
    let old_lease = first.started[0].clone();

    let revision = state.apply(
        assignment_event("assignment", "task", "agent-b", "2026-01-01T00:00:00Z"),
        instant(1),
    );
    assert_eq!(revision.ended.as_slice(), std::slice::from_ref(&old_lease));
    assert_eq!(revision.started.len(), 1);
    let revised_lease = revision.started[0].clone();
    assert_eq!(revised_lease.agent_id, "agent-b");
    assert_ne!(revised_lease.lease_id, old_lease.lease_id);

    let stale = state.apply(
        result_event("stale-result", "task", &old_lease.lease_id),
        instant(2),
    );
    assert!(!stale.trusted);
    assert!(stale.ended.is_empty());
    assert_eq!(state.active_leases().next(), Some(&revised_lease));

    let retracted = state.apply(
        AllocationEvent::Comment {
            comment_node_id: "assignment".into(),
            task_node_id: "task".into(),
            artifact: None,
        },
        instant(3),
    );
    assert_eq!(
        retracted.ended.as_slice(),
        std::slice::from_ref(&revised_lease)
    );
    let recreated = state.apply(
        assignment_event("assignment", "task", "agent-a", "2026-01-01T00:00:00Z"),
        instant(4),
    );
    assert_eq!(recreated.started.len(), 1);
    let recreated_lease = recreated.started[0].clone();
    assert_ne!(recreated_lease.lease_id, old_lease.lease_id);
    assert_ne!(recreated_lease.lease_id, revised_lease.lease_id);
    assert_eq!(
        recreated_lease.lease_id,
        hex::encode(Sha256::digest(b"task\0assignment\0\x33"))
    );

    let stale_revision = state.apply(
        result_event("stale-revision-result", "task", &revised_lease.lease_id),
        instant(5),
    );
    assert!(!stale_revision.trusted);
    assert!(stale_revision.ended.is_empty());
    assert_eq!(state.active_leases().next(), Some(&recreated_lease));
}

#[test]
fn exact_result_releases_and_refills_while_stale_result_is_untrusted() {
    let now = instant(0);
    let mut state = AllocationState::default();
    state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        now,
    );
    let first = state.apply(
        assignment_event("assignment-1", "task-1", "agent", "2026-01-01T00:00:00Z"),
        now,
    );
    state.apply(
        assignment_event("assignment-2", "task-2", "agent", "2026-01-01T00:01:00Z"),
        now,
    );
    let lease = first.started[0].clone();

    let stale = state.apply(result_event("stale", "task-1", "wrong"), instant(1));
    assert!(!stale.trusted);
    assert!(stale.ended.is_empty());
    assert_eq!(state.active_leases().next(), Some(&lease));

    let exact = state.apply(
        result_event("result", "task-1", &lease.lease_id),
        instant(2),
    );
    assert!(exact.trusted);
    assert_eq!(exact.ended, [lease]);
    assert_eq!(exact.started.len(), 1);
    assert_eq!(exact.started[0].task_node_id, "task-2");
}

#[test]
fn allocator_contract_result_binding_matrix_preserves_the_current_lease() {
    let mut state = AllocationState::default();
    state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        instant(0),
    );
    let assigned = state.apply(
        assignment_event("assignment-1", "task-1", "agent", "2026-01-01T00:00:00Z"),
        instant(0),
    );
    state.apply(
        assignment_event("assignment-2", "task-2", "agent", "2026-01-01T00:01:00Z"),
        instant(0),
    );
    let current = assigned.started[0].clone();

    for event in [
        result_event_with(
            "untrusted",
            "task-1",
            &current.lease_id,
            false,
            TaskType::ValidateIssue,
        ),
        result_event_with(
            "wrong-task",
            "task-other",
            &current.lease_id,
            true,
            TaskType::ValidateIssue,
        ),
        result_event_with(
            "wrong-type",
            "task-1",
            &current.lease_id,
            true,
            TaskType::RequestInfo,
        ),
        result_event_with(
            "wrong-lease",
            "task-1",
            "not-the-current-lease",
            true,
            TaskType::ValidateIssue,
        ),
    ] {
        let mut candidate = state.clone();
        let rejected = candidate.apply(event, instant(1));
        assert!(!rejected.trusted);
        assert!(rejected.ended.is_empty());
        assert!(rejected.started.is_empty());
        assert_eq!(candidate.active_leases().next(), Some(&current));
    }

    let mut released = state;
    let exact = released.apply(
        result_event("result", "task-1", &current.lease_id),
        instant(1),
    );
    assert!(exact.trusted);
    assert_eq!(exact.ended.as_slice(), std::slice::from_ref(&current));
    assert_eq!(exact.started.len(), 1);
    let replacement = exact.started[0].clone();
    assert_eq!(replacement.task_node_id, "task-2");

    let late = released.apply(
        result_event("late-result", "task-1", &current.lease_id),
        instant(2),
    );
    assert!(!late.trusted);
    assert!(late.ended.is_empty());
    assert_eq!(released.active_leases().next(), Some(&replacement));
}

#[test]
fn allocator_contract_exact_result_projection_deletes_then_refills() {
    let mut state = AllocationState::default();
    state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        instant(0),
    );
    let first = state.apply(
        assignment_event("assignment-1", "task-1", "agent", "2026-01-01T00:00:00Z"),
        instant(0),
    );
    state.apply(
        assignment_event("assignment-2", "task-2", "agent", "2026-01-01T00:01:00Z"),
        instant(0),
    );
    let ended = first.started[0].clone();
    let delta = state.apply(
        result_event("result", "task-1", &ended.lease_id),
        instant(1),
    );
    let started = delta.started[0].clone();
    assert_eq!(started.task_node_id, "task-2");
    assert_eq!(started.slot_id, "agent/1");

    let changes = allocation_changes("gh", 42, &delta, &state.agent_runtime());
    assert_eq!(
        changes
            .iter()
            .map(|change| (label(change), is_delete(change), is_update(change)))
            .collect::<Vec<_>>(),
        vec![
            ("LEASE_FOR", true, false),
            ("LEASES_SLOT", true, false),
            ("WorkGraphTaskLease", true, false),
            ("WorkGraphTaskLease", false, true),
            ("LEASE_FOR", false, true),
            ("LEASES_SLOT", false, true),
            ("WorkGraphAgent", false, true),
        ]
    );
    let started_element = format!("workgraph-lease:task-2:{}", started.lease_id);
    assert_eq!(
        relation_endpoints(
            &changes,
            "LEASE_FOR",
            &format!("LEASE_FOR:{started_element}:task-2"),
        ),
        (started_element.as_str(), "task-2")
    );
    assert_eq!(
        relation_endpoints(
            &changes,
            "LEASES_SLOT",
            &format!("LEASES_SLOT:{started_element}:workgraph-agent-slot:agent/1"),
        ),
        (started_element.as_str(), "workgraph-agent-slot:agent/1",)
    );
    assert_eq!(
        node_property(&changes, &started_element, "assignmentCommentNodeId"),
        &ElementValue::from(&json!("assignment-2"))
    );
    assert_agent_counts(&state, &delta, "agent", 0, 1, 0);
}

#[test]
fn allocator_contract_expiry_reissues_and_rejects_the_old_attempt() {
    let mut state = AllocationState::default();
    state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT1M\n",
        ),
        instant(0),
    );
    let first = state.apply(
        assignment_event("assignment", "task", "agent", "2026-01-01T00:00:00Z"),
        instant(0),
    );
    let attempt_one = first.started[0].clone();
    assert_eq!(attempt_one.acquired_at, "2026-01-01T00:00:00.000Z");
    assert_eq!(attempt_one.expires_at, "2026-01-01T00:01:00.000Z");
    assert!(state
        .active_exact(
            "task",
            &attempt_one.lease_id,
            "assignment",
            "agent",
            "agent/1",
            instant(1),
        )
        .is_none());

    let expired = state.expire(instant(1));
    assert_eq!(expired.ended.as_slice(), std::slice::from_ref(&attempt_one));
    assert_eq!(expired.started.len(), 1);
    let attempt_two = expired.started[0].clone();
    assert_ne!(attempt_two.lease_id, attempt_one.lease_id);
    assert_eq!(
        attempt_two.lease_id,
        hex::encode(Sha256::digest(b"task\0assignment\0\x32"))
    );
    assert_eq!(attempt_two.acquired_at, "2026-01-01T00:01:00.000Z");
    assert_eq!(attempt_two.expires_at, "2026-01-01T00:02:00.000Z");

    let stale = state.apply(
        result_event("stale", "task", &attempt_one.lease_id),
        instant(1),
    );
    assert!(!stale.trusted);
    assert!(stale.ended.is_empty());
    assert_eq!(state.active_leases().next(), Some(&attempt_two));
}

#[test]
fn feedback_requeues_and_acceptance_suppresses_the_replacement() {
    let now = instant(0);
    let mut state = AllocationState::default();
    state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        now,
    );
    let assigned = state.apply(
        assignment_event("assignment", "task", "agent", "2026-01-01T00:00:00Z"),
        now,
    );
    state.apply(
        result_event("result", "task", &assigned.started[0].lease_id),
        instant(1),
    );

    let feedback = state.apply(feedback_event("feedback", "task"), instant(2));
    assert!(feedback.trusted);
    assert_eq!(feedback.started.len(), 1);
    let duplicate = state.apply(feedback_event("feedback-duplicate", "task"), instant(2));
    assert!(duplicate.trusted);
    assert!(duplicate.started.is_empty());
    assert_eq!(state.active_leases().next(), feedback.started.first());
    state.validate().unwrap();
    let acceptance = state.apply(acceptance_event("acceptance", "task"), instant(3));
    assert!(acceptance.trusted);
    assert_eq!(acceptance.ended, feedback.started);
    assert!(acceptance.started.is_empty());
    assert_eq!(state.agent_runtime()["agent"].available_slot_count, 1);

    let deletion = state.apply(
        AllocationEvent::Comment {
            comment_node_id: "acceptance".into(),
            task_node_id: "task".into(),
            artifact: None,
        },
        instant(4),
    );
    assert_eq!(deletion.started.len(), 1);
}

#[test]
fn result_retraction_invalidates_feedback_before_capacity_refill() {
    for edited in [false, true] {
        let now = instant(0);
        let mut state = AllocationState::default();
        state.sync_agents(
            &agent_file(
                "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
            ),
            now,
        );
        let task_a = state.apply(
            assignment_event("assignment-a", "task-a", "agent", "2026-01-01T00:00:00Z"),
            now,
        );
        state.apply(
            assignment_event("assignment-b", "task-b", "agent", "2026-01-01T00:01:00Z"),
            now,
        );
        let lease_a = task_a.started[0].clone();
        let completed = state.apply(
            result_event("result", "task-a", &lease_a.lease_id),
            instant(1),
        );
        let lease_b = completed.started[0].clone();
        let feedback = state.apply(feedback_event("feedback", "task-a"), instant(2));
        assert!(feedback.trusted);
        assert!(feedback.started.is_empty());
        assert_eq!(state.agent_runtime()["agent"].queue_depth, 1);

        let replacement = edited.then(|| AllocationArtifact::Result {
            reporter_trusted: true,
            task_type: TaskType::ValidateIssue,
            lease_id: lease_a.lease_id.clone(),
            outcome: Outcome::Succeeded,
            body_digest: "sha256:edited-result".into(),
        });
        let invalidated = state.apply(
            AllocationEvent::Comment {
                comment_node_id: "result".into(),
                task_node_id: "task-a".into(),
                artifact: replacement,
            },
            instant(3),
        );
        assert!(!invalidated.trusted);
        assert_eq!(
            invalidated.untrusted_feedback,
            BTreeSet::from(["feedback".to_string()])
        );
        assert!(invalidated.started.is_empty());
        assert_eq!(state.agent_runtime()["agent"].queue_depth, 0);
        assert_eq!(state.active_leases().next(), Some(&lease_b));
        let stored = serde_json::to_value(&state).unwrap();
        assert!(stored["comments"].get("feedback").is_none());
        let projected = allocation_changes("gh", 42, &invalidated, &state.agent_runtime());
        assert_eq!(
            node_property(&projected, "feedback", "trusted"),
            &ElementValue::Bool(false)
        );
        state.validate().unwrap();

        let released = state.apply(
            result_event("result-b", "task-b", &lease_b.lease_id),
            instant(4),
        );
        assert_eq!(released.ended, vec![lease_b]);
        assert!(released.started.is_empty());
        assert!(state.active_leases().next().is_none());
    }
}

#[test]
fn result_retraction_invalidates_dependent_acceptance() {
    let now = instant(0);
    let mut state = AllocationState::default();
    state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        now,
    );
    let assigned = state.apply(
        assignment_event("assignment", "task", "agent", "2026-01-01T00:00:00Z"),
        now,
    );
    state.apply(
        result_event("result", "task", &assigned.started[0].lease_id),
        instant(1),
    );
    assert!(
        state
            .apply(acceptance_event("acceptance", "task"), instant(2))
            .trusted
    );

    let invalidated = state.apply(
        AllocationEvent::Comment {
            comment_node_id: "result".into(),
            task_node_id: "task".into(),
            artifact: None,
        },
        instant(3),
    );
    assert_eq!(
        invalidated.untrusted_acceptances,
        BTreeSet::from(["acceptance".to_string()])
    );
    let stored = serde_json::to_value(&state).unwrap();
    assert!(stored["comments"].get("acceptance").is_none());
    let projected = allocation_changes("gh", 42, &invalidated, &state.agent_runtime());
    assert_eq!(
        node_property(&projected, "acceptance", "trusted"),
        &ElementValue::Bool(false)
    );
    state.validate().unwrap();
}

#[test]
fn task_cancellation_deauthorizes_dependents_independent_of_comment_id_order() {
    for (result_id, feedback_id, acceptance_id) in [
        ("a-result", "z-feedback", "z-acceptance"),
        ("z-result", "a-feedback", "a-acceptance"),
    ] {
        let now = instant(0);
        let mut state = AllocationState::default();
        state.sync_agents(
            &agent_file(
                "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
            ),
            now,
        );
        let assigned = state.apply(
            assignment_event("assignment", "task", "agent", "2026-01-01T00:00:00Z"),
            now,
        );
        state.apply(
            result_event(result_id, "task", &assigned.started[0].lease_id),
            instant(1),
        );
        assert!(
            state
                .apply(
                    feedback_event_for(feedback_id, "task", result_id),
                    instant(2),
                )
                .trusted
        );
        assert!(
            state
                .apply(
                    acceptance_event_for(acceptance_id, "task", result_id),
                    instant(3),
                )
                .trusted
        );

        let cancelled = state.apply(
            AllocationEvent::TaskCancelled {
                task_node_id: "task".into(),
            },
            instant(4),
        );
        assert_eq!(
            cancelled.untrusted_assignments,
            BTreeSet::from(["assignment".to_string()])
        );
        assert_eq!(
            cancelled.untrusted_feedback,
            BTreeSet::from([feedback_id.to_string()])
        );
        assert_eq!(
            cancelled.untrusted_acceptances,
            BTreeSet::from([acceptance_id.to_string()])
        );
        let changes = allocation_changes("gh", 42, &cancelled, &state.agent_runtime());
        for artifact_id in ["assignment", feedback_id, acceptance_id] {
            assert_eq!(
                node_property(&changes, artifact_id, "trusted"),
                &ElementValue::Bool(false)
            );
        }
        let stored = serde_json::to_value(&state).unwrap();
        assert_eq!(stored["comments"], json!({}));
        assert_eq!(stored["queue"], json!({}));
        assert!(state.active_leases().next().is_none());
        state.validate().unwrap();
    }
}

#[test]
fn removing_agent_evicts_queued_assignments_and_cleans_up_after_active_release() {
    let now = instant(0);
    let mut state = AllocationState::default();
    state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        now,
    );
    let active = state
        .apply(
            assignment_event(
                "assignment-active",
                "task-active",
                "agent",
                "2026-01-01T00:00:00Z",
            ),
            now,
        )
        .started
        .remove(0);
    assert!(state
        .apply(
            assignment_event(
                "assignment-queued",
                "task-queued",
                "agent",
                "2026-01-01T00:01:00Z",
            ),
            now,
        )
        .started
        .is_empty());

    let removed = state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: replacement\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        instant(1),
    );
    assert_eq!(state.active_leases().next(), Some(&active));
    assert_eq!(
        removed.untrusted_assignments,
        BTreeSet::from(["assignment-queued".to_string()])
    );
    let stored = serde_json::to_value(&state).unwrap();
    assert!(stored["queue"].get("assignment-queued").is_none());
    assert!(stored["comments"].get("assignment-queued").is_none());
    assert!(stored["queue"].get("assignment-active").is_some());
    assert_eq!(state.agent_runtime()["agent"].queue_depth, 0);
    assert_eq!(state.agent_runtime()["agent"].active_lease_count, 1);
    let changes = allocation_changes("gh", 1, &removed, &state.agent_runtime());
    assert_eq!(
        node_property(&changes, "assignment-queued", "trusted"),
        &ElementValue::Bool(false)
    );
    state.validate().unwrap();

    let released = state.apply(
        result_event("result", "task-active", &active.lease_id),
        instant(2),
    );
    assert_eq!(released.ended, vec![active]);
    assert_eq!(
        released.untrusted_assignments,
        BTreeSet::from(["assignment-active".to_string()])
    );
    let stored = serde_json::to_value(&state).unwrap();
    assert!(stored["queue"].get("assignment-active").is_none());
    assert!(stored["comments"].get("assignment-active").is_none());
    assert!(!state.agent_runtime().contains_key("agent"));
    let changes = allocation_changes("gh", 2, &released, &state.agent_runtime());
    assert_eq!(
        node_property(&changes, "assignment-active", "trusted"),
        &ElementValue::Bool(false)
    );
    state.validate().unwrap();
}

#[test]
fn cancellation_expiry_and_capacity_retirement_are_deterministic() {
    let now = instant(0);
    let mut state = AllocationState::default();
    let three = agent_file(
        "version: 1\nagents:\n  - agentId: agent\n    slots: 3\n    leaseDuration: PT1M\n",
    );
    state.sync_agents(&three, now);
    let mut leases = Vec::new();
    for number in 1..=3 {
        leases.extend(
            state
                .apply(
                    assignment_event(
                        &format!("assignment-{number}"),
                        &format!("task-{number}"),
                        "agent",
                        &format!("2026-01-01T00:0{number}:00Z"),
                    ),
                    now,
                )
                .started,
        );
    }
    let one = agent_file(
        "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT1M\n",
    );
    state.sync_agents(&one, now);
    assert_eq!(
        state.agent_runtime()["agent"].retiring_slots,
        BTreeSet::from([2, 3])
    );
    let released = state.apply(
        result_event("result-2", "task-2", &leases[1].lease_id),
        instant(1),
    );
    assert!(released.removed_slots.contains(&("agent".to_string(), 2)));

    let cancelled = state.apply(
        AllocationEvent::TaskCancelled {
            task_node_id: "task-1".into(),
        },
        instant(1),
    );
    assert_eq!(cancelled.ended[0].task_node_id, "task-1");
    assert_eq!(
        cancelled.untrusted_assignments,
        BTreeSet::from(["assignment-1".to_string()])
    );
    let changes = allocation_changes("gh", 1, &cancelled, &state.agent_runtime());
    assert_eq!(
        node_property(&changes, "assignment-1", "trusted"),
        &ElementValue::Bool(false)
    );
    assert!(!state
        .active_leases()
        .any(|lease| lease.task_node_id == "task-1"));

    let expired = state.expire(instant(2));
    let ended: Vec<_> = expired
        .ended
        .iter()
        .map(|lease| lease.lease_id.clone())
        .collect();
    let mut sorted = ended.clone();
    sorted.sort();
    assert_eq!(ended, sorted);
    assert_eq!(expired.started.len(), expired.ended.len());
    state.sync_agents(&three, instant(2));
    let grown = state.apply(
        assignment_event("assignment-4", "task-4", "agent", "2026-01-01T00:04:00Z"),
        instant(2),
    );
    assert_eq!(grown.started[0].slot_id, "agent/2");
    let deleted = state.apply(
        AllocationEvent::Comment {
            comment_node_id: "assignment-4".into(),
            task_node_id: "task-4".into(),
            artifact: None,
        },
        instant(2),
    );
    assert_eq!(deleted.ended[0].task_node_id, "task-4");
    assert!(state.validate().is_ok());
}

#[test]
fn v1_protocols_are_exact_and_trust_is_role_specific() {
    let identity = json!({"id": "U_bot", "login": "bot"});
    assert!(serde_json::from_value::<LeaseTrust>(json!({
        "assigners": [identity.clone()],
        "reporters": [identity.clone()]
    }))
    .is_ok());
    assert!(serde_json::from_value::<LeaseTrust>(json!({
        "dispatchers": [identity.clone()],
        "reporters": [identity]
    }))
    .is_err());
    assert!(matches!(
        classify_comment(ASSIGNMENT),
        CommentClassification::Assignment(_)
    ));
    for body in [
        "WorkGraphTaskAssignment/v2\n\n```json\n{}\n```\n",
        "WorkGraphTaskAssignment/v1\n\n```json\n{\"agentId\":\"issue-validator\",\
         \"agentId\":\"agent\",\"queuePriority\":1}\n```\n",
        "WorkGraphTaskResult/v2\n\n```json\n{}\n```\n",
    ] {
        assert!(matches!(
            classify_comment(body),
            CommentClassification::Invalid(_)
        ));
    }
    let trusted = convert_full(
        "issue_comment",
        &comment_event("created", ASSIGNMENT, "open", true, "assignment"),
    );
    assert!(matches!(
        trusted.allocation,
        Some(AllocationEvent::Comment {
            artifact: Some(AllocationArtifact::Assignment { trusted: true, .. }),
            ..
        })
    ));
    let untrusted = Converter::new("gh", "acme", &task_type(), 1)
        .convert(
            "issue_comment",
            &comment_event("created", ASSIGNMENT, "open", true, "assignment"),
        )
        .unwrap()
        .unwrap();
    assert!(matches!(
        untrusted.allocation,
        Some(AllocationEvent::Comment {
            artifact: Some(AllocationArtifact::Assignment { trusted: false, .. }),
            ..
        })
    ));
}

async fn allocator_fixture(
    source_id: &str,
) -> (
    TempDir,
    Arc<MemoryStateStoreProvider>,
    Arc<RedbWalProvider>,
    Allocator,
) {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(MemoryStateStoreProvider::new());
    let wal = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
    wal.register(source_id, WriteAheadLogConfig::default())
        .await
        .unwrap();
    let allocator = Allocator::new(source_id.into(), store.clone(), wal.clone());
    (tmp, store, wal, allocator)
}

struct DirectiveProjector {
    source_id: String,
    projections: Mutex<VecDeque<VNextAllocatorProjection>>,
    commits: Arc<AtomicUsize>,
}

impl DirectiveProjector {
    fn new(source_id: &str) -> Self {
        Self {
            source_id: source_id.to_string(),
            projections: Mutex::new(VecDeque::new()),
            commits: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn next(&self, projection: VNextAllocatorProjection) {
        self.projections.lock().await.push_back(projection);
    }
}

struct CountingProjectionCommit {
    commits: Arc<AtomicUsize>,
}

#[async_trait]
impl PreparedProjectionCommit for CountingProjectionCommit {
    async fn commit(self: Box<Self>) {
        self.commits.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl WorkGraphProjector for DirectiveProjector {
    async fn prepare(
        &self,
        _inputs: Vec<ProjectionInput>,
        _effective_from: u64,
    ) -> anyhow::Result<PreparedProjection> {
        Ok(PreparedProjection {
            changes: Vec::new(),
            allocator: self
                .projections
                .lock()
                .await
                .pop_front()
                .unwrap_or_default(),
            rejection: None,
            state_changed: true,
            checkpoint: vec![1],
            commit: Box::new(CountingProjectionCommit {
                commits: self.commits.clone(),
            }),
        })
    }

    async fn restore(&self, _checkpoint: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    fn source_id(&self) -> &str {
        &self.source_id
    }
}

struct FailAtWal {
    inner: Arc<RedbWalProvider>,
    calls: AtomicUsize,
    fail_at: usize,
}

#[async_trait]
impl WalProvider for FailAtWal {
    async fn register(&self, source_id: &str, config: WriteAheadLogConfig) -> Result<(), WalError> {
        self.inner.register(source_id, config).await
    }

    async fn append(&self, source_id: &str, event: &SourceChange) -> Result<u64, WalError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == self.fail_at {
            return Err(WalError::StorageError("injected failure".to_string()));
        }
        self.inner.append(source_id, event).await
    }

    async fn read_from(
        &self,
        source_id: &str,
        sequence: u64,
    ) -> Result<Vec<(u64, SourceChange)>, WalError> {
        self.inner.read_from(source_id, sequence).await
    }

    async fn prune_up_to(&self, source_id: &str, sequence: u64) -> Result<u64, WalError> {
        self.inner.prune_up_to(source_id, sequence).await
    }

    async fn head_sequence(&self, source_id: &str) -> Result<u64, WalError> {
        self.inner.head_sequence(source_id).await
    }

    async fn oldest_sequence(&self, source_id: &str) -> Result<Option<u64>, WalError> {
        self.inner.oldest_sequence(source_id).await
    }

    async fn event_count(&self, source_id: &str) -> Result<u64, WalError> {
        self.inner.event_count(source_id).await
    }

    async fn delete_wal(&self, source_id: &str) -> Result<(), WalError> {
        self.inner.delete_wal(source_id).await
    }
}

fn vnext_task_input(source_key: &str, is_open: bool) -> ProjectionInput {
    ProjectionInput::UpsertTask(TaskDocument {
        source_key: source_key.to_string(),
        body: "WorkGraphTask/v3\n\n```json\n{}\n```\n".to_string(),
        is_open,
        state_reason: String::new(),
        parent_source_key: None,
    })
}

fn vnext_artifact_input(source_key: &str, task_source_key: &str, marker: &str) -> ProjectionInput {
    ProjectionInput::UpsertLifecycleArtifact(LifecycleArtifactDocument {
        source_key: source_key.to_string(),
        task_source_key: task_source_key.to_string(),
        body: format!("{marker}\n\n```json\n{{}}\n```\n"),
    })
}

fn vnext_task_binding() -> VNextTaskBinding {
    VNextTaskBinding {
        source_key: "issue-node".to_string(),
        task_id: "task-1".to_string(),
        task_element_id: "workgraph-vnext:task:task-1".to_string(),
    }
}

fn vnext_assignment_binding(source_key: &str, assignment_id: &str) -> VNextAssignmentBinding {
    VNextAssignmentBinding {
        source_key: source_key.to_string(),
        task_source_key: "issue-node".to_string(),
        task_id: "task-1".to_string(),
        assignment_id: assignment_id.to_string(),
        permitted_executors: vec!["agent".to_string()],
    }
}

fn vnext_dispatch_binding(lease_id: &str) -> VNextDispatchBinding {
    VNextDispatchBinding {
        source_key: "dispatch-comment".to_string(),
        task_source_key: "issue-node".to_string(),
        task_id: "task-1".to_string(),
        assignment_id: "assignment-1".to_string(),
        lease_id: lease_id.to_string(),
        executor_id: "agent".to_string(),
        slot_id: "agent/1".to_string(),
    }
}

#[tokio::test]
async fn vnext_allocator_projects_exact_lease_details_and_retains_them_after_dispatch() {
    let source_id = "vnext-allocation";
    let (_tmp, store, wal, allocator) = allocator_fixture(source_id).await;
    allocator
        .sync_agents(
            &agent_location(),
            &agent_file(
                "version: 1\nagents:\n  - agentId: agent-b\n    slots: 1\n    leaseDuration: PT15M\n  - agentId: agent-a\n    slots: 1\n    leaseDuration: PT15M\n",
            ),
            &agent_content("agents"),
            1,
        )
        .await
        .unwrap();
    let projector = DirectiveProjector::new(source_id);
    let task_binding = vnext_task_binding();
    let assignment_binding = VNextAssignmentBinding {
        permitted_executors: vec!["agent-b".to_string(), "agent-a".to_string()],
        ..vnext_assignment_binding("assign-comment", "assignment-1")
    };

    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding.clone()],
            ..VNextAllocatorProjection::default()
        })
        .await;
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_task_input("issue-node", true)],
            2,
            "task-delivery",
        )
        .await
        .unwrap();

    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding.clone()],
            assignments: vec![assignment_binding.clone()],
            ..VNextAllocatorProjection::default()
        })
        .await;
    let before_assignment = wal.head_sequence(source_id).await.unwrap();
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_artifact_input(
                "assign-comment",
                "issue-node",
                "WorkGraphTaskAssign/v1",
            )],
            3,
            "assign-delivery",
        )
        .await
        .unwrap();
    let assignment_changes = wal
        .read_from(source_id, before_assignment + 1)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, change)| change)
        .collect::<Vec<_>>();
    let lease_id = hex::encode(Sha256::digest(b"task-1\0assignment-1\0\x31"));
    let lease_element = format!("workgraph-vnext-lease:{lease_id}");
    for (property_name, expected) in [
        ("leaseId", lease_id.as_str()),
        ("taskId", "task-1"),
        ("assignmentId", "assignment-1"),
        ("executorId", "agent-a"),
        ("slotId", "agent-a/1"),
    ] {
        assert_eq!(
            node_property(&assignment_changes, &lease_element, property_name),
            &ElementValue::from(&json!(expected))
        );
    }
    assert_eq!(
        relation_endpoints(
            &assignment_changes,
            "LEASE_FOR",
            &format!("LEASE_FOR:{lease_element}:workgraph-vnext:task:task-1"),
        ),
        (lease_element.as_str(), "workgraph-vnext:task:task-1")
    );
    for (artifact_name, artifact_id) in [
        ("lease.id", lease_id.as_str()),
        ("lease.assignmentId", "assignment-1"),
        ("lease.executorId", "agent-a"),
        ("lease.slotId", "agent-a/1"),
    ] {
        let artifact_element = format!("workgraph-vnext-artifact:{lease_id}:{artifact_name}");
        assert_eq!(
            node_property(&assignment_changes, &artifact_element, "taskId"),
            &ElementValue::from(&json!("task-1"))
        );
        assert_eq!(
            node_property(&assignment_changes, &artifact_element, "artifactName"),
            &ElementValue::from(&json!(artifact_name))
        );
        assert_eq!(
            node_property(&assignment_changes, &artifact_element, "artifactId"),
            &ElementValue::from(&json!(artifact_id))
        );
        assert_eq!(
            relation_endpoints(
                &assignment_changes,
                "ARTIFACT_FOR",
                &format!("ARTIFACT_FOR:{artifact_element}:workgraph-vnext:task:task-1"),
            ),
            (artifact_element.as_str(), "workgraph-vnext:task:task-1")
        );
    }

    let dispatch_input =
        vnext_artifact_input("dispatch-comment", "issue-node", "WorkGraphTaskDispatch/v1");
    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding.clone()],
            assignments: vec![assignment_binding.clone()],
            dispatches: vec![VNextDispatchBinding {
                executor_id: "agent-b".to_string(),
                slot_id: "agent-a/1".to_string(),
                ..vnext_dispatch_binding(&lease_id)
            }],
        })
        .await;
    let before_rejected_dispatch = wal.head_sequence(source_id).await.unwrap();
    assert!(allocator
        .ingest_vnext(
            &projector,
            vec![dispatch_input.clone()],
            4,
            "bad-dispatch-delivery",
        )
        .await
        .is_err());
    assert_eq!(
        wal.head_sequence(source_id).await.unwrap(),
        before_rejected_dispatch
    );
    assert!(!allocator
        .vnext_origin_completed("bad-dispatch-delivery")
        .await
        .unwrap());

    let dispatch_binding = VNextDispatchBinding {
        executor_id: "agent-a".to_string(),
        slot_id: "agent-a/1".to_string(),
        ..vnext_dispatch_binding(&lease_id)
    };
    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding.clone()],
            assignments: vec![assignment_binding.clone()],
            dispatches: vec![dispatch_binding.clone()],
        })
        .await;
    let before_dispatch = wal.head_sequence(source_id).await.unwrap();
    allocator
        .ingest_vnext(&projector, vec![dispatch_input], 4, "dispatch-delivery")
        .await
        .unwrap();
    let dispatch_changes = wal
        .read_from(source_id, before_dispatch + 1)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, change)| change)
        .collect::<Vec<_>>();
    assert!(dispatch_changes
        .iter()
        .any(|change| { label(change) == "LEASES_SLOT" && is_delete(change) }));
    assert!(!dispatch_changes.iter().any(|change| {
        is_delete(change)
            && matches!(
                label(change),
                "WorkGraphTaskLease" | "LEASE_FOR" | "WorkGraphTaskArtifact" | "ARTIFACT_FOR"
            )
    }));

    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding.clone()],
            assignments: vec![assignment_binding.clone()],
            dispatches: vec![dispatch_binding.clone()],
        })
        .await;
    let before_close = wal.head_sequence(source_id).await.unwrap();
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_task_input("issue-node", false)],
            5,
            "close-after-dispatch",
        )
        .await
        .unwrap();
    let close_changes = wal
        .read_from(source_id, before_close + 1)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, change)| change)
        .collect::<Vec<_>>();
    assert!(!close_changes.iter().any(|change| {
        is_delete(change)
            && matches!(
                label(change),
                "WorkGraphTaskLease" | "LEASE_FOR" | "WorkGraphTaskArtifact" | "ARTIFACT_FOR"
            )
    }));

    let before_restart = wal.head_sequence(source_id).await.unwrap();
    let restarted = Allocator::new(source_id.to_string(), store, wal.clone());
    restarted.recover(6).await.unwrap();
    let restated = wal
        .read_from(source_id, before_restart + 1)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, change)| change)
        .collect::<Vec<_>>();
    assert!(restated
        .iter()
        .any(|change| { id(change) == lease_element && label(change) == "WorkGraphTaskLease" }));
    assert!(restated
        .iter()
        .any(|change| label(change) == "LEASE_FOR" && !is_delete(change)));
    assert_eq!(
        restated
            .iter()
            .filter(|change| label(change) == "WorkGraphTaskArtifact")
            .count(),
        4
    );
    assert!(!restated.iter().any(|change| label(change) == "LEASES_SLOT"));

    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding.clone()],
            assignments: vec![assignment_binding.clone()],
            dispatches: vec![dispatch_binding],
        })
        .await;
    restarted
        .ingest_vnext(
            &projector,
            vec![vnext_task_input("issue-node", true)],
            7,
            "reopen-after-dispatch",
        )
        .await
        .unwrap();

    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding.clone()],
            assignments: vec![assignment_binding],
            ..VNextAllocatorProjection::default()
        })
        .await;
    let before_dispatch_edit = wal.head_sequence(source_id).await.unwrap();
    restarted
        .ingest_vnext(
            &projector,
            vec![vnext_artifact_input(
                "dispatch-comment",
                "issue-node",
                "WorkGraphTaskResult/v1",
            )],
            8,
            "dispatch-edit-away",
        )
        .await
        .unwrap();
    let dispatch_edit = wal
        .read_from(source_id, before_dispatch_edit + 1)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, change)| change)
        .collect::<Vec<_>>();
    let replacement_lease_id = hex::encode(Sha256::digest(b"task-1\0assignment-1\0\x32"));
    let replacement_lease_element = format!("workgraph-vnext-lease:{replacement_lease_id}");
    assert!(dispatch_edit.iter().any(|change| {
        id(change) == lease_element && label(change) == "WorkGraphTaskLease" && is_delete(change)
    }));
    assert!(dispatch_edit.iter().any(|change| {
        id(change) == replacement_lease_element
            && label(change) == "WorkGraphTaskLease"
            && !is_delete(change)
    }));

    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding],
            ..VNextAllocatorProjection::default()
        })
        .await;
    let before_delete = wal.head_sequence(source_id).await.unwrap();
    restarted
        .ingest_vnext(
            &projector,
            vec![ProjectionInput::DeleteLifecycleArtifact {
                source_key: "assign-comment".to_string(),
            }],
            9,
            "assign-delete-delivery",
        )
        .await
        .unwrap();
    let deleted = wal
        .read_from(source_id, before_delete + 1)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, change)| change)
        .collect::<Vec<_>>();
    assert!(deleted.iter().any(|change| {
        id(change) == replacement_lease_element
            && label(change) == "WorkGraphTaskLease"
            && is_delete(change)
    }));
    assert_eq!(
        deleted
            .iter()
            .filter(|change| label(change) == "WorkGraphTaskArtifact" && is_delete(change))
            .count(),
        4
    );
}

#[tokio::test]
async fn vnext_allocator_converges_delayed_assignment_and_cancels_on_edit_or_task_close() {
    let source_id = "vnext-out-of-order";
    let (_tmp, _store, wal, allocator) = allocator_fixture(source_id).await;
    allocator
        .sync_agents(
            &agent_location(),
            &agent_file(
                "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
            ),
            &agent_content("agents"),
            1,
        )
        .await
        .unwrap();
    let projector = DirectiveProjector::new(source_id);
    let assignment_input =
        vnext_artifact_input("assign-comment", "issue-node", "WorkGraphTaskAssign/v1");
    let task_binding = vnext_task_binding();
    let assignment_binding = vnext_assignment_binding("assign-comment", "assignment-1");

    projector.next(VNextAllocatorProjection::default()).await;
    allocator
        .ingest_vnext(
            &projector,
            vec![assignment_input.clone()],
            2,
            "assign-first",
        )
        .await
        .unwrap();
    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding.clone()],
            assignments: vec![assignment_binding],
            ..VNextAllocatorProjection::default()
        })
        .await;
    let before_task = wal.head_sequence(source_id).await.unwrap();
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_task_input("issue-node", true)],
            3,
            "task-second",
        )
        .await
        .unwrap();
    let lease_id = hex::encode(Sha256::digest(b"task-1\0assignment-1\0\x31"));
    let lease_element = format!("workgraph-vnext-lease:{lease_id}");
    assert!(wal
        .read_from(source_id, before_task + 1)
        .await
        .unwrap()
        .iter()
        .any(|(_, change)| {
            id(change) == lease_element
                && label(change) == "WorkGraphTaskLease"
                && !is_delete(change)
        }));

    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding.clone()],
            ..VNextAllocatorProjection::default()
        })
        .await;
    let before_edit = wal.head_sequence(source_id).await.unwrap();
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_artifact_input(
                "assign-comment",
                "issue-node",
                "WorkGraphTaskResult/v1",
            )],
            4,
            "assign-edit-away",
        )
        .await
        .unwrap();
    assert!(wal
        .read_from(source_id, before_edit + 1)
        .await
        .unwrap()
        .iter()
        .any(|(_, change)| {
            id(change) == lease_element
                && label(change) == "WorkGraphTaskLease"
                && is_delete(change)
        }));

    let second_assignment = vnext_assignment_binding("assign-comment-2", "assignment-2");
    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding.clone()],
            assignments: vec![second_assignment.clone()],
            ..VNextAllocatorProjection::default()
        })
        .await;
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_artifact_input(
                "assign-comment-2",
                "issue-node",
                "WorkGraphTaskAssign/v1",
            )],
            5,
            "assign-again",
        )
        .await
        .unwrap();
    let second_lease_id = hex::encode(Sha256::digest(b"task-1\0assignment-2\0\x31"));
    let second_lease_element = format!("workgraph-vnext-lease:{second_lease_id}");
    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task_binding],
            assignments: vec![second_assignment],
            ..VNextAllocatorProjection::default()
        })
        .await;
    let before_close = wal.head_sequence(source_id).await.unwrap();
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_task_input("issue-node", false)],
            6,
            "task-close",
        )
        .await
        .unwrap();
    assert!(wal
        .read_from(source_id, before_close + 1)
        .await
        .unwrap()
        .iter()
        .any(|(_, change)| {
            id(change) == second_lease_element
                && label(change) == "WorkGraphTaskLease"
                && is_delete(change)
        }));
}

#[tokio::test]
async fn vnext_allocator_recovers_partial_lease_wal_without_duplicate_facts() {
    let source_id = "vnext-partial-wal";
    let (_tmp, store, inner, allocator) = allocator_fixture(source_id).await;
    allocator
        .sync_agents(
            &agent_location(),
            &agent_file(
                "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
            ),
            &agent_content("agents"),
            1,
        )
        .await
        .unwrap();
    let projector = DirectiveProjector::new(source_id);
    projector
        .next(VNextAllocatorProjection {
            tasks: vec![vnext_task_binding()],
            ..VNextAllocatorProjection::default()
        })
        .await;
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_task_input("issue-node", true)],
            2,
            "task",
        )
        .await
        .unwrap();
    let commits_before_failure = projector.commits.load(Ordering::SeqCst);
    let before = inner.head_sequence(source_id).await.unwrap();
    let failing = Allocator::new(
        source_id.to_string(),
        store.clone(),
        Arc::new(FailAtWal {
            inner: inner.clone(),
            calls: AtomicUsize::new(0),
            fail_at: 1,
        }),
    );
    projector
        .next(VNextAllocatorProjection {
            tasks: vec![vnext_task_binding()],
            assignments: vec![vnext_assignment_binding("assign-comment", "assignment-1")],
            ..VNextAllocatorProjection::default()
        })
        .await;
    assert!(failing
        .ingest_vnext(
            &projector,
            vec![vnext_artifact_input(
                "assign-comment",
                "issue-node",
                "WorkGraphTaskAssign/v1",
            )],
            3,
            "assign-crash",
        )
        .await
        .is_err());
    assert_eq!(
        projector.commits.load(Ordering::SeqCst),
        commits_before_failure
    );
    let stored = store
        .get(source_id, "allocator:state")
        .await
        .unwrap()
        .unwrap();
    let stored: Value = serde_json::from_slice(&stored).unwrap();
    let pending_len = stored["pending"].as_array().unwrap().len();
    assert!(pending_len > 1);
    assert_eq!(stored["pendingOffset"], json!(1));
    assert_eq!(inner.head_sequence(source_id).await.unwrap(), before + 1);

    let restarted = Allocator::new(source_id.to_string(), store, inner.clone());
    assert_eq!(restarted.vnext_checkpoint().await.unwrap(), vec![1]);
    assert_eq!(
        inner.head_sequence(source_id).await.unwrap(),
        before + pending_len as u64
    );
    let recovered = inner.read_from(source_id, before + 1).await.unwrap();
    let lease_id = hex::encode(Sha256::digest(b"task-1\0assignment-1\0\x31"));
    assert_eq!(
        recovered
            .iter()
            .filter(|(_, change)| {
                id(change) == format!("workgraph-vnext-lease:{lease_id}")
                    && label(change) == "WorkGraphTaskLease"
            })
            .count(),
        1
    );
    assert_eq!(
        recovered
            .iter()
            .filter(|(_, change)| label(change) == "WorkGraphTaskArtifact")
            .count(),
        4
    );
    assert!(restarted
        .vnext_origin_completed("assign-crash")
        .await
        .unwrap());
}

#[tokio::test]
async fn vnext_allocator_converges_dispatch_assignment_task_out_of_order() {
    let source_id = "vnext-reverse-order";
    let (_tmp, _store, wal, allocator) = allocator_fixture(source_id).await;
    allocator
        .sync_agents(
            &agent_location(),
            &agent_file(
                "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
            ),
            &agent_content("agents"),
            1,
        )
        .await
        .unwrap();
    let projector = DirectiveProjector::new(source_id);
    let lease_id = hex::encode(Sha256::digest(b"task-1\0assignment-1\0\x31"));

    projector.next(VNextAllocatorProjection::default()).await;
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_artifact_input(
                "dispatch-comment",
                "issue-node",
                "WorkGraphTaskDispatch/v1",
            )],
            2,
            "dispatch-first",
        )
        .await
        .unwrap();
    projector.next(VNextAllocatorProjection::default()).await;
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_artifact_input(
                "assign-comment",
                "issue-node",
                "WorkGraphTaskAssign/v1",
            )],
            3,
            "assign-second",
        )
        .await
        .unwrap();
    projector
        .next(VNextAllocatorProjection {
            tasks: vec![vnext_task_binding()],
            assignments: vec![vnext_assignment_binding("assign-comment", "assignment-1")],
            dispatches: vec![vnext_dispatch_binding(&lease_id)],
        })
        .await;
    let before = wal.head_sequence(source_id).await.unwrap();
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_task_input("issue-node", true)],
            4,
            "task-third",
        )
        .await
        .unwrap();
    let changes = wal
        .read_from(source_id, before + 1)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, change)| change)
        .collect::<Vec<_>>();
    assert!(changes.iter().any(|change| {
        id(change) == format!("workgraph-vnext-lease:{lease_id}")
            && label(change) == "WorkGraphTaskLease"
            && !is_delete(change)
    }));
    assert!(changes
        .iter()
        .any(|change| label(change) == "LEASE_FOR" && !is_delete(change)));
    assert_eq!(
        changes
            .iter()
            .filter(|change| label(change) == "WorkGraphTaskArtifact")
            .count(),
        4
    );
    assert!(!changes.iter().any(|change| label(change) == "LEASES_SLOT"));
}

#[tokio::test]
async fn vnext_allocator_snapshot_retracts_and_restores_cross_source_duplicate_assignment() {
    let source_id = "vnext-duplicate-assignment";
    let (_tmp, _store, wal, allocator) = allocator_fixture(source_id).await;
    allocator
        .sync_agents(
            &agent_location(),
            &agent_file(
                "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
            ),
            &agent_content("agents"),
            1,
        )
        .await
        .unwrap();
    let projector = DirectiveProjector::new(source_id);
    let task = vnext_task_binding();
    let assignment = vnext_assignment_binding("assign-one", "assignment-1");
    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task.clone()],
            ..VNextAllocatorProjection::default()
        })
        .await;
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_task_input("issue-node", true)],
            2,
            "task",
        )
        .await
        .unwrap();
    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task.clone()],
            assignments: vec![assignment.clone()],
            ..VNextAllocatorProjection::default()
        })
        .await;
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_artifact_input(
                "assign-one",
                "issue-node",
                "WorkGraphTaskAssign/v1",
            )],
            3,
            "assign-one",
        )
        .await
        .unwrap();
    let first_lease_id = hex::encode(Sha256::digest(b"task-1\0assignment-1\0\x31"));

    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task.clone()],
            ..VNextAllocatorProjection::default()
        })
        .await;
    let before_duplicate = wal.head_sequence(source_id).await.unwrap();
    allocator
        .ingest_vnext(
            &projector,
            vec![vnext_artifact_input(
                "assign-two",
                "issue-node",
                "WorkGraphTaskAssign/v1",
            )],
            4,
            "assign-two-duplicate",
        )
        .await
        .unwrap();
    assert!(wal
        .read_from(source_id, before_duplicate + 1)
        .await
        .unwrap()
        .iter()
        .any(|(_, change)| {
            id(change) == format!("workgraph-vnext-lease:{first_lease_id}")
                && label(change) == "WorkGraphTaskLease"
                && is_delete(change)
        }));

    projector
        .next(VNextAllocatorProjection {
            tasks: vec![task],
            assignments: vec![assignment],
            ..VNextAllocatorProjection::default()
        })
        .await;
    let before_restore = wal.head_sequence(source_id).await.unwrap();
    allocator
        .ingest_vnext(
            &projector,
            vec![ProjectionInput::DeleteLifecycleArtifact {
                source_key: "assign-two".to_string(),
            }],
            5,
            "assign-two-delete",
        )
        .await
        .unwrap();
    let replacement_lease_id = hex::encode(Sha256::digest(b"task-1\0assignment-1\0\x32"));
    assert!(wal
        .read_from(source_id, before_restore + 1)
        .await
        .unwrap()
        .iter()
        .any(|(_, change)| {
            id(change) == format!("workgraph-vnext-lease:{replacement_lease_id}")
                && label(change) == "WorkGraphTaskLease"
                && !is_delete(change)
        }));
}

#[tokio::test]
async fn allocator_contract_exact_active_lookup_requires_every_binding() {
    let mut state = AllocationState::default();
    state.sync_agents(
        &agent_file(
            "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
        ),
        instant(0),
    );
    let lease = state
        .apply(
            assignment_event("assignment", "task", "agent", "2026-01-01T00:00:00Z"),
            instant(0),
        )
        .started
        .remove(0);
    assert_eq!(
        state.active_exact(
            "task",
            &lease.lease_id,
            "assignment",
            "agent",
            "agent/1",
            instant(14),
        ),
        Some(&lease)
    );
    for (task, lease_id, assignment, agent, slot) in [
        (
            "wrong-task",
            lease.lease_id.as_str(),
            "assignment",
            "agent",
            "agent/1",
        ),
        ("task", "wrong-lease", "assignment", "agent", "agent/1"),
        (
            "task",
            lease.lease_id.as_str(),
            "wrong-assignment",
            "agent",
            "agent/1",
        ),
        (
            "task",
            lease.lease_id.as_str(),
            "assignment",
            "wrong-agent",
            "agent/1",
        ),
        (
            "task",
            lease.lease_id.as_str(),
            "assignment",
            "agent",
            "agent/2",
        ),
    ] {
        assert!(state
            .active_exact(task, lease_id, assignment, agent, slot, instant(14),)
            .is_none());
    }
    assert!(state
        .active_exact(
            "task",
            &lease.lease_id,
            "assignment",
            "agent",
            "agent/1",
            instant(15),
        )
        .is_none());

    let source_id = "exact-active";
    let (_tmp, store, _wal, allocator) = allocator_fixture(source_id).await;
    store
        .set(
            source_id,
            "allocator:state",
            serde_json::to_vec(&state).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        allocator
            .validate_active(
                "task",
                &lease.lease_id,
                "assignment",
                "agent",
                "agent/1",
                instant(14),
            )
            .await
            .unwrap(),
        Some(lease.clone())
    );
    assert!(allocator
        .validate_active(
            "task",
            &lease.lease_id,
            "assignment",
            "wrong-agent",
            "agent/1",
            instant(14),
        )
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn allocator_contract_durable_ingest_is_idempotent_and_wal_ordered() {
    let source_id = "durable-contract";
    let (_tmp, store, wal, allocator) = allocator_fixture(source_id).await;
    let agents = agent_file(
        "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
    );
    allocator
        .sync_agents(&agent_location(), &agents, &agent_content("agents"), 1)
        .await
        .unwrap();
    allocator
        .ingest(
            "assignment-1-delivery",
            assignment_event("IC_assignment", "I_task", "agent", "2026-01-01T00:00:00Z"),
            Vec::new(),
            2,
        )
        .await
        .unwrap();
    allocator
        .ingest(
            "assignment-2-delivery",
            assignment_event(
                "IC_assignment_next",
                "I_next",
                "agent",
                "2026-01-01T00:01:00Z",
            ),
            Vec::new(),
            3,
        )
        .await
        .unwrap();
    let old_lease = hex::encode(Sha256::digest(b"I_task\0IC_assignment\0\x31"));
    let before = wal.head_sequence(source_id).await.unwrap();
    let conversion = convert_full(
        "issue_comment",
        &comment_event(
            "created",
            &result_body(&old_lease),
            "open",
            true,
            "IC_result",
        ),
    );
    let (appended, trusted) = allocator
        .ingest(
            "result-delivery",
            conversion.allocation.unwrap(),
            conversion.changes,
            4,
        )
        .await
        .unwrap();
    assert!(trusted);
    assert_eq!(appended, 10);

    let changes: Vec<_> = wal
        .read_from(source_id, before + 1)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, change)| change)
        .collect();
    assert_eq!(
        changes
            .iter()
            .map(|change| (label(change), is_delete(change), is_update(change)))
            .collect::<Vec<_>>(),
        vec![
            ("WorkGraphTaskResult", false, false),
            ("COMMENT_ON", false, false),
            ("RESULT_FOR", false, false),
            ("LEASE_FOR", true, false),
            ("LEASES_SLOT", true, false),
            ("WorkGraphTaskLease", true, false),
            ("WorkGraphTaskLease", false, true),
            ("LEASE_FOR", false, true),
            ("LEASES_SLOT", false, true),
            ("WorkGraphAgent", false, true),
        ]
    );
    assert_eq!(
        property(&changes, "WorkGraphTaskResult", "trusted"),
        &ElementValue::Bool(true)
    );
    let next_lease = hex::encode(Sha256::digest(b"I_next\0IC_assignment_next\0\x31"));
    assert!(changes.iter().any(|change| {
        id(change) == format!("workgraph-lease:I_next:{next_lease}")
            && label(change) == "WorkGraphTaskLease"
            && is_update(change)
    }));

    assert_eq!(
        allocator
            .ingest(
                "result-delivery",
                result_event("ignored", "I_task", &old_lease),
                Vec::new(),
                5,
            )
            .await
            .unwrap(),
        (0, false)
    );
    assert_eq!(
        allocator
            .ingest(
                "same-current-delivery",
                assignment_event(
                    "IC_assignment_next",
                    "I_next",
                    "agent",
                    "2026-01-01T00:01:00Z",
                ),
                Vec::new(),
                6,
            )
            .await
            .unwrap(),
        (0, true)
    );
    let stored = store
        .get(source_id, "allocator:state")
        .await
        .unwrap()
        .unwrap();
    let stored: Value = serde_json::from_slice(&stored).unwrap();
    assert_eq!(stored["pending"], json!([]));
    assert_eq!(wal.head_sequence(source_id).await.unwrap(), before + 10);
}

#[tokio::test]
async fn pending_projection_replays_every_crash_prefix_and_then_clears() {
    let projected: Vec<_> = convert(
        "issues",
        &issue_event("opened", issue("I_pending", VALIDATION_TASK, true, "open")),
    )
    .into_iter()
    .take(2)
    .collect();
    assert_eq!(projected.len(), 2);

    for prefix in 0..=projected.len() {
        let source_id = format!("pending-{prefix}");
        let (_tmp, store, wal, allocator) = allocator_fixture(&source_id).await;
        for change in projected.iter().take(prefix) {
            wal.append(&source_id, change).await.unwrap();
        }
        let encoded = serde_json::to_vec(&json!({
            "version": 4,
            "agents": {},
            "queue": {},
            "assignmentAttempts": {},
            "comments": {},
            "active": {},
            "pending": projected,
            "pendingOffset": prefix,
        }))
        .unwrap();
        store
            .set(&source_id, "allocator:state", encoded)
            .await
            .unwrap();

        assert_eq!(allocator.recover(99).await.unwrap(), 0);
        assert_eq!(
            wal.head_sequence(&source_id).await.unwrap(),
            projected.len() as u64
        );
        let stored = store
            .get(&source_id, "allocator:state")
            .await
            .unwrap()
            .unwrap();
        let state: Value = serde_json::from_slice(&stored).unwrap();
        assert_eq!(state["pending"], json!([]));
        let head = wal.head_sequence(&source_id).await.unwrap();
        allocator.recover(100).await.unwrap();
        assert_eq!(wal.head_sequence(&source_id).await.unwrap(), head);
    }
}

#[tokio::test]
async fn restart_restates_the_exact_active_lease_and_corruption_fails_closed() {
    let source_id = "restart";
    let (_tmp, store, wal, allocator) = allocator_fixture(source_id).await;
    let agents = agent_file(
        "version: 1\nagents:\n  - agentId: agent\n    slots: 1\n    leaseDuration: PT15M\n",
    );
    allocator
        .sync_agents(&agent_location(), &agents, &agent_content("agents"), 1)
        .await
        .unwrap();
    allocator
        .ingest(
            "assignment-delivery",
            assignment_event("assignment", "task", "agent", "2026-01-01T00:00:00Z"),
            Vec::new(),
            2,
        )
        .await
        .unwrap();
    let lease_id = hex::encode(Sha256::digest(b"task\0assignment\0\x31"));
    let active = allocator
        .validate_active(
            "task",
            &lease_id,
            "assignment",
            "agent",
            "agent/1",
            instant(1),
        )
        .await
        .unwrap()
        .unwrap();
    let before = wal.head_sequence(source_id).await.unwrap();

    let restarted = Allocator::new(source_id.into(), store.clone(), wal.clone());
    restarted.recover(3).await.unwrap();
    let restated = wal.read_from(source_id, before + 1).await.unwrap();
    let lease_element = format!("workgraph-lease:task:{lease_id}");
    assert!(restated
        .iter()
        .any(|(_, change)| change.get_reference().element_id.as_ref() == lease_element));
    assert_eq!(
        restarted
            .validate_active(
                "task",
                &lease_id,
                "assignment",
                "agent",
                "agent/1",
                instant(1),
            )
            .await
            .unwrap(),
        Some(active)
    );

    let stored = store
        .get(source_id, "allocator:state")
        .await
        .unwrap()
        .unwrap();
    let legacy_worker_v2 = json!({
        "version": 2,
        "workers": {},
        "queue": {},
        "assignmentAttempts": {},
        "comments": {},
        "active": {},
        "pending": []
    });
    store
        .set(
            source_id,
            "allocator:state",
            serde_json::to_vec(&legacy_worker_v2).unwrap(),
        )
        .await
        .unwrap();
    assert!(restarted.recover(4).await.is_err());

    store
        .set(source_id, "allocator:state", stored.clone())
        .await
        .unwrap();
    let mut prior_prototype: Value = serde_json::from_slice(&stored).unwrap();
    prior_prototype["version"] = json!(3);
    store
        .set(
            source_id,
            "allocator:state",
            serde_json::to_vec(&prior_prototype).unwrap(),
        )
        .await
        .unwrap();
    assert!(restarted
        .recover(5)
        .await
        .unwrap_err()
        .to_string()
        .contains("clear source state"));

    store
        .set(source_id, "allocator:state", stored.clone())
        .await
        .unwrap();
    let mut unsupported: Value = serde_json::from_slice(&stored).unwrap();
    unsupported["version"] = json!(1);
    store
        .set(
            source_id,
            "allocator:state",
            serde_json::to_vec(&unsupported).unwrap(),
        )
        .await
        .unwrap();
    assert!(restarted.recover(6).await.is_err());

    store
        .set(source_id, "allocator:state", b"{not-json".to_vec())
        .await
        .unwrap();
    assert!(restarted.recover(7).await.is_err());
    assert!(restarted
        .validate_active(
            "task",
            &lease_id,
            "assignment",
            "agent",
            "agent/1",
            instant(1),
        )
        .await
        .is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
// VNext tests
// ═══════════════════════════════════════════════════════════════════════════

mod vnext_tests {
    use crate::config::{GitHubWorkGraphSourceConfig, WorkflowDefinitionConfig};
    use crate::descriptor::GitHubWorkGraphSourceDescriptor;
    use crate::source::GitHubWorkGraphSourceBuilder;
    use crate::vnext::*;
    use drasi_plugin_sdk::prelude::SourcePluginDescriptor;

    // ── Marker recognition ──────────────────────────────────────────────

    #[test]
    fn vnext_task_marker_recognized() {
        let body = "WorkGraphTask/v3\n{\"some\":\"json\"}";
        assert!(body.starts_with(VNEXT_TASK_MARKER));
    }

    #[test]
    fn vnext_task_marker_v2_not_recognized() {
        let body = "WorkGraphTask/v2\n{\"some\":\"json\"}";
        assert!(!body.starts_with(VNEXT_TASK_MARKER));
    }

    #[test]
    fn vnext_lifecycle_markers() {
        assert!(is_vnext_lifecycle_marker(
            "WorkGraphTaskAssign/v1\nsome body"
        ));
        assert!(is_vnext_lifecycle_marker(
            "WorkGraphTaskDispatch/v1\nsome body"
        ));
        assert!(is_vnext_lifecycle_marker(
            "WorkGraphTaskResult/v1\nsome body"
        ));
        assert!(is_vnext_lifecycle_marker(
            "WorkGraphTaskEvaluate/v1\nsome body"
        ));
        assert!(!is_vnext_lifecycle_marker(
            "WorkGraphTaskAssignment/v1\nfoo"
        ));
        assert!(!is_vnext_lifecycle_marker("ordinary comment"));
    }

    #[test]
    fn lifecycle_trust_role_assign_dispatch_are_assigner() {
        assert_eq!(
            lifecycle_trust_role("WorkGraphTaskAssign/v1\nfoo"),
            Some(LifecycleTrustRole::Assigner)
        );
        assert_eq!(
            lifecycle_trust_role("WorkGraphTaskDispatch/v1\nfoo"),
            Some(LifecycleTrustRole::Assigner)
        );
    }

    #[test]
    fn lifecycle_trust_role_result_evaluate_are_reporter() {
        assert_eq!(
            lifecycle_trust_role("WorkGraphTaskResult/v1\nfoo"),
            Some(LifecycleTrustRole::Reporter)
        );
        assert_eq!(
            lifecycle_trust_role("WorkGraphTaskEvaluate/v1\nfoo"),
            Some(LifecycleTrustRole::Reporter)
        );
    }

    #[test]
    fn lifecycle_trust_role_ordinary_is_none() {
        assert_eq!(lifecycle_trust_role("just a comment"), None);
    }

    // ── Definition source key ───────────────────────────────────────────

    #[test]
    fn definition_source_key_deterministic() {
        let key1 = definition_source_key("myorg/myrepo", "refs/heads/main", "path/to/def.body");
        let key2 = definition_source_key("myorg/myrepo", "refs/heads/main", "path/to/def.body");
        assert_eq!(key1, key2);
        assert_eq!(
            key1,
            "github:definition:myorg/myrepo:refs/heads/main:path/to/def.body"
        );
    }

    #[test]
    fn definition_source_key_differs_for_different_path() {
        let key1 = definition_source_key("org/repo", "refs/heads/main", "a.body");
        let key2 = definition_source_key("org/repo", "refs/heads/main", "b.body");
        assert_ne!(key1, key2);
    }

    // ── Document types serde ────────────────────────────────────────────

    #[test]
    fn task_document_roundtrip() {
        let doc = TaskDocument {
            source_key: "MDExOklzc3VlNTI=".to_string(),
            body: "WorkGraphTask/v3\n{}".to_string(),
            is_open: true,
            state_reason: "".to_string(),
            parent_source_key: Some("MDExOklzc3VlNTM=".to_string()),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let parsed: TaskDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source_key, doc.source_key);
        assert_eq!(parsed.parent_source_key, doc.parent_source_key);
        assert!(parsed.is_open);
    }

    #[test]
    fn lifecycle_artifact_document_roundtrip() {
        let doc = LifecycleArtifactDocument {
            source_key: "comment-node-1".to_string(),
            task_source_key: "issue-node-1".to_string(),
            body: "WorkGraphTaskAssign/v1\n{}".to_string(),
        };
        let json = serde_json::to_string(&doc).unwrap();
        let parsed: LifecycleArtifactDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source_key, doc.source_key);
        assert_eq!(parsed.task_source_key, doc.task_source_key);
    }

    #[test]
    fn projection_input_upsert_task_serde() {
        let input = ProjectionInput::UpsertTask(TaskDocument {
            source_key: "node1".to_string(),
            body: "WorkGraphTask/v3\ndata".to_string(),
            is_open: true,
            state_reason: "".to_string(),
            parent_source_key: None,
        });
        let json = serde_json::to_string(&input).unwrap();
        let parsed: ProjectionInput = serde_json::from_str(&json).unwrap();
        match parsed {
            ProjectionInput::UpsertTask(t) => {
                assert_eq!(t.source_key, "node1");
                assert!(t.is_open);
            }
            _ => panic!("expected UpsertTask"),
        }
    }

    #[test]
    fn projection_input_delete_task_serde() {
        let input = ProjectionInput::DeleteTask {
            source_key: "node1".to_string(),
        };
        let json = serde_json::to_string(&input).unwrap();
        let parsed: ProjectionInput = serde_json::from_str(&json).unwrap();
        match parsed {
            ProjectionInput::DeleteTask { source_key } => assert_eq!(source_key, "node1"),
            _ => panic!("expected DeleteTask"),
        }
    }

    #[test]
    fn vnext_allocator_projection_roundtrip() {
        let projection = VNextAllocatorProjection {
            tasks: vec![VNextTaskBinding {
                source_key: "issue-node".to_string(),
                task_id: "task-1".to_string(),
                task_element_id: "task-element".to_string(),
            }],
            assignments: vec![VNextAssignmentBinding {
                source_key: "assign-comment".to_string(),
                task_source_key: "issue-node".to_string(),
                task_id: "task-1".to_string(),
                assignment_id: "assignment-1".to_string(),
                permitted_executors: vec!["agent".to_string()],
            }],
            dispatches: vec![VNextDispatchBinding {
                source_key: "dispatch-comment".to_string(),
                task_source_key: "issue-node".to_string(),
                task_id: "task-1".to_string(),
                assignment_id: "assignment-1".to_string(),
                lease_id: "lease-1".to_string(),
                executor_id: "agent".to_string(),
                slot_id: "agent/1".to_string(),
            }],
        };
        let encoded = serde_json::to_vec(&projection).unwrap();
        assert_eq!(
            serde_json::from_slice::<VNextAllocatorProjection>(&encoded).unwrap(),
            projection
        );
    }

    // ── GitHub issue locator ────────────────────────────────────────────

    #[test]
    fn locator_upsert_serde() {
        let locator = GitHubIssueLocator {
            source_key: "node1".to_string(),
            repository_owner: "myorg".to_string(),
            repository_name: "myrepo".to_string(),
            issue_number: 42,
            issue_node_id: "node1".to_string(),
        };
        let input = ProjectionInput::UpsertLocator(locator);
        let json = serde_json::to_string(&input).unwrap();
        let parsed: ProjectionInput = serde_json::from_str(&json).unwrap();
        match parsed {
            ProjectionInput::UpsertLocator(loc) => {
                assert_eq!(loc.repository_owner, "myorg");
                assert_eq!(loc.issue_number, 42);
            }
            _ => panic!("expected UpsertLocator"),
        }
    }

    // ── Config validation ───────────────────────────────────────────────

    #[test]
    fn workflow_definition_config_default_path() {
        let config = WorkflowDefinitionConfig::default();
        assert_eq!(
            config.path,
            ".github/workgraph/workflows/issue-lifecycle-vnext.body"
        );
    }

    #[test]
    fn workflow_definition_config_validation_missing_repository() {
        let config = WorkflowDefinitionConfig {
            repository: "".to_string(),
            ..WorkflowDefinitionConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn workflow_definition_config_validation_missing_token() {
        let config = WorkflowDefinitionConfig {
            repository: "acme/repo".to_string(),
            token: "".to_string(),
            ..WorkflowDefinitionConfig::default()
        };
        assert!(config.validate().is_err());
    }

    // ── Builder validation ──────────────────────────────────────────────

    #[test]
    fn builder_fails_without_projector_when_definition_configured() {
        let config = GitHubWorkGraphSourceConfig {
            organization: "acme".to_string(),
            task_issue_type: crate::config::TaskIssueType {
                id: "IT_test".to_string(),
                name: "WorkGraphTask".to_string(),
            },
            repositories: vec![],
            agent_config: None,
            lease_trust: None,
            workflow_definition: Some(WorkflowDefinitionConfig {
                repository: "acme/repo".to_string(),
                r#ref: "main".to_string(),
                token: "tok".to_string(),
                ..WorkflowDefinitionConfig::default()
            }),
            webhook: crate::config::WebhookConfig {
                secret: "s".to_string(),
                lease_validation_token: "v".to_string(),
                ..crate::config::WebhookConfig::default()
            },
            durability: drasi_lib::DurabilityConfig {
                enabled: true,
                ..drasi_lib::DurabilityConfig::default()
            },
        };
        let result = GitHubWorkGraphSourceBuilder::new("test")
            .with_config(config)
            .build();
        assert!(result.is_err());
        let err = match result {
            Ok(_) => panic!("should fail without projector"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("WorkGraphProjector"),
            "error should mention projector: {err}"
        );
    }

    #[tokio::test]
    async fn dynamic_descriptor_rejects_programmatic_projector_config_clearly() {
        let error = match GitHubWorkGraphSourceDescriptor
            .create_source(
                "test",
                &serde_json::json!({"workflowDefinition": {}}),
                false,
            )
            .await
        {
            Ok(_) => panic!("dynamic descriptor cannot inject a projector"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("with_workgraph_projector"));
    }

    // ── Webhook normalization helpers ────────────────────────────────────

    #[test]
    fn extract_issue_locator_from_payload() {
        use crate::webhook::extract_issue_locator;
        let issue = serde_json::json!({
            "node_id": "MDExOklzc3VlMQ==",
            "number": 42
        });
        let payload = serde_json::json!({
            "repository": {
                "full_name": "myorg/myrepo"
            }
        });
        let locator = extract_issue_locator(&issue, &payload).unwrap();
        assert_eq!(locator.source_key, "MDExOklzc3VlMQ==");
        assert_eq!(locator.repository_owner, "myorg");
        assert_eq!(locator.repository_name, "myrepo");
        assert_eq!(locator.issue_number, 42);
        assert_eq!(locator.issue_node_id, "MDExOklzc3VlMQ==");
    }

    #[test]
    fn item_is_open_checks_state() {
        use crate::webhook::item_is_open;
        let open = serde_json::json!({"state": "open"});
        let closed = serde_json::json!({"state": "closed"});
        let missing = serde_json::json!({});
        assert!(item_is_open(&open));
        assert!(!item_is_open(&closed));
        assert!(item_is_open(&missing));
    }
}
