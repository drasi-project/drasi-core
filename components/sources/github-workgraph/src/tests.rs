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

use crate::config::{
    GitHubWorkGraphSourceConfig, LeaseTrust, RepositoryFilter, TaskIssueType, TrustedIdentity,
    WebhookConfig, WorkerConfig, DEFAULT_WORKER_API_BASE_URL,
};
use crate::descriptor::GitHubWorkGraphSourceDescriptor;
use crate::lease_ledger::{AnchorState, LeaseLedger};
use crate::mapping::{
    anchor_changes, worker_changes, Conversion, Converter, WorkerProjection, NODE_LABELS,
    RELATION_LABELS,
};
use crate::webhook::verify_signature;
use crate::worker_client::{WorkerFileClient, WorkerFileError};
use crate::worker_sync::push_touches_worker_file;
use crate::workers::{
    error_code as worker_error_code, parse_iso8601_duration_seconds, parse_worker_file,
    WorkerFileContent, WorkerFileLocation,
};
use crate::workgraph::{
    classify_comment, classify_task_body, error_code, CommentClassification, TaskClassification,
};
use drasi_core::evaluation::context::QueryPartEvaluationContext;
use drasi_core::evaluation::functions::FunctionRegistry;
use drasi_core::evaluation::variable_value::VariableValue;
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_core::query::{ContinuousQuery, QueryBuilder};
use drasi_github_workgraph::{canonical_task_lease_body, TaskLease};
use drasi_lib::wal::CapacityPolicy;
use drasi_lib::DurabilityConfig;
use drasi_plugin_sdk::prelude::SourcePluginDescriptor;
use drasi_query_cypher::CypherParser;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
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

