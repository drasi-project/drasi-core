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

use crate::config::{GitHubWorkGraphSourceConfig, RepositoryFilter, TaskIssueType, WebhookConfig};
use crate::descriptor::GitHubWorkGraphSourceDescriptor;
use crate::mapping::{Converter, NODE_LABELS, RELATION_LABELS};
use crate::webhook::verify_signature;
use crate::workgraph::{
    classify_comment, classify_task_body, error_code, CommentClassification, TaskClassification,
};
use drasi_core::evaluation::context::QueryPartEvaluationContext;
use drasi_core::evaluation::functions::FunctionRegistry;
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_core::query::{ContinuousQuery, QueryBuilder};
use drasi_lib::wal::CapacityPolicy;
use drasi_lib::DurabilityConfig;
use drasi_plugin_sdk::prelude::SourcePluginDescriptor;
use drasi_query_cypher::CypherParser;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

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
const ASSIGNMENT: &str = r#"WorkGraphTaskAssignment/v1

```json
{
  "agentProfile": "issue-validator"
}
```
"#;
const INFO_REQUEST_ASSIGNMENT: &str = r#"WorkGraphTaskAssignment/v1

```json
{
  "agentProfile": "issue-info-requester"
}
```
"#;
const RESULT: &str = r#"WorkGraphTaskResult/v1

```json
{
  "taskType": "validate-issue",
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
  "outcome": "succeeded",
  "summary": "Requested the missing information.",
  "result": {
    "requestCommentNodeId": "IC_request"
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

fn convert(event: &str, payload: &Value) -> Vec<SourceChange> {
    Converter::new("gh", "acme", &task_type(), 1)
        .convert(event, payload)
        .unwrap()
        .unwrap()
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
         RETURN task.nodeId AS taskId, assignment.agentProfile AS agentProfile, \
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

#[test]
fn task_envelopes_accept_only_strict_work_definitions() {
    for body in [VALIDATION_TASK, REQUEST_INFO_TASK] {
        assert!(matches!(
            classify_task_body(body),
            TaskClassification::Task(_)
        ));
    }
    for body in [
        &format!("{VALIDATION_TASK}\n"),
        "WorkGraphTask/v1\n\n```yaml\ntaskType: validate-issue\ninputs:\n  validationProfile: other\n```\n",
        "WorkGraphTask/v1\n\n```yaml\ntaskType: validate-issue\nagentProfile: issue-validator\ninputs:\n  validationProfile: new-issue-default\n```\n",
        "WorkGraphTask/v1\n\n```yaml\ntaskType: validate-issue\ninputs:\n  validationProfile: new-issue-default\n---\ntaskType: request-info\ninputs:\n  validationResultCommentNodeId: IC_result\n```\n",
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
        classify_comment(ACCEPTANCE),
        CommentClassification::Acceptance(_)
    ));
    assert!(matches!(
        classify_comment("WorkGraphTaskResult/v1\n\n```json\n{}\n```"),
        CommentClassification::Invalid(_)
    ));
    assert!(matches!(
        classify_comment("WorkGraphTaskAssignment/v1\n\n```json\n{\"agentProfile\":\"issue-validator\"}\n```\n"),
        CommentClassification::Invalid(error) if error.code == error_code::NON_CANONICAL_JSON
    ));
    assert!(matches!(
        classify_comment("WorkGraphTaskAssignment/v1\n\n```json\n{\n  \"agentProfile\": \"issue-risk-profiler\"\n}\n```\n"),
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
fn task_state_is_retained_on_close_and_reopen() {
    let closed = convert(
        "issues",
        &issue_event("closed", issue("I_task", VALIDATION_TASK, true, "closed")),
    );
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
            .unwrap();
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
        .unwrap();
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
        .unwrap();
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
    assert_eq!(additions(&process_changes(&query, changes).await), 1);
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
fn supported_assignment_profiles_map_exactly() {
    for (body, expected_profile, id) in [
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
            property(&changes, "WorkGraphTaskAssignment", "agentProfile"),
            &ElementValue::from(&json!(expected_profile))
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
        .unwrap();
    assert_eq!(id(&removal[0]), "TASK_FOR:42");
}

#[test]
fn config_requires_exact_task_type_id_and_name() {
    let mut config = GitHubWorkGraphSourceConfig {
        organization: "acme".to_string(),
        task_issue_type: task_type(),
        repositories: vec![],
        webhook: WebhookConfig {
            secret: "secret".to_string(),
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
}

#[test]
fn descriptor_exposes_task_type_and_graph_schema() {
    let descriptor = GitHubWorkGraphSourceDescriptor;
    let schema = descriptor.config_schema_json();
    assert!(schema.contains("taskIssueType"));
    assert!(NODE_LABELS.contains(&"WorkGraphTask"));
    assert!(NODE_LABELS.contains(&"WorkGraphTaskAssignment"));
    assert!(NODE_LABELS.contains(&"WorkGraphTaskResult"));
    assert!(NODE_LABELS.contains(&"WorkGraphTaskResultAcceptance"));
    assert!(RELATION_LABELS.contains(&"ASSIGNMENT_FOR"));
    assert!(RELATION_LABELS.contains(&"RESULT_FOR"));
    assert!(RELATION_LABELS.contains(&"ACCEPTS_RESULT"));
    assert!(RELATION_LABELS.contains(&"TASK_FOR"));
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