/// The configured lease trust used by every worker-queue test. The default
/// comment author in `comment_event` ("bot") is both dispatcher and reporter,
/// matching the prototype's single-identity deployment.
fn lease_trust() -> LeaseTrust {
    LeaseTrust {
        dispatchers: vec![TrustedIdentity {
            id: "U_bot".to_string(),
            login: "bot".to_string(),
        }],
        reporters: vec![TrustedIdentity {
            id: "U_reporter".to_string(),
            login: "reporter".to_string(),
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

/// Drives deliveries through exactly the fold the live Source and the
/// bootstrapper use: convert, apply every contribution to a ledger, then
/// project the affected anchors.
#[derive(Default)]
struct LeaseWorld {
    ledger: LeaseLedger,
    clock: u64,
}

impl LeaseWorld {
    fn deliver(&mut self, payload: &Value) -> Vec<SourceChange> {
        self.clock += 1;
        let conversion = convert_full("issue_comment", payload);
        let mut changes = conversion.changes;
        let mut affected = std::collections::BTreeSet::new();
        for intent in &conversion.lifecycle {
            affected.extend(self.ledger.apply(intent));
        }
        changes.extend(anchor_changes("gh", self.clock, &self.ledger, affected));
        changes
    }

    /// The anchor projection for the canonical single-task anchor.
    fn anchor(&self) -> Option<AnchorState> {
        self.ledger.project(LEASE_ANCHOR_ID)
    }
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
        worker_config: None,
        lease_trust: None,
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

// ---------------------------------------------------------------------------
// Worker queue: worker file contract, worker/slot projection, and the
// Assignment/v2, Lease/v1, Result/v2, and LeaseExpiration/v1 lifecycle.
// ---------------------------------------------------------------------------

const WORKER_FILE: &str = "version: 1\nworkers:\n  - workerId: validator-1\n    agentProfile: \
                           issue-validator\n    slots: 2\n    leaseDuration: PT15M\n  - workerId: \
                           info-requester-1\n    agentProfile: issue-info-requester\n    slots: \
                           1\n    leaseDuration: PT15M\n";

const ASSIGNMENT_V2: &str = r#"WorkGraphTaskAssignment/v2

```json
{
  "agentProfile": "issue-validator",
  "workerId": "validator-1"
}
```
"#;

const LEASE: &str = r#"WorkGraphTaskLease/v1

```json
{
  "leaseId": "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
  "assignmentCommentNodeId": "IC_assignment",
  "workerId": "validator-1",
  "slotId": "validator-1/1",
  "acquiredAt": "2026-08-19T22:00:00Z",
  "expiresAt": "2026-08-19T22:15:00Z"
}
```
"#;

const RESULT_V2: &str = r#"WorkGraphTaskResult/v2

```json
{
  "taskType": "validate-issue",
  "leaseId": "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
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

const LEASE_EXPIRATION: &str = r#"WorkGraphTaskLeaseExpiration/v1

```json
{
  "leaseCommentNodeId": "IC_lease",
  "leaseId": "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
  "expiredAt": "2026-08-19T22:15:00Z",
  "reason": "deadline-reached"
}
```
"#;

const LEASE_ID: &str = "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21";
const LEASE_ANCHOR_ID: &str = "workgraph-lease:I_task:0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21";

fn worker_location() -> WorkerFileLocation {
    WorkerFileLocation {
        repository: "acme/widgets".to_string(),
        r#ref: "main".to_string(),
        path: ".github/workgraph/workers.yaml".to_string(),
    }
}

fn worker_content(text: &str) -> WorkerFileContent {
    WorkerFileContent {
        text: text.to_string(),
        oid: "blob-oid".to_string(),
    }
}

fn project_workers(text: &str) -> Vec<SourceChange> {
    project_workers_with(text, &BTreeMap::new(), &BTreeMap::new())
}

fn project_workers_with(
    text: &str,
    retiring: &BTreeMap<String, u32>,
    removed: &BTreeMap<String, u32>,
) -> Vec<SourceChange> {
    let file = parse_worker_file(text).expect("worker file must parse");
    let content = worker_content(text);
    worker_changes(
        "gh",
        1,
        &worker_location(),
        &WorkerProjection::Loaded {
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
fn worker_file_accepts_only_the_strict_version_one_grammar() {
    let file = parse_worker_file(WORKER_FILE).expect("valid worker file");
    assert_eq!(file.version, 1);
    assert_eq!(file.workers.len(), 2);
    assert_eq!(file.workers[0].worker_id, "validator-1");
    assert_eq!(file.workers[0].agent_profile, "issue-validator");
    assert_eq!(file.workers[0].slots, 2);
    assert_eq!(file.workers[0].lease_duration, "PT15M");
    assert_eq!(file.workers[0].lease_duration_seconds, 900);
    assert_eq!(
        file.workers[0].slot_ids(),
        vec!["validator-1/1", "validator-1/2"]
    );

    for (body, expected) in [
        // Zero workers must never become a silently empty pool.
        ("version: 1\nworkers: []\n", worker_error_code::INVALID_WORKER_FILE_PAYLOAD),
        // Unsupported and missing versions.
        (
            "version: 2\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: PT1M\n",
            worker_error_code::INVALID_WORKER_FILE_PAYLOAD,
        ),
        ("workers: []\n", worker_error_code::INVALID_WORKER_FILE_YAML),
        // Unknown field.
        (
            "version: 1\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: PT1M\n    extra: nope\n",
            worker_error_code::INVALID_WORKER_FILE_YAML,
        ),
        // Wrong types.
        (
            "version: 1\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: two\n    leaseDuration: PT1M\n",
            worker_error_code::INVALID_WORKER_FILE_YAML,
        ),
        // Unsupported profile.
        (
            "version: 1\nworkers:\n  - workerId: w\n    agentProfile: issue-risk-profiler\n    slots: 1\n    leaseDuration: PT1M\n",
            worker_error_code::INVALID_WORKER_FILE_PAYLOAD,
        ),
        // Non-positive and unsafe slot counts.
        (
            "version: 1\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 0\n    leaseDuration: PT1M\n",
            worker_error_code::INVALID_WORKER_FILE_PAYLOAD,
        ),
        (
            "version: 1\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 17\n    leaseDuration: PT1M\n",
            worker_error_code::INVALID_WORKER_FILE_PAYLOAD,
        ),
        // Empty, oversized, and slot-ambiguous worker IDs.
        (
            "version: 1\nworkers:\n  - workerId: ''\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: PT1M\n",
            worker_error_code::INVALID_WORKER_FILE_PAYLOAD,
        ),
        (
            "version: 1\nworkers:\n  - workerId: a/b\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: PT1M\n",
            worker_error_code::INVALID_WORKER_FILE_PAYLOAD,
        ),
        // Invalid, non-positive, and unsafe durations.
        (
            "version: 1\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: 15m\n",
            worker_error_code::INVALID_WORKER_FILE_PAYLOAD,
        ),
        (
            "version: 1\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: PT0S\n",
            worker_error_code::INVALID_WORKER_FILE_PAYLOAD,
        ),
        (
            "version: 1\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: P2D\n",
            worker_error_code::INVALID_WORKER_FILE_PAYLOAD,
        ),
        // Duplicate worker IDs, and therefore duplicate derived slot IDs.
        (
            "version: 1\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: PT1M\n  - workerId: w\n    agentProfile: issue-info-requester\n    slots: 1\n    leaseDuration: PT1M\n",
            worker_error_code::INVALID_WORKER_FILE_PAYLOAD,
        ),
    ] {
        let error = parse_worker_file(body).expect_err("worker file must be rejected");
        assert_eq!(error.code, expected, "unexpected code for: {body}");
    }
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
fn worker_file_location_validates_repository_ref_and_path() {
    assert!(worker_location().validate().is_ok());
    for (repository, git_ref, path) in [
        ("widgets", "main", ".github/workgraph/workers.yaml"),
        ("acme/a/b", "main", ".github/workgraph/workers.yaml"),
        ("acme/widgets", "", ".github/workgraph/workers.yaml"),
        ("acme/widgets", "ma in", ".github/workgraph/workers.yaml"),
        ("acme/widgets", "main", "/absolute.yaml"),
        ("acme/widgets", "main", "../escape.yaml"),
        ("acme/widgets", "main", "a//b.yaml"),
        ("acme/widgets", "main", ""),
    ] {
        let location = WorkerFileLocation {
            repository: repository.to_string(),
            r#ref: git_ref.to_string(),
            path: path.to_string(),
        };
        assert!(
            location.validate().is_err(),
            "expected rejection for {repository}@{git_ref}:{path}"
        );
    }
    let location = worker_location();
    assert_eq!(location.owner(), "acme");
    assert_eq!(location.name(), "widgets");
    assert_eq!(location.expression(), "main:.github/workgraph/workers.yaml");
    assert!(location.matches_push("acme/widgets", "refs/heads/main"));
    assert!(location.matches_push("ACME/Widgets", "main"));
    assert!(!location.matches_push("acme/widgets", "refs/heads/other"));
    assert!(!location.matches_push("acme/other", "refs/heads/main"));
}

#[test]
fn workers_project_stable_nodes_slots_and_relations() {
    let changes = project_workers(WORKER_FILE);

    assert_eq!(
        ids_with_label(&changes, "WorkGraphWorker"),
        vec![
            "workgraph-worker:validator-1",
            "workgraph-worker:info-requester-1"
        ]
    );
    assert_eq!(
        ids_with_label(&changes, "WorkGraphWorkerSlot"),
        vec![
            "workgraph-worker-slot:validator-1/1",
            "workgraph-worker-slot:validator-1/2",
            "workgraph-worker-slot:info-requester-1/1",
        ]
    );
    assert_eq!(
        ids_with_label(&changes, "HAS_SLOT"),
        vec![
            "HAS_SLOT:workgraph-worker:validator-1:workgraph-worker-slot:validator-1/1",
            "HAS_SLOT:workgraph-worker:validator-1:workgraph-worker-slot:validator-1/2",
            "HAS_SLOT:workgraph-worker:info-requester-1:workgraph-worker-slot:info-requester-1/1",
        ]
    );

    let worker = "workgraph-worker:validator-1";
    assert_eq!(
        node_property(&changes, worker, "workerId"),
        &ElementValue::from(&json!("validator-1"))
    );
    assert_eq!(
        node_property(&changes, worker, "agentProfile"),
        &ElementValue::from(&json!("issue-validator"))
    );
    assert_eq!(
        node_property(&changes, worker, "configuredSlotCount"),
        &ElementValue::Integer(2)
    );
    assert_eq!(
        node_property(&changes, worker, "leaseDuration"),
        &ElementValue::from(&json!("PT15M"))
    );
    assert_eq!(
        node_property(&changes, worker, "leaseDurationSeconds"),
        &ElementValue::Integer(900)
    );
    // Configuration provenance travels with every projected worker.
    assert_eq!(
        node_property(&changes, worker, "configRepository"),
        &ElementValue::from(&json!("acme/widgets"))
    );
    assert_eq!(
        node_property(&changes, worker, "configRef"),
        &ElementValue::from(&json!("main"))
    );
    assert_eq!(
        node_property(&changes, worker, "configPath"),
        &ElementValue::from(&json!(".github/workgraph/workers.yaml"))
    );
    assert_eq!(
        node_property(&changes, worker, "configBlobOid"),
        &ElementValue::from(&json!("blob-oid"))
    );
    assert_eq!(
        node_property(&changes, worker, "configDigest"),
        &ElementValue::from(&json!(format!(
            "sha256:{}",
            hex::encode(Sha256::digest(WORKER_FILE))
        )))
    );

    let slot = "workgraph-worker-slot:validator-1/2";
    assert_eq!(
        node_property(&changes, slot, "slotId"),
        &ElementValue::from(&json!("validator-1/2"))
    );
    assert_eq!(
        node_property(&changes, slot, "slotNumber"),
        &ElementValue::Integer(2)
    );
    assert_eq!(
        node_property(&changes, slot, "workerId"),
        &ElementValue::from(&json!("validator-1"))
    );
    assert_eq!(
        node_property(&changes, slot, "enabled"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&changes, slot, "retiring"),
        &ElementValue::Bool(false)
    );

    // A valid configuration always clears any previous configuration error.
    assert!(changes
        .iter()
        .any(|change| is_delete(change) && id(change) == "workgraph-error:worker-config"));

    // Re-projecting the same file is byte-identical, so redelivery converges.
    let repeat = project_workers(WORKER_FILE);
    assert_eq!(changes.len(), repeat.len());
    for (first, second) in changes.iter().zip(repeat.iter()) {
        assert_eq!(id(first), id(second));
        assert_eq!(label(first), label(second));
    }
}

#[test]
fn capacity_reduction_retires_excess_slots_without_deleting_them() {
    let reduced = "version: 1\nworkers:\n  - workerId: validator-1\n    agentProfile: \
                   issue-validator\n    slots: 1\n    leaseDuration: PT15M\n";
    let retiring = BTreeMap::from([("validator-1".to_string(), 3)]);
    let changes = project_workers_with(reduced, &retiring, &BTreeMap::new());

    // Every previously materialized slot stays addressable so an in-flight
    // Lease keeps a valid LEASES_SLOT target.
    assert_eq!(
        ids_with_label(&changes, "WorkGraphWorkerSlot"),
        vec![
            "workgraph-worker-slot:validator-1/1",
            "workgraph-worker-slot:validator-1/2",
            "workgraph-worker-slot:validator-1/3",
        ]
    );
    assert!(!changes
        .iter()
        .any(|change| is_delete(change) && label(change) == "WorkGraphWorkerSlot"));

    assert_eq!(
        node_property(&changes, "workgraph-worker-slot:validator-1/1", "enabled"),
        &ElementValue::Bool(true)
    );
    for retired in [
        "workgraph-worker-slot:validator-1/2",
        "workgraph-worker-slot:validator-1/3",
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
            "workgraph-worker:validator-1",
            "configuredSlotCount"
        ),
        &ElementValue::Integer(1)
    );

    // Growing back re-enables the same stable slot identities.
    let grown = project_workers_with(WORKER_FILE, &retiring, &BTreeMap::new());
    assert_eq!(
        node_property(&grown, "workgraph-worker-slot:validator-1/2", "enabled"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&grown, "workgraph-worker-slot:validator-1/3", "retiring"),
        &ElementValue::Bool(true)
    );
}

#[test]
fn removed_workers_are_deleted_with_their_slots_and_relations() {
    let single = "version: 1\nworkers:\n  - workerId: validator-1\n    agentProfile: \
                  issue-validator\n    slots: 1\n    leaseDuration: PT15M\n";
    let removed = BTreeMap::from([("info-requester-1".to_string(), 2)]);
    let changes = project_workers_with(single, &BTreeMap::new(), &removed);

    for deleted in [
        "workgraph-worker:info-requester-1",
        "workgraph-worker-slot:info-requester-1/1",
        "workgraph-worker-slot:info-requester-1/2",
        "HAS_SLOT:workgraph-worker:info-requester-1:workgraph-worker-slot:info-requester-1/1",
        "HAS_SLOT:workgraph-worker:info-requester-1:workgraph-worker-slot:info-requester-1/2",
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
        .any(|change| !is_delete(change) && id(change) == "workgraph-worker:validator-1"));
}

#[test]
fn rejected_worker_config_emits_an_error_and_never_an_empty_pool() {
    let error = parse_worker_file("version: 1\nworkers: []\n").expect_err("must reject");
    let changes = worker_changes(
        "gh",
        1,
        &worker_location(),
        &WorkerProjection::Rejected(&error),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    assert_eq!(changes.len(), 1);
    assert_eq!(id(&changes[0]), "workgraph-error:worker-config");
    assert_eq!(label(&changes[0]), "WorkGraphError");
    assert_eq!(
        node_property(&changes, "workgraph-error:worker-config", "errorKind"),
        &ElementValue::from(&json!("invalid-workgraph-worker-config"))
    );
    assert_eq!(
        node_property(&changes, "workgraph-error:worker-config", "errorCode"),
        &ElementValue::from(&json!(worker_error_code::INVALID_WORKER_FILE_PAYLOAD))
    );
    assert_eq!(
        node_property(&changes, "workgraph-error:worker-config", "configPath"),
        &ElementValue::from(&json!(".github/workgraph/workers.yaml"))
    );
    // A rejected configuration must not delete or rewrite the worker pool.
    assert!(
        !changes
            .iter()
            .any(|change| label(change) == "WorkGraphWorker"
                || label(change) == "WorkGraphWorkerSlot")
    );
}

#[test]
fn assignment_v1_stays_readable_while_v2_names_a_worker_queue() {
    let v1 = convert(
        "issue_comment",
        &comment_event("created", ASSIGNMENT, "open", true, "IC_assignment"),
    );
    assert_eq!(
        property(&v1, "WorkGraphTaskAssignment", "version"),
        &ElementValue::Integer(1)
    );
    assert_eq!(
        property(&v1, "WorkGraphTaskAssignment", "workerId"),
        &ElementValue::Null
    );
    assert!(!v1.iter().any(|change| label(change) == "ASSIGNED_TO"));

    let v2 = convert(
        "issue_comment",
        &comment_event("created", ASSIGNMENT_V2, "open", true, "IC_assignment"),
    );
    assert_eq!(
        property(&v2, "WorkGraphTaskAssignment", "version"),
        &ElementValue::Integer(2)
    );
    assert_eq!(
        property(&v2, "WorkGraphTaskAssignment", "workerId"),
        &ElementValue::from(&json!("validator-1"))
    );
    assert_eq!(
        property(&v2, "WorkGraphTaskAssignment", "agentProfile"),
        &ElementValue::from(&json!("issue-validator"))
    );
    for relation in ["COMMENT_ON", "ASSIGNMENT_FOR", "ASSIGNED_TO"] {
        assert!(
            v2.iter()
                .any(|change| label(change) == relation && is_insert(change)),
            "missing {relation}"
        );
    }
    // ASSIGNED_TO targets the stable worker identity even when the worker is
    // not (yet) configured: the Source validates the profile, not membership.
    assert_eq!(
        ids_with_label(&v2, "ASSIGNED_TO"),
        vec!["ASSIGNED_TO:IC_assignment:workgraph-worker:validator-1"]
    );
}

#[test]
fn assignment_v2_requires_exactly_agent_profile_and_worker_id() {
    for body in [
        // Missing workerId.
        "WorkGraphTaskAssignment/v2\n\n```json\n{\n  \"agentProfile\": \"issue-validator\"\n}\n```\n",
        // Extra field.
        "WorkGraphTaskAssignment/v2\n\n```json\n{\n  \"agentProfile\": \"issue-validator\",\n  \"workerId\": \"validator-1\",\n  \"slots\": 1\n}\n```\n",
        // Empty and whitespace-bearing worker IDs.
        "WorkGraphTaskAssignment/v2\n\n```json\n{\n  \"agentProfile\": \"issue-validator\",\n  \"workerId\": \"\"\n}\n```\n",
        "WorkGraphTaskAssignment/v2\n\n```json\n{\n  \"agentProfile\": \"issue-validator\",\n  \"workerId\": \"validator 1\"\n}\n```\n",
        // Unsupported profile.
        "WorkGraphTaskAssignment/v2\n\n```json\n{\n  \"agentProfile\": \"issue-risk-profiler\",\n  \"workerId\": \"validator-1\"\n}\n```\n",
        // Wrong type.
        "WorkGraphTaskAssignment/v2\n\n```json\n{\n  \"agentProfile\": \"issue-validator\",\n  \"workerId\": 1\n}\n```\n",
        // v1 shape under the v2 marker is still rejected; so is v2 under v1.
        "WorkGraphTaskAssignment/v1\n\n```json\n{\n  \"agentProfile\": \"issue-validator\",\n  \"workerId\": \"validator-1\"\n}\n```\n",
        // Unsupported version.
        "WorkGraphTaskAssignment/v3\n\n```json\n{\n  \"agentProfile\": \"issue-validator\"\n}\n```\n",
    ] {
        assert!(
            matches!(classify_comment(body), CommentClassification::Invalid(_)),
            "expected rejection for: {body}"
        );
    }
}

#[test]
fn lease_rejects_invalid_ids_timestamps_and_orderings() {
    let lease = |body: &str| format!("WorkGraphTaskLease/v1\n\n```json\n{body}\n```\n");
    let valid = |acquired: &str, expires: &str| {
        format!(
            "{{\n  \"leaseId\": \"{LEASE_ID}\",\n  \"assignmentCommentNodeId\": \"IC_assignment\",\
             \n  \"workerId\": \"validator-1\",\n  \"slotId\": \"validator-1/1\",\n  \
             \"acquiredAt\": \"{acquired}\",\n  \"expiresAt\": \"{expires}\"\n}}"
        )
    };
    assert!(matches!(
        classify_comment(&lease(&valid(
            "2026-08-19T22:00:00Z",
            "2026-08-19T22:15:00Z"
        ))),
        CommentClassification::Lease(_)
    ));

    for body in [
        // Non-UTC and non-RFC-3339 timestamps.
        valid("2026-08-19T22:00:00+02:00", "2026-08-19T22:15:00Z"),
        valid("2026-08-19T22:00:00Z", "2026-08-19 22:15:00Z"),
        valid("2026-08-19T22:00:00Z", "not-a-time"),
        // acquiredAt must be strictly earlier than expiresAt.
        valid("2026-08-19T22:15:00Z", "2026-08-19T22:15:00Z"),
        valid("2026-08-19T22:30:00Z", "2026-08-19T22:15:00Z"),
    ] {
        assert!(
            matches!(
                classify_comment(&lease(&body)),
                CommentClassification::Invalid(error) if error.code == error_code::INVALID_LEASE_PAYLOAD
            ),
            "expected payload rejection for: {body}"
        );
    }

    for body in [
        // Missing and unknown fields.
        format!("{{\n  \"leaseId\": \"{LEASE_ID}\"\n}}"),
        format!(
            "{{\n  \"leaseId\": \"{LEASE_ID}\",\n  \"assignmentCommentNodeId\": \"IC_assignment\",\
             \n  \"workerId\": \"validator-1\",\n  \"slotId\": \"validator-1/1\",\n  \
             \"acquiredAt\": \"2026-08-19T22:00:00Z\",\n  \"expiresAt\": \
             \"2026-08-19T22:15:00Z\",\n  \"extra\": true\n}}"
        ),
        // Empty opaque identifiers.
        valid("2026-08-19T22:00:00Z", "2026-08-19T22:15:00Z").replace("validator-1/1", ""),
    ] {
        assert!(
            matches!(
                classify_comment(&lease(&body)),
                CommentClassification::Invalid(_)
            ),
            "expected rejection for: {body}"
        );
    }

    // Non-canonical formatting and an unsupported version stay rejected.
    assert!(matches!(
        classify_comment("WorkGraphTaskLease/v1\n\n```json\n{\"leaseId\":\"x\"}\n```\n"),
        CommentClassification::Invalid(_)
    ));
    assert!(matches!(
        classify_comment("WorkGraphTaskLease/v2\n\n```json\n{}\n```\n"),
        CommentClassification::Invalid(error) if error.code == error_code::UNSUPPORTED_VERSION
    ));
}

#[test]
fn shared_lease_writer_round_trips_through_the_authoritative_source_classifier() {
    let lease = TaskLease {
        lease_id: LEASE_ID.to_string(),
        assignment_comment_node_id: "IC_assignment".to_string(),
        worker_id: "validator-1".to_string(),
        slot_id: "validator-1/1".to_string(),
        acquired_at: "2026-08-19T22:00:00Z".to_string(),
        expires_at: "2026-08-19T22:15:00Z".to_string(),
    };
    let body = canonical_task_lease_body(&lease).unwrap();
    assert!(body.ends_with("```\n"));
    assert_eq!(body.matches("```\n").count(), 1);
    assert!(matches!(
        classify_comment(&body),
        CommentClassification::Lease(parsed) if *parsed == lease
    ));
}

#[test]
fn result_v2_requires_exactly_its_five_top_level_fields() {
    let result = |body: &str| format!("WorkGraphTaskResult/v2\n\n```json\n{body}\n```\n");
    let criteria = "{\n    \"criteria\": [\n      {\n        \"criterion\": \"c\",\n        \
                    \"passed\": true,\n        \"evidence\": \"e\"\n      }\n    ]\n  }";
    for body in [
        // Missing leaseId.
        format!(
            "{{\n  \"taskType\": \"validate-issue\",\n  \"outcome\": \"succeeded\",\n  \
             \"summary\": \"s\",\n  \"result\": {criteria}\n}}"
        ),
        // Extra field.
        format!(
            "{{\n  \"taskType\": \"validate-issue\",\n  \"leaseId\": \"{LEASE_ID}\",\n  \
             \"outcome\": \"succeeded\",\n  \"summary\": \"s\",\n  \"result\": {criteria},\n  \
             \"workerId\": \"validator-1\"\n}}"
        ),
        // Empty leaseId.
        format!(
            "{{\n  \"taskType\": \"validate-issue\",\n  \"leaseId\": \"\",\n  \"outcome\": \
             \"succeeded\",\n  \"summary\": \"s\",\n  \"result\": {criteria}\n}}"
        ),
        // Task-specific schema still enforced under v2.
        format!(
            "{{\n  \"taskType\": \"validate-issue\",\n  \"leaseId\": \"{LEASE_ID}\",\n  \
             \"outcome\": \"succeeded\",\n  \"summary\": \"s\",\n  \"result\": {{\n    \
             \"criteria\": []\n  }}\n}}"
        ),
        // A v1 body under the v2 marker and a v2 body under the v1 marker.
        "{\n  \"taskType\": \"request-info\",\n  \"outcome\": \"succeeded\",\n  \"summary\": \
         \"s\",\n  \"result\": {\n    \"requestCommentNodeId\": \"IC_request\"\n  }\n}"
            .to_string(),
    ] {
        assert!(
            matches!(
                classify_comment(&result(&body)),
                CommentClassification::Invalid(_)
            ),
            "expected rejection for: {body}"
        );
    }
    // leaseId under the historical v1 marker is not accepted.
    assert!(matches!(
        classify_comment(&format!(
            "WorkGraphTaskResult/v1\n\n```json\n{{\n  \"taskType\": \"request-info\",\n  \
             \"leaseId\": \"{LEASE_ID}\",\n  \"outcome\": \"succeeded\",\n  \"summary\": \"s\",\n  \
             \"result\": {{\n    \"requestCommentNodeId\": \"IC_request\"\n  }}\n}}\n```\n"
        )),
        CommentClassification::Invalid(_)
    ));
    // Request-info under v2 remains valid.
    assert!(matches!(
        classify_comment(&result(&format!(
            "{{\n  \"taskType\": \"request-info\",\n  \"leaseId\": \"{LEASE_ID}\",\n  \"outcome\": \
             \"succeeded\",\n  \"summary\": \"s\",\n  \"result\": {{\n    \
             \"requestCommentNodeId\": \"IC_request\"\n  }}\n}}"
        ))),
        CommentClassification::Result(_)
    ));
}

#[test]
fn worker_queue_markers_stay_mutually_exclusive_and_error_when_malformed() {
    assert!(matches!(
        classify_comment(ASSIGNMENT_V2),
        CommentClassification::Assignment(_)
    ));
    assert!(matches!(
        classify_comment(LEASE),
        CommentClassification::Lease(_)
    ));
    assert!(matches!(
        classify_comment(LEASE_EXPIRATION),
        CommentClassification::LeaseExpiration(_)
    ));
    assert!(matches!(
        classify_comment(RESULT_V2),
        CommentClassification::Result(_)
    ));
    // `WorkGraphTaskLeaseExpiration/` must never be read as `WorkGraphTaskLease/`.
    assert!(matches!(
        classify_comment(LEASE_EXPIRATION),
        CommentClassification::LeaseExpiration(_)
    ));
    // Prose that merely mentions a marker stays an ordinary comment.
    for body in [
        "see WorkGraphTaskLease/v1 for details",
        "WorkGraphTaskLeaseExpirations/v1 is not a marker",
    ] {
        assert!(
            matches!(classify_comment(body), CommentClassification::Ordinary),
            "expected ordinary for: {body}"
        );
    }

    // A marked but malformed worker-queue comment on a task becomes an error
    // node bound by ERROR_ON, never a partial success-shaped artifact.
    for (body, id_suffix) in [
        (
            "WorkGraphTaskLease/v1\n\n```json\n{}\n```\n",
            "IC_bad_lease",
        ),
        (
            "WorkGraphTaskLeaseExpiration/v1\n\n```json\n{}\n```\n",
            "IC_bad_expiration",
        ),
        (
            "WorkGraphTaskAssignment/v2\n\n```json\n{}\n```\n",
            "IC_bad_assignment",
        ),
        (
            "WorkGraphTaskResult/v2\n\n```json\n{}\n```\n",
            "IC_bad_result",
        ),
    ] {
        let changes = convert(
            "issue_comment",
            &comment_event("created", body, "open", true, id_suffix),
        );
        assert!(
            changes
                .iter()
                .any(|change| label(change) == "WorkGraphError" && is_insert(change)),
            "expected WorkGraphError for: {body}"
        );
        assert!(changes.iter().any(|change| label(change) == "ERROR_ON"));
        for forbidden in [
            "WorkGraphTaskLease",
            "WorkGraphTaskLeaseAnchor",
            "WorkGraphTaskLeaseExpiration",
            "WorkGraphTaskAssignment",
            "WorkGraphTaskResult",
        ] {
            assert!(
                !changes.iter().any(|change| label(change) == forbidden),
                "{forbidden} must not be projected for: {body}"
            );
        }
    }
}

#[tokio::test]
async fn capacity_query_joins_workers_slots_and_assignments() {
    let registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(registry.clone()));
    let query = QueryBuilder::new(
        "MATCH (worker:WorkGraphWorker)-[:HAS_SLOT]->(slot:WorkGraphWorkerSlot) \
         WHERE slot.enabled = true \
         RETURN worker.workerId AS workerId, worker.configuredSlotCount AS configuredSlotCount, \
         slot.slotId AS slotId",
        parser,
    )
    .with_function_registry(registry)
    .build()
    .await;

    let projected = process_changes(&query, project_workers(WORKER_FILE)).await;
    // validator-1 offers two slots, info-requester-1 offers one.
    assert_eq!(additions(&projected), 3);

    // Reducing validator-1 to one slot withdraws exactly the excess slot.
    let reduced = "version: 1\nworkers:\n  - workerId: validator-1\n    agentProfile: \
                   issue-validator\n    slots: 1\n    leaseDuration: PT15M\n  - workerId: \
                   info-requester-1\n    agentProfile: issue-info-requester\n    slots: 1\n    \
                   leaseDuration: PT15M\n";
    let retiring = BTreeMap::from([("validator-1".to_string(), 2)]);
    let after = process_changes(
        &query,
        project_workers_with(reduced, &retiring, &BTreeMap::new()),
    )
    .await;
    assert_eq!(removals(&after), 1);
    assert_eq!(additions(&after), 0);
}

#[tokio::test]
async fn assignment_to_worker_queue_query_distinguishes_v1_from_v2() {
    let registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(registry.clone()));
    let query = QueryBuilder::new(
        "MATCH (assignment:WorkGraphTaskAssignment)-[:ASSIGNED_TO]->(worker:WorkGraphWorker) \
         WHERE assignment.version = 2 \
         RETURN worker.workerId AS workerId, assignment.sourceCommentNodeId AS assignmentId",
        parser,
    )
    .with_function_registry(registry)
    .build()
    .await;

    process_changes(&query, project_workers(WORKER_FILE)).await;
    // A historical v1 Assignment names no worker queue and never matches.
    let v1 = process_changes(
        &query,
        convert(
            "issue_comment",
            &comment_event("created", ASSIGNMENT, "open", true, "IC_v1_assignment"),
        ),
    )
    .await;
    assert_eq!(additions(&v1), 0);

    let v2 = process_changes(
        &query,
        convert(
            "issue_comment",
            &comment_event("created", ASSIGNMENT_V2, "open", true, "IC_v2_assignment"),
        ),
    )
    .await;
    assert_eq!(additions(&v2), 1);
}

#[test]
fn push_relevance_is_exact_and_conservative_when_truncated() {
    let path = ".github/workgraph/workers.yaml";
    let commit = |key: &str, entry: &str| json!({ key: [entry] });

    for key in ["added", "modified", "removed"] {
        let payload = json!({ "commits": [commit(key, path)] });
        assert!(push_touches_worker_file(&payload, path), "{key}");
    }
    // An unrelated path is ignored.
    assert!(!push_touches_worker_file(
        &json!({ "commits": [commit("modified", "README.md")] }),
        path
    ));
    // A push with no commits at all changed nothing relevant.
    assert!(!push_touches_worker_file(&json!({ "commits": [] }), path));
    // The head commit is inspected alongside the commit list.
    assert!(push_touches_worker_file(
        &json!({ "commits": [], "head_commit": commit("added", path) }),
        path
    ));
    // GitHub truncates large pushes; an unprovable push converges instead of
    // silently leaving stale capacity behind.
    assert!(push_touches_worker_file(
        &json!({ "commits": [commit("modified", "README.md")], "size": 25 }),
        path
    ));
    // A commit array at GitHub's delivery cap is likewise unprovable, even
    // without a `size` field to compare against.
    let capped: Vec<Value> = (0..20).map(|_| commit("modified", "README.md")).collect();
    assert!(push_touches_worker_file(
        &json!({ "commits": capped }),
        path
    ));
    // Just below the cap and consistent with `size`, the payload is trusted.
    let small: Vec<Value> = (0..19).map(|_| commit("modified", "README.md")).collect();
    assert!(!push_touches_worker_file(
        &json!({ "commits": small, "size": 19 }),
        path
    ));
    // A payload without a commit list is likewise unprovable.
    assert!(push_touches_worker_file(&json!({}), path));

    // A branch create, delete, or force-push rewrites what the ref resolves to
    // without necessarily naming the file in any commit, so all three converge.
    for flag in ["created", "deleted", "forced"] {
        let payload = json!({
            "commits": [commit("modified", "README.md")],
            "size": 1,
            flag: true
        });
        assert!(
            push_touches_worker_file(&payload, path),
            "a {flag} push must converge"
        );
        // The same payload with the flag explicitly false stays irrelevant.
        let payload = json!({
            "commits": [commit("modified", "README.md")],
            "size": 1,
            flag: false
        });
        assert!(
            !push_touches_worker_file(&payload, path),
            "a non-{flag} push about another file must not converge"
        );
    }
}

#[test]
fn worker_config_is_validated_and_its_token_is_redacted() {
    let mut config = GitHubWorkGraphSourceConfig {
        organization: "acme".to_string(),
        task_issue_type: task_type(),
        repositories: vec![],
        lease_trust: None,
        worker_config: Some(WorkerConfig {
            repository: "acme/widgets".to_string(),
            r#ref: "main".to_string(),
            path: ".github/workgraph/workers.yaml".to_string(),
            token: "read-only-token".to_string(),
            api_base_url: DEFAULT_WORKER_API_BASE_URL.to_string(),
        }),
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

    let source = crate::GitHubWorkGraphSourceBuilder::new("gh")
        .with_config(config.clone())
        .build()
        .unwrap();
    let properties = drasi_lib::sources::Source::properties(&source);
    let worker = properties
        .get("workerConfig")
        .and_then(Value::as_object)
        .expect("workerConfig is reported");
    assert_eq!(worker.get("token"), Some(&json!("[REDACTED]")));
    assert_eq!(worker.get("repository"), Some(&json!("acme/widgets")));

    for mutate in [
        |worker: &mut WorkerConfig| worker.token.clear(),
        |worker: &mut WorkerConfig| worker.repository = "widgets".to_string(),
        |worker: &mut WorkerConfig| worker.path = "/etc/passwd".to_string(),
        |worker: &mut WorkerConfig| worker.r#ref.clear(),
        |worker: &mut WorkerConfig| worker.api_base_url.clear(),
    ] {
        let mut broken = config.clone();
        mutate(broken.worker_config.as_mut().unwrap());
        assert!(broken.validate().is_err());
    }

    // Omitting the worker file entirely stays valid; the queue is simply off.
    config.worker_config = None;
    assert!(config.validate().is_ok());
}

#[test]
fn worker_queue_timestamps_require_the_exact_canonical_utc_form() {
    let lease = |acquired: &str| {
        format!(
            "WorkGraphTaskLease/v1\n\n```json\n{{\n  \"leaseId\": \"{LEASE_ID}\",\n  \
             \"assignmentCommentNodeId\": \"IC_assignment\",\n  \"workerId\": \"validator-1\",\n  \
             \"slotId\": \"validator-1/1\",\n  \"acquiredAt\": \"{acquired}\",\n  \"expiresAt\": \
             \"2026-08-19T23:00:00Z\"\n}}\n```\n"
        )
    };
    assert!(matches!(
        classify_comment(&lease("2026-08-19T22:00:00Z")),
        CommentClassification::Lease(_)
    ));
    for acquired in [
        // Separator, case, offset, precision, and truncation variants all
        // spell the same instant differently and are therefore rejected.
        "2026-08-19 22:00:00Z",
        "2026-08-19t22:00:00Z",
        "2026-08-19T22:00:00z",
        "2026-08-19T22:00:00.000Z",
        "2026-08-19T22:00:00+00:00",
        "2026-08-19T22:00:00",
        "2026-08-19T22:00Z",
        "26-08-19T22:00:00Z",
        "2026-13-19T22:00:00Z",
        "2026-08-32T22:00:00Z",
        "2026-08-19T25:00:00Z",
    ] {
        assert!(
            matches!(
                classify_comment(&lease(acquired)),
                CommentClassification::Invalid(error)
                    if error.code == error_code::INVALID_LEASE_PAYLOAD
            ),
            "expected rejection for: {acquired}"
        );
    }
}

#[tokio::test]
async fn worker_file_client_separates_unreadable_from_rejected_configurations() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let blob = |text: &str, truncated: bool, binary: bool, size: u64| {
        json!({"data":{"repository":{"object":{
            "__typename": "Blob", "oid": "blob-oid", "text": text,
            "byteSize": size, "isTruncated": truncated, "isBinary": binary
        }}}})
    };

    // A complete text blob is returned with its provenance.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(blob(
            WORKER_FILE,
            false,
            false,
            128,
        )))
        .mount(&server)
        .await;
    let client = WorkerFileClient::new("token", &format!("{}/graphql", server.uri())).unwrap();
    let content = client.fetch(&worker_location()).await.unwrap();
    assert_eq!(content.text, WORKER_FILE);
    assert_eq!(content.oid, "blob-oid");
    assert!(parse_worker_file(&content.text).is_ok());

    // A missing repository, a missing object, and a non-Blob object are all
    // deterministic configuration rejections, not transport failures.
    for body in [
        json!({"data":{"repository": Value::Null}}),
        json!({"data":{"repository":{"object": Value::Null}}}),
        json!({"data":{"repository":{"object":{"__typename":"Tree"}}}}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let client = WorkerFileClient::new("token", &format!("{}/graphql", server.uri())).unwrap();
        assert!(
            matches!(
                client.fetch(&worker_location()).await,
                Err(WorkerFileError::Rejected(_))
            ),
            "expected rejection for: {body}"
        );
    }

    // Oversized, truncated, and binary blobs are rejected as unsafe sizes.
    for body in [
        blob(WORKER_FILE, false, false, 512 * 1024),
        blob(WORKER_FILE, true, false, 128),
        blob(WORKER_FILE, false, true, 128),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let client = WorkerFileClient::new("token", &format!("{}/graphql", server.uri())).unwrap();
        assert!(matches!(
            client.fetch(&worker_location()).await,
            Err(WorkerFileError::Rejected(_))
        ));
    }

    // Authentication failures and GraphQL errors are unreadable, not empty.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let client = WorkerFileClient::new("token", &format!("{}/graphql", server.uri())).unwrap();
    assert!(matches!(
        client.fetch(&worker_location()).await,
        Err(WorkerFileError::Unavailable(_))
    ));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"errors":[{"message":"Resource not accessible by integration"}]}),
        ))
        .mount(&server)
        .await;
    let client = WorkerFileClient::new("token", &format!("{}/graphql", server.uri())).unwrap();
    assert!(matches!(
        client.fetch(&worker_location()).await,
        Err(WorkerFileError::Unavailable(_))
    ));
}

// ---------------------------------------------------------------------------
// Lease lifecycle: trust gating, the immutable anchor, and exact active counts.
// ---------------------------------------------------------------------------

/// A task comment on `task_id` authored by `login`, so a test can distinguish a
/// configured dispatcher or reporter from any other commenter.
fn comment_event_by(
    action: &str,
    body: &str,
    comment_id: &str,
    task_id: &str,
    login: &str,
) -> Value {
    let mut event = comment_event(action, body, "open", true, comment_id);
    event["issue"]["node_id"] = json!(task_id);
    event["comment"]["user"] = json!({
        "login": login,
        "node_id": format!("U_{login}"),
        "id": 7,
        "type": "User"
    });
    event
}

fn lease_body(lease_id: &str, slot: &str) -> String {
    LEASE
        .replace(LEASE_ID, lease_id)
        .replace("validator-1/1", slot)
}

fn assignment_v2_on(task_id: &str, comment_id: &str) -> Vec<SourceChange> {
    convert(
        "issue_comment",
        &comment_event_by("created", ASSIGNMENT_V2, comment_id, task_id, "bot"),
    )
}

async fn build_query(text: &str) -> ContinuousQuery {
    let registry = Arc::new(FunctionRegistry::new());
    drasi_functions_cypher::register_default_cypher_functions(&registry);
    let parser = Arc::new(CypherParser::new(registry.clone()));
    QueryBuilder::new(text, parser)
        .with_function_registry(registry)
        .build()
        .await
}

/// The latest value of `key` in any row whose grouping matches
/// `group_key = group_value`.
fn aggregate_value(
    results: &[QueryPartEvaluationContext],
    group_key: &str,
    group_value: &str,
    key: &str,
) -> Option<VariableValue> {
    results.iter().rev().find_map(|result| {
        let after = match result {
            QueryPartEvaluationContext::Adding { after, .. }
            | QueryPartEvaluationContext::Updating { after, .. }
            | QueryPartEvaluationContext::Aggregation { after, .. } => after,
            _ => return None,
        };
        let matches = after
            .get(group_key)
            .is_some_and(|value| value == &VariableValue::String(group_value.to_string()));
        matches.then(|| after.get(key).cloned()).flatten()
    })
}

/// The canonical active-lease query. One positive match, no `OPTIONAL MATCH`,
/// no subtraction: only a trusted Lease has a `LEASE_ANCHOR` edge, and only a
/// trusted end can clear `isActive`.
const ACTIVE_LEASES: &str =
    "MATCH (lease:WorkGraphTaskLease)-[:LEASE_ANCHOR]->(anchor:WorkGraphTaskLeaseAnchor) \
     WHERE anchor.isActive = true \
     RETURN lease.workerId AS workerId, count(lease) AS activeLeaseCount";

fn lease_event(task: &str, comment: &str, lease_id: &str, slot: &str, login: &str) -> Value {
    comment_event_by("created", &lease_body(lease_id, slot), comment, task, login)
}

fn end_event(task: &str, comment: &str, body: &str, login: &str) -> Value {
    comment_event_by("created", body, comment, task, login)
}

/// Assert an anchor's fully recomputed lifecycle.
fn assert_anchor(
    world: &LeaseWorld,
    is_active: bool,
    end_reason: &str,
    end_comment: Option<&str>,
    context: &str,
) {
    let state = world
        .anchor()
        .unwrap_or_else(|| panic!("{context}: no anchor"));
    assert_eq!(state.is_active, is_active, "{context}: isActive");
    assert_eq!(state.end_reason, end_reason, "{context}: endReason");
    assert_eq!(
        state.end_comment_node_id.as_deref(),
        end_comment,
        "{context}: endCommentNodeId"
    );
}

#[test]
fn lease_trust_configuration_is_strictly_validated() {
    let identity = |id: &str, login: &str| TrustedIdentity {
        id: id.to_string(),
        login: login.to_string(),
    };
    let valid = LeaseTrust {
        dispatchers: vec![identity("U_a", "a")],
        reporters: vec![identity("U_b", "b")],
    };
    assert!(valid.validate().is_ok());

    for broken in [
        LeaseTrust {
            dispatchers: vec![],
            reporters: vec![identity("U_b", "b")],
        },
        LeaseTrust {
            dispatchers: vec![identity("U_a", "a")],
            reporters: vec![],
        },
        LeaseTrust {
            dispatchers: vec![identity("", "a")],
            reporters: vec![identity("U_b", "b")],
        },
        LeaseTrust {
            dispatchers: vec![identity("U_a", " a")],
            reporters: vec![identity("U_b", "b")],
        },
        LeaseTrust {
            dispatchers: vec![identity("U_a", "a"), identity("U_a", "other")],
            reporters: vec![identity("U_b", "b")],
        },
    ] {
        assert!(broken.validate().is_err());
    }

    // Both the node ID and the login must match, so a renamed account loses
    // trust instead of silently inheriting it.
    let author = |id: &str, login: &str| json!({ "node_id": id, "login": login });
    assert!(valid.is_dispatcher(Some(&author("U_a", "a"))));
    assert!(!valid.is_dispatcher(Some(&author("U_a", "renamed"))));
    assert!(!valid.is_dispatcher(Some(&author("U_other", "a"))));
    assert!(!valid.is_dispatcher(None));
    assert!(valid.is_reporter(Some(&author("U_b", "b"))));
    assert!(!valid.is_reporter(Some(&author("U_a", "a"))));
}

#[test]
fn lease_keeps_acquisition_facts_on_its_own_node_and_the_anchor_minimal() {
    let mut world = LeaseWorld::default();
    let changes = world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));

    // Every acquisition fact and the author live on the stable comment node.
    for (key, expected) in [
        ("sourceCommentNodeId", json!("IC_lease")),
        ("leaseId", json!(LEASE_ID)),
        ("assignmentCommentNodeId", json!("IC_assignment")),
        ("workerId", json!("validator-1")),
        ("slotId", json!("validator-1/1")),
        ("taskNodeId", json!("I_task")),
        ("acquiredAt", json!("2026-08-19T22:00:00Z")),
        ("expiresAt", json!("2026-08-19T22:15:00Z")),
        ("leaseAnchorNodeId", json!(LEASE_ANCHOR_ID)),
        ("authorId", json!("U_bot")),
        ("trusted", json!(true)),
    ] {
        assert_eq!(
            property(&changes, "WorkGraphTaskLease", key),
            &ElementValue::from(&expected),
            "unexpected {key}"
        );
    }
    for relation in ["COMMENT_ON", "LEASE_FOR", "LEASES_SLOT", "LEASE_ANCHOR"] {
        assert!(
            changes.iter().any(|change| label(change) == relation),
            "missing {relation}"
        );
    }

    // The anchor carries only its key and the recomputed lifecycle.
    assert_eq!(
        node_property(&changes, LEASE_ANCHOR_ID, "isActive"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&changes, LEASE_ANCHOR_ID, "endReason"),
        &ElementValue::from(&json!("none"))
    );
    assert_eq!(
        node_property(&changes, LEASE_ANCHOR_ID, "acquisitionCount"),
        &ElementValue::Integer(1)
    );
    for acquisition in ["workerId", "slotId", "acquiredAt", "leaseCommentNodeId"] {
        assert!(
            node_property_opt(&changes, LEASE_ANCHOR_ID, acquisition).is_none(),
            "the anchor must not carry the acquisition fact {acquisition}"
        );
    }
}

#[test]
fn only_configured_identities_can_move_the_lease_lifecycle() {
    // An untrusted author's Lease is projected with its provenance but reaches
    // no anchor, so it can never occupy capacity.
    let mut world = LeaseWorld::default();
    let squat = world.deliver(&lease_event(
        "I_task",
        "IC_squat",
        LEASE_ID,
        "validator-1/1",
        "attacker",
    ));
    assert_eq!(
        property(&squat, "WorkGraphTaskLease", "trusted"),
        &ElementValue::Bool(false)
    );
    assert!(squat.iter().any(|change| label(change) == "LEASE_FOR"));
    assert!(!squat.iter().any(|change| label(change) == "LEASE_ANCHOR"));
    assert!(world.anchor().is_none());

    // An untrusted author's Result and Expiration bind nothing and end nothing.
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    for (body, comment) in [(RESULT_V2, "IC_evil_r"), (LEASE_EXPIRATION, "IC_evil_x")] {
        let changes = world.deliver(&end_event("I_task", comment, body, "attacker"));
        assert_eq!(
            node_property(&changes, comment, "trusted"),
            &ElementValue::Bool(false)
        );
        for relation in ["RESULT_FOR_LEASE", "EXPIRES_LEASE"] {
            assert!(
                !changes.iter().any(|change| label(change) == relation),
                "{comment} must not emit {relation}"
            );
        }
        assert_anchor(&world, true, "none", None, comment);
    }

    // With no configured trust at all nothing is trusted: fail closed.
    let unconfigured = convert_untrusted(
        "issue_comment",
        &comment_event("created", LEASE, "open", true, "IC_lease"),
    );
    assert_eq!(
        property(&unconfigured, "WorkGraphTaskLease", "trusted"),
        &ElementValue::Bool(false)
    );
}

#[test]
fn an_untrusted_editor_cannot_rewrite_a_trusted_comment_into_a_lifecycle_artifact() {
    // GitHub preserves `comment.user` across an edit, so trusting the author
    // alone would let anyone with edit rights turn a trusted author's ordinary
    // comment into a Lease, a Result, or an Expiration.
    for (body, marker, author) in [
        (LEASE, "WorkGraphTaskLease", "bot"),
        (RESULT_V2, "RESULT_FOR_LEASE", "reporter"),
        (LEASE_EXPIRATION, "EXPIRES_LEASE", "reporter"),
    ] {
        // The webhook reports the acting identity as the delivery `sender`.
        let mut edited = comment_event_by("edited", body, "IC_target", "I_task", author);
        edited["changes"] = json!({ "body": { "from": "an ordinary note" } });
        edited["sender"] = json!({"login": "attacker", "node_id": "U_attacker"});

        let mut world = LeaseWorld::default();
        let changes = world.deliver(&edited);
        assert!(
            !changes.iter().any(|change| label(change) == "LEASE_ANCHOR"
                || label(change) == "RESULT_FOR_LEASE"
                || label(change) == "EXPIRES_LEASE"),
            "an untrusted editor produced a lifecycle binding for {marker}"
        );
        assert!(
            world.anchor().is_none(),
            "an untrusted editor moved the lifecycle for {marker}"
        );
        // The edit is still visible, with the editor recorded.
        assert_eq!(
            node_property(&changes, "IC_target", "editorLogin"),
            &ElementValue::from(&json!("attacker"))
        );

        // A trusted editor of the same comment is accepted.
        let mut trusted_edit = edited.clone();
        trusted_edit["sender"] = json!({"login": author, "node_id": format!("U_{author}")});
        let mut world = LeaseWorld::default();
        let changes = world.deliver(&trusted_edit);
        assert!(
            changes.iter().any(|change| label(change) == "LEASE_ANCHOR"
                || label(change) == "RESULT_FOR_LEASE"
                || label(change) == "EXPIRES_LEASE"),
            "a trusted editor was rejected for {marker}"
        );
    }
}

#[test]
fn a_bootstrap_shaped_editor_identity_is_checked_the_same_way() {
    // Bootstrap has no `sender`; it projects GitHub's `editor` on the comment.
    // An absent editor is fine, an untrusted one removes trust.
    for (editor, expect_trusted) in [
        (Value::Null, true),
        (json!({"login": "bot", "node_id": "U_bot"}), true),
        (json!({"login": "attacker", "node_id": "U_attacker"}), false),
    ] {
        let mut event = comment_event("created", LEASE, "open", true, "IC_lease");
        event["comment"]["editor"] = editor.clone();
        let mut world = LeaseWorld::default();
        let changes = world.deliver(&event);
        assert_eq!(
            property(&changes, "WorkGraphTaskLease", "trusted"),
            &ElementValue::Bool(expect_trusted),
            "unexpected trust for editor {editor}"
        );
        assert_eq!(world.anchor().is_some(), expect_trusted);
    }
}

#[test]
fn result_v1_is_preserved_and_v2_binds_and_ends_its_exact_lease() {
    let v1 = convert(
        "issue_comment",
        &comment_event("created", RESULT, "open", true, "IC_result"),
    );
    assert_eq!(
        property(&v1, "WorkGraphTaskResult", "version"),
        &ElementValue::Integer(1)
    );
    assert_eq!(
        property(&v1, "WorkGraphTaskResult", "leaseId"),
        &ElementValue::Null
    );
    assert!(v1.iter().any(|change| label(change) == "RESULT_FOR"));
    assert!(!v1.iter().any(|change| label(change) == "RESULT_FOR_LEASE"));

    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    let v2 = world.deliver(&end_event("I_task", "IC_result", RESULT_V2, "reporter"));

    assert_eq!(
        property(&v2, "WorkGraphTaskResult", "version"),
        &ElementValue::Integer(2)
    );
    assert_eq!(
        property(&v2, "WorkGraphTaskResult", "trusted"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        ids_with_label(&v2, "RESULT_FOR_LEASE"),
        vec![format!("RESULT_FOR_LEASE:IC_result:{LEASE_ANCHOR_ID}")]
    );
    assert_anchor(&world, false, "result", Some("IC_result"), "v2 result");
    // A Result's authoritative end instant is its own comment timestamp.
    assert_eq!(
        node_property(&v2, LEASE_ANCHOR_ID, "endedAt"),
        &ElementValue::from(&json!("2026-01-03T00:00:00Z"))
    );
}

#[test]
fn lease_expiration_ends_only_the_lease_comment_it_names() {
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));

    // A stale reference to some other Lease comment stays projected but cannot
    // end this lease.
    let stale = LEASE_EXPIRATION.replace("IC_lease", "IC_some_other_lease");
    let changes = world.deliver(&end_event("I_task", "IC_stale_expiry", &stale, "reporter"));
    assert_eq!(
        property(
            &changes,
            "WorkGraphTaskLeaseExpiration",
            "leaseCommentNodeId"
        ),
        &ElementValue::from(&json!("IC_some_other_lease"))
    );
    assert!(changes
        .iter()
        .any(|change| label(change) == "EXPIRES_LEASE"));
    assert_anchor(&world, true, "none", None, "stale expiration");
    assert_eq!(
        world.anchor().unwrap().end_claim_count,
        1,
        "the stale claim is still recorded"
    );

    // The matching Expiration does end it.
    let changes = world.deliver(&end_event(
        "I_task",
        "IC_expiry",
        LEASE_EXPIRATION,
        "reporter",
    ));
    assert_anchor(
        &world,
        false,
        "expired",
        Some("IC_expiry"),
        "matching expiration",
    );
    assert_eq!(
        node_property(&changes, LEASE_ANCHOR_ID, "endedAt"),
        &ElementValue::from(&json!("2026-08-19T22:15:00Z"))
    );
}

#[test]
fn duplicate_and_mixed_ends_apply_once_and_deterministically() {
    // A Result at 2026-01-03T00:00:00Z and an Expiration at
    // 2026-08-19T22:15:00Z: the earliest authoritative end always wins,
    // whichever order the deliveries arrive in.
    for order in [
        vec![("IC_result", RESULT_V2), ("IC_expiry", LEASE_EXPIRATION)],
        vec![("IC_expiry", LEASE_EXPIRATION), ("IC_result", RESULT_V2)],
    ] {
        let mut world = LeaseWorld::default();
        world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
        for (comment, body) in &order {
            world.deliver(&end_event("I_task", comment, body, "reporter"));
        }
        assert_anchor(&world, false, "result", Some("IC_result"), "mixed ends");
        assert_eq!(world.anchor().unwrap().end_claim_count, 2);
    }

    // Duplicate ends of the same kind collapse onto one deterministic end.
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    for comment in ["IC_r2", "IC_r1"] {
        world.deliver(&end_event("I_task", comment, RESULT_V2, "reporter"));
    }
    // Same instant, so the stable comment node ID breaks the tie.
    assert_anchor(&world, false, "result", Some("IC_r1"), "duplicate results");
    assert_eq!(world.anchor().unwrap().end_claim_count, 2);
}

#[test]
fn removing_or_rekeying_an_end_restores_the_state_the_survivors_imply() {
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    world.deliver(&end_event("I_task", "IC_result", RESULT_V2, "reporter"));
    assert_anchor(&world, false, "result", Some("IC_result"), "ended");

    // Deleting the only end reactivates the lease: current state, not history.
    let changes = world.deliver(&comment_event_by(
        "deleted",
        RESULT_V2,
        "IC_result",
        "I_task",
        "reporter",
    ));
    assert_anchor(&world, true, "none", None, "end deleted");
    assert_eq!(
        node_property(&changes, LEASE_ANCHOR_ID, "isActive"),
        &ElementValue::Bool(true)
    );

    // With two ends, removing one leaves the other in force.
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    world.deliver(&end_event("I_task", "IC_result", RESULT_V2, "reporter"));
    world.deliver(&end_event(
        "I_task",
        "IC_expiry",
        LEASE_EXPIRATION,
        "reporter",
    ));
    world.deliver(&comment_event_by(
        "deleted",
        RESULT_V2,
        "IC_result",
        "I_task",
        "reporter",
    ));
    assert_anchor(
        &world,
        false,
        "expired",
        Some("IC_expiry"),
        "one end survives",
    );

    // Editing an end onto a different leaseId updates both anchors.
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    world.deliver(&end_event("I_task", "IC_result", RESULT_V2, "reporter"));
    let rekeyed = RESULT_V2.replace(LEASE_ID, "0198d8c4-7c28-7d43-a8dd-000000000000");
    let mut edited = comment_event_by("edited", &rekeyed, "IC_result", "I_task", "reporter");
    edited["changes"] = json!({ "body": { "from": RESULT_V2 } });
    edited["sender"] = json!({"login": "reporter", "node_id": "U_reporter"});
    let changes = world.deliver(&edited);
    assert_anchor(&world, true, "none", None, "end rekeyed away");
    // The anchor it moved to has no acquisition, so it is removed rather than
    // materialized as something a query could bind to.
    assert!(changes.iter().any(|change| is_delete(change)
        && id(change) == "workgraph-lease:I_task:0198d8c4-7c28-7d43-a8dd-000000000000"));
}

#[test]
fn re_observing_an_acquisition_never_resurrects_an_ended_lease() {
    for action in ["pinned", "unpinned", "edited"] {
        let mut world = LeaseWorld::default();
        world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
        world.deliver(&end_event("I_task", "IC_result", RESULT_V2, "reporter"));
        assert_anchor(&world, false, "result", Some("IC_result"), action);

        let mut event = comment_event("edited", LEASE, "open", true, "IC_lease");
        event["action"] = json!(action);
        if action == "edited" {
            event["changes"] = json!({ "body": { "from": LEASE } });
            event["sender"] = json!({"login": "bot", "node_id": "U_bot"});
        }
        let changes = world.deliver(&event);
        assert_anchor(&world, false, "result", Some("IC_result"), action);
        if let Some(value) = node_property_opt(&changes, LEASE_ANCHOR_ID, "isActive") {
            assert_eq!(
                value,
                &ElementValue::Bool(false),
                "{action} resurrected an ended lease"
            );
        }
    }

    // Redelivering the original acquisition is likewise a no-op.
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    world.deliver(&end_event("I_task", "IC_result", RESULT_V2, "reporter"));
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    assert_anchor(&world, false, "result", Some("IC_result"), "redelivery");
}

#[test]
fn duplicate_acquisitions_fail_closed_and_recover_when_one_is_removed() {
    let mut world = LeaseWorld::default();
    world.deliver(&lease_event(
        "I_task",
        "IC_lease_1",
        LEASE_ID,
        "validator-1/1",
        "bot",
    ));
    assert_anchor(&world, true, "none", None, "single acquisition");

    // A second trusted Lease claiming the same identity is ambiguous, so the
    // anchor fails closed rather than double-booking or silently rewriting.
    let changes = world.deliver(&lease_event(
        "I_task",
        "IC_lease_2",
        LEASE_ID,
        "validator-1/2",
        "bot",
    ));
    assert_anchor(&world, false, "conflict", None, "conflicting acquisitions");
    assert_eq!(
        node_property(&changes, LEASE_ANCHOR_ID, "acquisitionCount"),
        &ElementValue::Integer(2)
    );
    // Each Lease keeps its own facts; neither rewrote the other.
    assert_eq!(
        node_property(&changes, "IC_lease_2", "slotId"),
        &ElementValue::from(&json!("validator-1/2"))
    );
    assert!(!changes.iter().any(|change| id(change) == "IC_lease_1"));

    // Deleting one restores the state the survivor implies.
    world.deliver(&comment_event_by(
        "deleted",
        &lease_body(LEASE_ID, "validator-1/2"),
        "IC_lease_2",
        "I_task",
        "bot",
    ));
    assert_anchor(&world, true, "none", None, "conflict resolved");
    assert_eq!(world.anchor().unwrap().acquisition_count, 1);

    // A conflict that already has an end still resolves to that end.
    world.deliver(&lease_event(
        "I_task",
        "IC_lease_2",
        LEASE_ID,
        "validator-1/2",
        "bot",
    ));
    world.deliver(&end_event("I_task", "IC_result", RESULT_V2, "reporter"));
    assert_anchor(&world, false, "conflict", None, "conflict outranks end");
    world.deliver(&comment_event_by(
        "deleted",
        &lease_body(LEASE_ID, "validator-1/2"),
        "IC_lease_2",
        "I_task",
        "bot",
    ));
    assert_anchor(
        &world,
        false,
        "result",
        Some("IC_result"),
        "end after recovery",
    );
}

#[test]
fn removing_or_rekeying_an_acquisition_updates_both_anchors() {
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    world.deliver(&end_event("I_task", "IC_result", RESULT_V2, "reporter"));

    let rekeyed = LEASE.replace(LEASE_ID, "0198d8c4-7c28-7d43-a8dd-000000000000");
    let mut edited = comment_event("edited", &rekeyed, "open", true, "IC_lease");
    edited["changes"] = json!({ "body": { "from": LEASE } });
    edited["sender"] = json!({"login": "bot", "node_id": "U_bot"});
    let changes = world.deliver(&edited);

    // The anchor it left has no acquisition left, so it is deleted; the one it
    // joined is materialized fresh and active.
    assert!(world.anchor().is_none());
    assert!(changes
        .iter()
        .any(|change| is_delete(change) && id(change) == LEASE_ANCHOR_ID));
    let moved = "workgraph-lease:I_task:0198d8c4-7c28-7d43-a8dd-000000000000";
    assert_eq!(
        node_property(&changes, moved, "isActive"),
        &ElementValue::Bool(true)
    );

    // Deleting an acquisition removes its anchor entirely.
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    let changes = world.deliver(&comment_event("deleted", LEASE, "open", true, "IC_lease"));
    assert!(world.anchor().is_none());
    assert!(changes
        .iter()
        .any(|change| is_delete(change) && id(change) == LEASE_ANCHOR_ID));
}

#[test]
fn an_end_naming_an_unacquired_lease_materializes_no_anchor() {
    // Cross-task: a trusted reporter on another task naming this leaseId.
    let mut world = LeaseWorld::default();
    world.deliver(&lease_event(
        "I_task_a",
        "IC_lease_a",
        LEASE_ID,
        "validator-1/1",
        "bot",
    ));
    let cross = world.deliver(&end_event("I_task_b", "IC_cross", RESULT_V2, "reporter"));
    assert!(cross.iter().any(|change| id(change)
        == format!("RESULT_FOR_LEASE:IC_cross:workgraph-lease:I_task_b:{LEASE_ID}")));
    assert!(
        !cross
            .iter()
            .any(|change| !is_delete(change) && label(change) == "WorkGraphTaskLeaseAnchor"),
        "an unacquired lease must materialize no anchor"
    );
    let state = world
        .ledger
        .project(&format!("workgraph-lease:I_task_a:{LEASE_ID}"))
        .expect("task A keeps its anchor");
    assert!(state.is_active, "task A's lease must not be released");

    // Unknown leaseId on the right task.
    let unknown = RESULT_V2.replace(LEASE_ID, "0198d8c4-7c28-7d43-a8dd-ffffffffffff");
    let orphan = world.deliver(&end_event("I_task_a", "IC_orphan", &unknown, "reporter"));
    assert!(!orphan
        .iter()
        .any(|change| !is_delete(change) && label(change) == "WorkGraphTaskLeaseAnchor"));
}

#[tokio::test]
async fn active_lease_count_is_exact_for_every_ending_shape() {
    for (name, ends) in [
        ("result only", vec![("IC_r1", RESULT_V2)]),
        ("expiration only", vec![("IC_x1", LEASE_EXPIRATION)]),
        (
            "result then expiration",
            vec![("IC_r1", RESULT_V2), ("IC_x1", LEASE_EXPIRATION)],
        ),
        (
            "expiration then result",
            vec![("IC_x1", LEASE_EXPIRATION), ("IC_r1", RESULT_V2)],
        ),
        (
            "duplicate results",
            vec![("IC_r1", RESULT_V2), ("IC_r2", RESULT_V2)],
        ),
        (
            "duplicate expirations",
            vec![("IC_x1", LEASE_EXPIRATION), ("IC_x2", LEASE_EXPIRATION)],
        ),
    ] {
        let query = build_query(ACTIVE_LEASES).await;
        let mut world = LeaseWorld::default();
        process_changes(&query, project_workers(WORKER_FILE)).await;

        let acquired = process_changes(
            &query,
            world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease")),
        )
        .await;
        assert_eq!(
            aggregate_value(&acquired, "workerId", "validator-1", "activeLeaseCount"),
            Some(VariableValue::Integer(1.into())),
            "{name}: the lease must start active"
        );

        for (index, (comment, body)) in ends.iter().enumerate() {
            let after = process_changes(
                &query,
                world.deliver(&end_event("I_task", comment, body, "reporter")),
            )
            .await;
            let value = aggregate_value(&after, "workerId", "validator-1", "activeLeaseCount");
            if index == 0 {
                assert_eq!(
                    value,
                    Some(VariableValue::Integer(0.into())),
                    "{name}: the first end must release exactly one lease"
                );
            } else {
                assert_eq!(value, None, "{name}: end {index} must not change the count");
            }
        }
    }
}

#[tokio::test]
async fn released_capacity_returns_when_the_end_is_removed() {
    let query = build_query(ACTIVE_LEASES).await;
    let mut world = LeaseWorld::default();
    process_changes(&query, project_workers(WORKER_FILE)).await;
    process_changes(
        &query,
        world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease")),
    )
    .await;
    let ended = process_changes(
        &query,
        world.deliver(&end_event("I_task", "IC_result", RESULT_V2, "reporter")),
    )
    .await;
    assert_eq!(
        aggregate_value(&ended, "workerId", "validator-1", "activeLeaseCount"),
        Some(VariableValue::Integer(0.into()))
    );

    let restored = process_changes(
        &query,
        world.deliver(&comment_event_by(
            "deleted",
            RESULT_V2,
            "IC_result",
            "I_task",
            "reporter",
        )),
    )
    .await;
    assert_eq!(
        aggregate_value(&restored, "workerId", "validator-1", "activeLeaseCount"),
        Some(VariableValue::Integer(1.into())),
        "removing the only end must return the lease to active"
    );
}

#[tokio::test]
async fn a_conflicting_acquisition_withholds_capacity_from_dispatch() {
    let query = build_query(ACTIVE_LEASES).await;
    let mut world = LeaseWorld::default();
    process_changes(&query, project_workers(WORKER_FILE)).await;
    process_changes(
        &query,
        world.deliver(&lease_event(
            "I_task",
            "IC_lease_1",
            LEASE_ID,
            "validator-1/1",
            "bot",
        )),
    )
    .await;
    let conflicted = process_changes(
        &query,
        world.deliver(&lease_event(
            "I_task",
            "IC_lease_2",
            LEASE_ID,
            "validator-1/2",
            "bot",
        )),
    )
    .await;
    // Neither Lease is offered as active, so the ambiguous identity cannot be
    // dispatched against at all.
    assert_eq!(
        aggregate_value(&conflicted, "workerId", "validator-1", "activeLeaseCount"),
        Some(VariableValue::Integer(0.into()))
    );

    let resolved = process_changes(
        &query,
        world.deliver(&comment_event_by(
            "deleted",
            &lease_body(LEASE_ID, "validator-1/2"),
            "IC_lease_2",
            "I_task",
            "bot",
        )),
    )
    .await;
    assert_eq!(
        aggregate_value(&resolved, "workerId", "validator-1", "activeLeaseCount"),
        Some(VariableValue::Integer(1.into())),
        "resolving the conflict restores the survivor"
    );
}

#[tokio::test]
async fn the_deadline_query_uses_the_recomputed_is_active_flag() {
    let registry = Arc::new(FunctionRegistry::new());
    drasi_functions_cypher::register_default_cypher_functions(&registry);
    let parser = Arc::new(CypherParser::new(registry.clone()));
    let deadline = QueryBuilder::new(
        "MATCH (lease:WorkGraphTaskLease)-[:LEASE_ANCHOR]->(anchor:WorkGraphTaskLeaseAnchor) \
         WHERE drasi.trueLater(anchor.isActive, datetime(lease.expiresAt)) \
         RETURN lease.sourceCommentNodeId AS leaseCommentNodeId, anchor.leaseId AS leaseId, \
         anchor.taskNodeId AS taskNodeId",
        parser,
    )
    .with_function_registry(registry)
    .build()
    .await;

    let mut world = LeaseWorld::default();
    let acquired = process_changes(
        &deadline,
        world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease")),
    )
    .await;
    assert_eq!(additions(&acquired), 0, "the deadline has not arrived");

    // A trusted completion flips the recomputed flag, which is what cancels the
    // scheduled expiry rather than leaving it armed forever.
    let completed = world.deliver(&end_event("I_task", "IC_result", RESULT_V2, "reporter"));
    assert_eq!(
        node_property(&completed, LEASE_ANCHOR_ID, "isActive"),
        &ElementValue::Bool(false)
    );
    assert_eq!(additions(&process_changes(&deadline, completed).await), 0);
}

#[tokio::test]
async fn lease_binds_a_configured_slot_and_a_stale_slot_reference_binds_nothing() {
    let query = build_query(
        "MATCH (lease:WorkGraphTaskLease)-[:LEASES_SLOT]->(slot:WorkGraphWorkerSlot) \
         RETURN lease.leaseId AS leaseId, slot.slotId AS slotId, slot.enabled AS enabled",
    )
    .await;
    let mut world = LeaseWorld::default();
    process_changes(&query, project_workers(WORKER_FILE)).await;
    let bound = process_changes(
        &query,
        world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease")),
    )
    .await;
    assert_eq!(additions(&bound), 1);

    let unbound = process_changes(
        &query,
        world.deliver(&lease_event(
            "I_task",
            "IC_stale",
            "0198d8c4-7c28-7d43-a8dd-ffffffffffff",
            "ghost-9/4",
            "bot",
        )),
    )
    .await;
    assert_eq!(additions(&unbound), 0);
}

#[test]
fn worker_queue_comment_crud_and_edits_converge_on_stable_identities() {
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    let deleted = world.deliver(&comment_event("deleted", LEASE, "open", true, "IC_lease"));
    for removed in [
        "IC_lease",
        "COMMENT_ON:IC_lease:I_task",
        "LEASE_FOR:IC_lease:I_task",
        "LEASES_SLOT:IC_lease:workgraph-worker-slot:validator-1/1",
    ] {
        assert!(
            deleted
                .iter()
                .any(|change| is_delete(change) && id(change) == removed),
            "missing delete for {removed}"
        );
    }

    // Editing an Assignment from v1 to v2 adds the queue binding in place.
    let mut edited = comment_event("edited", ASSIGNMENT_V2, "open", true, "IC_assignment");
    edited["changes"] = json!({ "body": { "from": ASSIGNMENT } });
    let changes = convert("issue_comment", &edited);
    assert!(changes
        .iter()
        .any(|change| label(change) == "ASSIGNED_TO" && is_insert(change)));

    // Editing a Lease into an ordinary comment removes the lease and its anchor.
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    let mut edited = comment_event("edited", "just a note", "open", true, "IC_lease");
    edited["changes"] = json!({ "body": { "from": LEASE } });
    edited["sender"] = json!({"login": "bot", "node_id": "U_bot"});
    let changes = world.deliver(&edited);
    assert!(changes
        .iter()
        .any(|change| label(change) == "GitHubIssueComment" && is_insert(change)));
    assert!(world.anchor().is_none());
}

#[test]
fn a_trusted_identity_cannot_edit_across_its_configured_role() {
    // Holding *a* lifecycle role is not enough: the editor must hold the role
    // the artifact itself requires. A reporter is not authorized to acquire
    // capacity, and a dispatcher is not authorized to report an end.
    for (body, author, wrong_editor, binding) in [
        // Dispatcher-authored Lease, edited by a configured reporter.
        (LEASE, "bot", "reporter", "LEASE_ANCHOR"),
        // Reporter-authored Result, edited by a configured dispatcher.
        (RESULT_V2, "reporter", "bot", "RESULT_FOR_LEASE"),
        // Reporter-authored Expiration, edited by a configured dispatcher.
        (LEASE_EXPIRATION, "reporter", "bot", "EXPIRES_LEASE"),
    ] {
        let mut edited = comment_event_by("edited", body, "IC_target", "I_task", author);
        edited["changes"] = json!({ "body": { "from": "an ordinary note" } });
        edited["sender"] = json!({
            "login": wrong_editor,
            "node_id": format!("U_{wrong_editor}")
        });

        let mut world = LeaseWorld::default();
        let changes = world.deliver(&edited);
        assert_eq!(
            node_property(&changes, "IC_target", "trusted"),
            &ElementValue::Bool(false),
            "'{wrong_editor}' must not be trusted to edit into {binding}"
        );
        assert!(
            !changes.iter().any(|change| label(change) == binding),
            "'{wrong_editor}' produced a {binding} binding"
        );
        assert!(
            world.anchor().is_none(),
            "'{wrong_editor}' moved the lifecycle via {binding}"
        );

        // The same edit by an identity holding the required role is accepted.
        let mut correct = edited.clone();
        correct["sender"] = json!({ "login": author, "node_id": format!("U_{author}") });
        let mut world = LeaseWorld::default();
        let changes = world.deliver(&correct);
        assert_eq!(
            node_property(&changes, "IC_target", "trusted"),
            &ElementValue::Bool(true),
            "'{author}' holds the role {binding} requires"
        );
        assert!(changes.iter().any(|change| label(change) == binding));
    }
}

#[test]
fn a_bootstrap_editor_is_also_role_matched() {
    // Bootstrap reads GitHub's `editor` rather than a webhook `sender`, and
    // applies exactly the same role match.
    let mut event = comment_event("created", LEASE, "open", true, "IC_lease");
    event["comment"]["editor"] = json!({"login": "reporter", "node_id": "U_reporter"});
    let mut world = LeaseWorld::default();
    let changes = world.deliver(&event);
    assert_eq!(
        property(&changes, "WorkGraphTaskLease", "trusted"),
        &ElementValue::Bool(false),
        "a reporter must not be able to edit a comment into a Lease"
    );
    assert!(world.anchor().is_none());

    let mut event = comment_event_by("created", RESULT_V2, "IC_result", "I_task", "reporter");
    event["comment"]["editor"] = json!({"login": "bot", "node_id": "U_bot"});
    let changes = convert("issue_comment", &event);
    assert_eq!(
        property(&changes, "WorkGraphTaskResult", "trusted"),
        &ElementValue::Bool(false),
        "a dispatcher must not be able to edit a comment into a Result"
    );
    assert!(!changes
        .iter()
        .any(|change| label(change) == "RESULT_FOR_LEASE"));
}

#[test]
fn a_pin_after_an_unattributed_edit_fails_closed() {
    // A pin or an unpin names the actor performing *that* action, never the
    // last editor. A lifecycle comment that shows it was edited but supplies no
    // editor identity therefore cannot be attributed, and must not be able to
    // sneak past the editor check by arriving as a pin.
    for (body, author, binding) in [
        (LEASE, "bot", "LEASE_ANCHOR"),
        (RESULT_V2, "reporter", "RESULT_FOR_LEASE"),
        (LEASE_EXPIRATION, "reporter", "EXPIRES_LEASE"),
    ] {
        for action in ["pinned", "unpinned", "created"] {
            let mut event = comment_event_by(action, body, "IC_target", "I_task", author);
            // GitHub reports the edit through the timestamps; the pin payload
            // carries no editor at all.
            event["comment"]["updated_at"] = json!("2026-01-04T00:00:00Z");
            event["sender"] = json!({"login": author, "node_id": format!("U_{author}")});

            let mut world = LeaseWorld::default();
            let changes = world.deliver(&event);
            assert_eq!(
                node_property(&changes, "IC_target", "trusted"),
                &ElementValue::Bool(false),
                "{action} of an edited {binding} comment must not be trusted"
            );
            assert!(
                !changes.iter().any(|change| label(change) == binding),
                "{action} produced a {binding} binding for an unattributed edit"
            );
            assert!(world.anchor().is_none(), "{action}/{binding}");
        }
    }

    // The same comment with `lastEditedAt` set and no editor also fails closed.
    let mut event = comment_event("created", LEASE, "open", true, "IC_target");
    event["comment"]["last_edited_at"] = json!("2026-01-04T00:00:00Z");
    let mut world = LeaseWorld::default();
    world.deliver(&event);
    assert!(world.anchor().is_none());

    // An edited comment that *does* name a role-matched editor is trusted.
    let mut event = comment_event("created", LEASE, "open", true, "IC_target");
    event["comment"]["updated_at"] = json!("2026-01-04T00:00:00Z");
    event["comment"]["editor"] = json!({"login": "bot", "node_id": "U_bot"});
    let mut world = LeaseWorld::default();
    world.deliver(&event);
    assert!(world.anchor().is_some_and(|state| state.is_active));
}

#[test]
fn an_unattributed_edit_makes_no_statement_rather_than_retracting() {
    // Indeterminate is not the same as untrusted: a pin whose editor is unknown
    // must not tear down lifecycle state that a reconciliation already
    // established from GitHub's own view of the comment.
    let mut world = LeaseWorld::default();
    world.deliver(&comment_event("created", LEASE, "open", true, "IC_lease"));
    assert_anchor(&world, true, "none", None, "acquired");

    let mut pinned = comment_event("pinned", LEASE, "open", true, "IC_lease");
    pinned["comment"]["updated_at"] = json!("2026-01-04T00:00:00Z");
    let conversion = convert_full("issue_comment", &pinned);
    assert!(
        conversion.lifecycle.is_empty(),
        "an unattributable pin must contribute no lifecycle statement"
    );
    // It still asks for reconciliation, so the Source can learn the editor.
    assert!(conversion.lifecycle_scope.is_some());

    world.deliver(&pinned);
    assert_anchor(&world, true, "none", None, "pin left the lease intact");
}
