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

use crate::config::{GitHubWorkGraphSourceConfig, WebhookConfig};
use crate::descriptor::GitHubWorkGraphSourceDescriptor;
use crate::mapping::{
    derive_status, ConvertError, Converter, Status, NODE_LABELS, RELATION_LABELS,
};
use crate::webhook::verify_signature;
use crate::workgraph::{
    assignment_element_id, classify, encode_id_component, error_code, Classification, Outcome,
    TaskType,
};
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_lib::wal::CapacityPolicy;
use drasi_lib::DurabilityConfig;
use drasi_plugin_sdk::prelude::SourcePluginDescriptor;
use serde_json::{json, Value};

const ASSIGN: &str = r#"<details>
<summary>WorkGraph Assignment</summary>

WorkGraphAssignment/v1

Validate the synthetic fixture Issue.

```json
{
  "assignmentId": "fixture-701-validation",
  "agentProfile": "issue-validator",
  "priority": 10,
  "taskType": "issue-validation",
  "task": {
    "validationProfile": "new-issue-default"
  }
}
```
</details>
"#;

const RESULT: &str = r#"<details>
<summary>WorkGraph Result</summary>

WorkGraphResult/v1

Evaluated both requested validation criteria.

```json
{
  "assignmentId": "assignment-validation-001",
  "taskType": "issue-validation",
  "outcome": "succeeded",
  "summary": "Evaluated both requested validation criteria.",
  "result": {
    "criteria": [
      {
        "criterion": "The issue defines acceptance criteria",
        "passed": true,
        "evidence": "The body contains an acceptance checklist."
      },
      {
        "criterion": "The issue identifies an owner",
        "passed": false,
        "evidence": "The title and body do not identify an owner."
      }
    ]
  }
}
```
</details>
"#;

const RISK_ASSIGNMENT: &str = r#"<details>
<summary>WorkGraph Assignment</summary>

WorkGraphAssignment/v1

Profile delivery risk.

```json
{
  "assignmentId": "assignment-risk-001",
  "agentProfile": "issue-risk-profiler",
  "priority": 4,
  "taskType": "issue-risk-profile",
  "task": {
    "riskProfile": "delivery",
    "dimensions": [
      "Security impact",
      "Rollback complexity"
    ]
  }
}
```
</details>
"#;

const RISK_RESULT: &str = r#"<details>
<summary>WorkGraph Result</summary>

WorkGraphResult/v1

Scored both requested risk dimensions.

```json
{
  "assignmentId": "assignment-risk-001",
  "taskType": "issue-risk-profile",
  "outcome": "blocked",
  "summary": "Scored both requested risk dimensions.",
  "result": {
    "dimensions": [
      {
        "dimension": "Security impact",
        "score": 100,
        "rationale": "The change affects authorization checks."
      }
    ]
  }
}
```
</details>
"#;

fn org() -> Value {
    json!({"login":"acme","id":42,"node_id":"O_1","url":"https://api.github.com/orgs/acme"})
}
fn repo() -> Value {
    json!({"node_id":"R_7","id":7,"name":"widgets","full_name":"acme/widgets",
        "owner":{"login":"acme"},"html_url":"https://gh/acme/widgets","private":false,
        "archived":false,"fork":false,"visibility":"public","default_branch":"main",
        "topics":["graph"],"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-02-01T00:00:00Z"})
}
fn issue(labels: Value) -> Value {
    json!({"node_id":"I_42","id":4242,"number":42,"title":"Broken","body":"It breaks.",
        "state":"open","state_reason":null,"locked":false,"created_at":"2024-03-01T00:00:00Z",
        "updated_at":"2024-03-02T00:00:00Z","closed_at":null,"labels":labels,
        "assignees":[{"login":"grace"}],"author_association":"MEMBER",
        "html_url":"https://gh/acme/widgets/issues/42",
        "user":{"login":"ada","node_id":"U_ada","id":1,"type":"User"}})
}
fn item_event(action: &str, labels: Value) -> Value {
    json!({"action":action,"organization":org(),"repository":repo(),"issue":issue(labels)})
}
fn comment_event(action: &str, body: &str, pr: bool) -> Value {
    let mut parent = issue(json!([]));
    if pr {
        parent["pull_request"] = json!({"url":"https://api.github.com/pulls/42"});
    }
    json!({"action":action,"organization":org(),"repository":repo(),"issue":parent,
        "comment":{"node_id":"IC_9","id":9001,"body":body,
        "created_at":"2024-03-03T00:00:00Z","updated_at":"2024-03-03T00:00:00Z",
        "author_association":"CONTRIBUTOR","html_url":"https://gh/comments/9001",
        "user":{"login":"bot","node_id":"U_bot","id":2,"type":"Bot"}}})
}
fn review(action: &str) -> Value {
    json!({"action":action,"organization":org(),"repository":repo(),
        "pull_request":{"node_id":"PR_5"},"review":{"node_id":"PRR_3","id":55,
        "state":"approved","body":"LGTM","submitted_at":"2024-03-05T00:00:00Z",
        "commit_id":"abc","html_url":"https://gh/reviews/3",
        "user":{"login":"ada","node_id":"U_ada","id":1,"type":"User"}}})
}
fn convert(event: &str, payload: &Value) -> Vec<SourceChange> {
    Converter::new("gh", "acme", 1)
        .convert(event, payload)
        .unwrap()
        .unwrap()
}
fn element(change: &SourceChange) -> &Element {
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => element,
        _ => panic!("expected element"),
    }
}
fn label(change: &SourceChange) -> String {
    let metadata = match change {
        SourceChange::Delete { metadata } => metadata,
        _ => match element(change) {
            Element::Node { metadata, .. } | Element::Relation { metadata, .. } => metadata,
        },
    };
    metadata.labels.join(",")
}
fn render(change: &SourceChange) -> String {
    let op = match change {
        SourceChange::Insert { .. } => "I",
        SourceChange::Update { .. } => "U",
        SourceChange::Delete { .. } => "D",
        SourceChange::Future { .. } => "F",
    };
    let id = &change.get_reference().element_id;
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => match element {
            Element::Relation {
                in_node, out_node, ..
            } => format!(
                "{op}:{}:{id}:{}>{}",
                label(change),
                in_node.element_id,
                out_node.element_id
            ),
            _ => format!("{op}:{}:{id}", label(change)),
        },
        _ => format!("{op}:{}:{id}", label(change)),
    }
}
fn changes(event: &str, payload: &Value) -> String {
    convert(event, payload)
        .iter()
        .map(render)
        .collect::<Vec<_>>()
        .join("|")
}
fn prop(change: &SourceChange, key: &str) -> Option<ElementValue> {
    match element(change) {
        Element::Node { properties, .. } | Element::Relation { properties, .. } => {
            properties.get(key).cloned()
        }
    }
}
fn text(change: &SourceChange, key: &str) -> Option<String> {
    match prop(change, key) {
        Some(ElementValue::String(value)) => Some(value.to_string()),
        _ => None,
    }
}
fn edited(from: &str, to: &str) -> Value {
    let mut payload = comment_event("edited", to, false);
    payload["changes"] = json!({"body":{"from":from}});
    payload
}

#[test]
fn families_actions_ids_properties_and_directions() {
    assert_eq!(NODE_LABELS.len(), 10);
    assert_eq!(RELATION_LABELS.len(), 6);
    let created = json!({"action":"created","organization":org(),"repository":repo()});
    assert_eq!(
        changes("repository", &created),
        "U:GitHubOrganization:O_1|I:GitHubRepository:R_7|I:IN_ORGANIZATION:IN_ORGANIZATION:R_7:O_1:R_7>O_1"
    );
    assert_eq!(
        text(&convert("repository", &created)[1], "createdAt").unwrap(),
        "2024-01-01T00:00:00Z"
    );
    let mut epoch = created.clone();
    epoch["repository"]["created_at"] = json!(1_704_067_200_i64);
    assert_eq!(
        text(&convert("repository", &epoch)[1], "createdAt").unwrap(),
        "2024-01-01T00:00:00Z"
    );

    let opened = item_event("opened", json!([{"name":"bug"},{"name":"status:ready"}]));
    let mapped = convert("issues", &opened);
    assert_eq!(
        &changes("issues", &opened)[..66],
        "I:GitHubIssue:I_42|I:IN_REPOSITORY:IN_REPOSITORY:I_42:R_7:I_42>R_7"
    );
    assert_eq!(text(&mapped[0], "status").as_deref(), Some("ready"));
    assert_eq!(text(&mapped[0], "authorId").as_deref(), Some("U_ada"));
    assert!(text(&mapped[0], "bodyDigest")
        .unwrap()
        .starts_with("sha256:"));
    let deleted = item_event("deleted", json!([]));
    assert!(changes("issues", &deleted).ends_with("D:GitHubIssue:I_42"));

    let mut transfer = item_event("transferred", json!([]));
    transfer["changes"] = json!({"new_issue":{"node_id":"I_99","number":99,"labels":[]},
        "new_repository":{"node_id":"R_8","owner":{"login":"acme"}}});
    let moved = changes("issues", &transfer);
    assert!(moved.contains("D:GitHubIssue:I_42") && moved.contains("I:GitHubIssue:I_99"));
    transfer["changes"]["new_repository"]["owner"]["login"] = json!("other");
    assert!(!changes("issues", &transfer).contains("I:GitHubIssue:I_99"));

    let mut pr = issue(json!([]));
    pr["node_id"] = json!("PR_5");
    pr["draft"] = json!(true);
    pr["head"] = json!({"ref":"feature","sha":"abc"});
    let pr = json!({"action":"opened","organization":org(),"repository":repo(),"pull_request":pr});
    let mapped = convert("pull_request", &pr);
    assert_eq!(prop(&mapped[0], "isDraft"), Some(ElementValue::Bool(true)));
    assert_eq!(text(&mapped[0], "headRefName").as_deref(), Some("feature"));
    assert_eq!(
        changes("pull_request_review", &review("submitted")),
        "I:GitHubPullRequestReview:PRR_3|I:REVIEW_OF:REVIEW_OF:PRR_3:PR_5:PRR_3>PR_5"
    );
    assert_eq!(
        text(
            &convert("pull_request_review", &review("dismissed"))[0],
            "state"
        )
        .unwrap(),
        "dismissed"
    );

    let converter = Converter::new("gh", "acme", 1);
    for event in ["push", "projects_v2", "pull_request_review_comment"] {
        assert!(converter.convert(event, &opened).unwrap().is_none());
    }
    let future = item_event("future_action", json!([]));
    assert!(converter.convert("issues", &future).unwrap().is_none());
    let mut other = opened;
    other["organization"]["login"] = json!("other");
    assert!(matches!(
        converter.convert("issues", &other),
        Err(ConvertError::OrganizationMismatch(_))
    ));
}

#[test]
fn status_is_exact_and_conflicts_are_snapshot_free() {
    for (value, expected) in [
        (json!({}), Status::Unknown),
        (json!({"labels":[]}), Status::Zero),
        (json!({"labels":[{"name":"Status:x"}]}), Status::Zero),
        (
            json!({"labels":[{"name":"status:x"}]}),
            Status::One("status:x".into()),
        ),
        (
            json!({"labels":[{"name":"status:z"},{"name":"status:a"}]}),
            Status::Conflict(vec!["status:a".into(), "status:z".into()]),
        ),
    ] {
        assert_eq!(derive_status(&value), expected);
    }
    let conflict = convert(
        "issues",
        &item_event(
            "labeled",
            json!([
        {"name":"status:z"},{"name":"status:a"}]),
        ),
    );
    assert_eq!(prop(&conflict[0], "status"), Some(ElementValue::Null));
    assert_eq!(label(&conflict[1]), "WorkGraphError");
    assert_eq!(
        text(&conflict[1], "errorCode").as_deref(),
        Some("multiple-status-labels")
    );
    assert_eq!(
        render(&conflict[2]),
        "U:ERROR_ON:ERROR_ON:workgraph-error:status:I_42:I_42:workgraph-error:status:I_42>I_42"
    );
    let opened = item_event("opened", json!([{"name":"status:a"},{"name":"status:b"}]));
    assert!(render(&convert("issues", &opened)[3]).starts_with("I:ERROR_ON"));
    let mut unknown = item_event("edited", json!([]));
    unknown["issue"].as_object_mut().unwrap().remove("labels");
    let mapped = convert("issues", &unknown);
    assert_eq!(mapped.len(), 1);
    assert_eq!(prop(&mapped[0], "status"), None);
}

#[test]
fn comments_create_edit_and_delete_every_class() {
    assert_eq!(
        changes("issue_comment", &comment_event("created", "hello", false)),
        "I:GitHubIssueComment:IC_9|I:COMMENT_ON:COMMENT_ON:IC_9:I_42:IC_9>I_42"
    );
    assert_eq!(
        label(&convert("issue_comment", &comment_event("created", "hi", true))[0]),
        "GitHubPullRequestComment"
    );
    let assignment = convert("issue_comment", &comment_event("created", ASSIGN, false));
    assert_eq!(
        assignment[0].get_reference().element_id.as_ref(),
        "workgraph-assignment:O_1:fixture-701-validation"
    );
    assert_eq!(
        prop(&assignment[0], "priority"),
        Some(ElementValue::Integer(10))
    );
    assert_eq!(
        prop(&assignment[0], "task"),
        Some(ElementValue::from(&json!({
            "validationProfile": "new-issue-default"
        })))
    );
    assert_eq!(text(&assignment[0], "authorLogin").as_deref(), Some("bot"));
    assert_eq!(
        text(&assignment[0], "sourceCommentNodeId").as_deref(),
        Some("IC_9")
    );
    assert_eq!(label(&assignment[1]), "COMMENT_ON");
    let result = convert("issue_comment", &comment_event("created", RESULT, false));
    assert_eq!(result.len(), 3);
    assert_eq!(label(&result[1]), "COMMENT_ON");
    assert_eq!(label(&result[2]), "RESULT_FOR");
    assert!(matches!(
        prop(&result[0], "result"),
        Some(ElementValue::Object(_))
    ));
    let invalid = convert(
        "issue_comment",
        &comment_event("created", "WorkGraphAssignment/v1\n\nbad", false),
    );
    assert_eq!(label(&invalid[0]), "WorkGraphError");
    assert_eq!(label(&invalid[1]), "ERROR_ON");
    assert_eq!(
        text(&invalid[0], "sourceCommentBody").as_deref(),
        Some("WorkGraphAssignment/v1\n\nbad")
    );
    let malformed_result = RESULT.replacen("<details>", "<details open>", 1);
    let invalid_result = convert(
        "issue_comment",
        &comment_event("created", &malformed_result, false),
    );
    assert_eq!(label(&invalid_result[0]), "WorkGraphError");
    assert_eq!(label(&invalid_result[1]), "ERROR_ON");
    assert_eq!(
        text(&invalid_result[0], "sourceCommentBody").as_deref(),
        Some(malformed_result.as_str())
    );

    let ordinary_to_assignment = changes("issue_comment", &edited("plain", ASSIGN));
    assert!(
        ordinary_to_assignment.starts_with("D:COMMENT_ON")
            && ordinary_to_assignment
                .ends_with("workgraph-assignment:O_1:fixture-701-validation>I_42")
    );
    let renamed = ASSIGN.replace("fixture-701-validation", "fixture-702-validation");
    let rename = changes("issue_comment", &edited(ASSIGN, &renamed));
    assert!(
        rename.contains("D:WorkGraphAssignment:workgraph-assignment:O_1:fixture-701-validation")
            && rename
                .contains("I:WorkGraphAssignment:workgraph-assignment:O_1:fixture-702-validation")
    );
    let retarget = changes(
        "issue_comment",
        &edited(
            RESULT,
            &RESULT.replace("assignment-validation-001", "assignment-validation-002"),
        ),
    );
    assert!(retarget.contains("D:RESULT_FOR") && retarget.contains("I:RESULT_FOR"));
    let error_to_plain = changes(
        "issue_comment",
        &edited("WorkGraphAssignment/v1\n\nbad", "plain"),
    );
    assert!(error_to_plain.starts_with("D:ERROR_ON") && error_to_plain.ends_with("IC_9>I_42"));
    let deleted = changes("issue_comment", &comment_event("deleted", RESULT, false));
    assert!(deleted.starts_with("D:RESULT_FOR") && deleted.ends_with("D:WorkGraphResult:IC_9"));
}

fn invalid_code(body: &str) -> &'static str {
    match classify(body) {
        Classification::Invalid(error) => error.code,
        other => panic!("expected invalid, got {other:?}"),
    }
}

fn envelope(marker: &str, value: Value) -> String {
    let label = match marker {
        "WorkGraphAssignment/v1" => "WorkGraph Assignment",
        "WorkGraphResult/v1" => "WorkGraph Result",
        other => panic!("unsupported test marker {other}"),
    };
    let json = serde_json::to_string_pretty(&value).unwrap();
    format!(
        "<details>\n<summary>{label}</summary>\n\n{marker}\n\nsummary\n\n```json\n{json}\n```\n</details>\n"
    )
}

#[test]
fn canonical_envelopes_and_payloads_are_strictly_typed() {
    for body in [
        "plain",
        " WorkGraphResult/v1",
        "Mention WorkGraphAssignment/v1 inline.",
        "<details>\n<summary>Release notes</summary>\n\nOrdinary prose.\n</details>\n",
        "<details>\n<summary>Discussion</summary>\n\nI tried WorkGraphAssignment/v1 inline.\n</details>\n",
    ] {
        assert_eq!(classify(body), Classification::Ordinary);
    }

    let Classification::Assignment(assignment) = classify(ASSIGN) else {
        panic!("canonical dogfood Assignment must parse");
    };
    assert_eq!(assignment.assignment_id, "fixture-701-validation");
    assert_eq!(assignment.task_type, TaskType::IssueValidation);

    let Classification::Result(result) = classify(RESULT) else {
        panic!("canonical demo Result must parse");
    };
    assert_eq!(result.assignment_id, "assignment-validation-001");
    assert_eq!(result.outcome, Outcome::Succeeded);
    assert_eq!(result.task_type, TaskType::IssueValidation);

    let Classification::Assignment(risk_assignment) = classify(RISK_ASSIGNMENT) else {
        panic!("canonical risk Assignment must parse");
    };
    assert_eq!(risk_assignment.task_type, TaskType::IssueRiskProfile);

    let Classification::Result(risk_result) = classify(RISK_RESULT) else {
        panic!("canonical risk Result must parse");
    };
    assert_eq!(risk_result.outcome, Outcome::Blocked);
    assert_eq!(risk_result.task_type, TaskType::IssueRiskProfile);

    assert!(matches!(
        classify(&ASSIGN.replacen(
            "Validate the synthetic fixture Issue.",
            "WorkGraphResult/v1",
            1,
        )),
        Classification::Assignment(_)
    ));
    assert!(matches!(
        classify(&RESULT.replace(
            "Evaluated both requested validation criteria.",
            "WorkGraphAssignment/v1"
        )),
        Classification::Result(_)
    ));

    assert_eq!(
        assignment_element_id("O_1", "a:b /ü"),
        "workgraph-assignment:O_1:a%3Ab%20%2F%C3%BC"
    );
    assert_eq!(encode_id_component("a:b"), "a%3Ab");
}

fn assert_malformed_variants(
    body: &str,
    label: &str,
    other_label: &str,
    marker: &str,
    other_marker: &str,
    human_summary: &str,
) {
    let prefix = format!("<details>\n<summary>{label}</summary>\n\n");
    let json_start = body.find("```json\n").unwrap() + "```json\n".len();
    let json_end = json_start + body[json_start..].find("\n```\n").unwrap();
    let json_text = &body[json_start..json_end];
    let compact_json =
        serde_json::to_string(&serde_json::from_str::<Value>(json_text).unwrap()).unwrap();
    let unwrapped = body
        .strip_prefix(&prefix)
        .unwrap()
        .strip_suffix("</details>\n")
        .unwrap()
        .to_string();

    let variants = [
        ("unwrapped legacy envelope", unwrapped),
        (
            "open attribute",
            body.replacen("<details>", "<details open>", 1),
        ),
        (
            "arbitrary attribute",
            body.replacen("<details>", "<details class=\"workgraph\">", 1),
        ),
        (
            "wrong summary label",
            body.replacen(
                &format!("<summary>{label}</summary>"),
                &format!("<summary>{other_label}</summary>"),
                1,
            ),
        ),
        (
            "missing summary label",
            body.replacen(&format!("<summary>{label}</summary>\n"), "", 1),
        ),
        ("wrong marker", body.replacen(marker, other_marker, 1)),
        ("missing marker", body.replacen(marker, "", 1)),
        ("CRLF bytes", body.replace('\n', "\r\n")),
        ("literal newline escapes", body.replace('\n', "\\n")),
        (
            "extra LF after details",
            body.replacen("<details>\n", "<details>\n\n", 1),
        ),
        (
            "several extra LFs after details",
            body.replacen("<details>\n", "<details>\n\n\n\n", 1),
        ),
        (
            "missing blank after summary label",
            body.replacen("</summary>\n\n", "</summary>\n", 1),
        ),
        (
            "extra blank after summary label",
            body.replacen("</summary>\n\n", "</summary>\n\n\n", 1),
        ),
        (
            "missing blank after marker",
            body.replacen(&format!("{marker}\n\n"), &format!("{marker}\n"), 1),
        ),
        (
            "extra blank after marker",
            body.replacen(&format!("{marker}\n\n"), &format!("{marker}\n\n\n"), 1),
        ),
        (
            "missing blank after human summary",
            body.replacen(
                &format!("{human_summary}\n\n```json"),
                &format!("{human_summary}\n```json"),
                1,
            ),
        ),
        (
            "extra blank after human summary",
            body.replacen(
                &format!("{human_summary}\n\n```json"),
                &format!("{human_summary}\n\n\n```json"),
                1,
            ),
        ),
        (
            "multiline human summary",
            body.replacen(human_summary, &format!("{human_summary}\ncontinued"), 1),
        ),
        ("empty human summary", body.replacen(human_summary, "", 1)),
        ("compact JSON", body.replacen(json_text, &compact_json, 1)),
        (
            "mismatched opening fence",
            body.replacen("```json\n", "```yaml\n", 1),
        ),
        (
            "mismatched closing fence",
            body.replacen("\n```\n</details>", "\n~~~\n</details>", 1),
        ),
        (
            "extra fence",
            body.replacen(
                "\n```\n</details>",
                "\n```\n```yaml\n{}\n```\n</details>",
                1,
            ),
        ),
        ("prose before wrapper", format!("Unexpected prose.\n{body}")),
        ("prose after wrapper", format!("{body}Unexpected prose.\n")),
        (
            "unclosed wrapper",
            body.strip_suffix("</details>\n").unwrap().to_string(),
        ),
        (
            "missing final LF",
            body.strip_suffix('\n').unwrap().to_string(),
        ),
        ("extra final LF", format!("{body}\n")),
    ];

    for (name, malformed) in variants {
        assert!(
            matches!(classify(&malformed), Classification::Invalid(_)),
            "{name} must be a WorkGraphError for {marker}"
        );
    }
}

#[test]
fn assignment_and_result_envelopes_reject_every_noncanonical_boundary() {
    assert_malformed_variants(
        ASSIGN,
        "WorkGraph Assignment",
        "WorkGraph Result",
        "WorkGraphAssignment/v1",
        "WorkGraphResult/v1",
        "Validate the synthetic fixture Issue.",
    );
    assert_malformed_variants(
        RESULT,
        "WorkGraph Result",
        "WorkGraph Assignment",
        "WorkGraphResult/v1",
        "WorkGraphAssignment/v1",
        "Evaluated both requested validation criteria.",
    );
}

#[test]
fn envelope_errors_and_typed_schema_errors_remain_specific() {
    let unsupported_assignment = ASSIGN.replace("WorkGraphAssignment/v1", "WorkGraphAssignment/v2");
    let unsupported_result = RESULT.replace("WorkGraphResult/v1", "WorkGraphResult/v2");
    for body in [
        "WorkGraphAssignment/",
        "WorkGraphAssignment/v1 ",
        &unsupported_assignment,
        &unsupported_result,
    ] {
        assert_eq!(invalid_code(body), error_code::UNSUPPORTED_VERSION);
    }

    for body in [ASSIGN, RESULT] {
        let json_start = body.find("```json\n").unwrap() + "```json\n".len();
        let json_end = json_start + body[json_start..].find("\n```\n").unwrap();
        let json_text = &body[json_start..json_end];
        let compact =
            serde_json::to_string(&serde_json::from_str::<Value>(json_text).unwrap()).unwrap();
        assert_eq!(
            invalid_code(&body.replacen(json_text, &compact, 1)),
            error_code::NON_CANONICAL_JSON
        );
        assert_eq!(
            invalid_code(&body.replacen(json_text, "not-json", 1)),
            error_code::INVALID_JSON
        );
        assert_eq!(
            invalid_code(&body.replacen(json_text, "[\n  1,\n  2\n]", 1)),
            error_code::JSON_NOT_OBJECT
        );
    }

    let repeated_result_marker = RESULT.replacen(
        "Evaluated both requested validation criteria.",
        "Do not repeat WorkGraphResult/v1 in the summary.",
        1,
    );
    assert_eq!(
        invalid_code(&repeated_result_marker),
        error_code::INVALID_ENVELOPE
    );
    let mismatched_result_summary = RESULT.replacen(
        "Evaluated both requested validation criteria.",
        "Changed only the human summary.",
        1,
    );
    assert_eq!(
        invalid_code(&mismatched_result_summary),
        error_code::INVALID_RESULT_PAYLOAD
    );

    for patch in [
        json!({"assignmentId":"","agentProfile":"p","priority":0,"taskType":"issue-validation","task":{"validationProfile":"v"}}),
        json!({"assignmentId":"a","agentProfile":"p","priority":-1,"taskType":"issue-validation","task":{"validationProfile":"v"}}),
        json!({"assignmentId":"a","agentProfile":"p","priority":0,"taskType":"issue-validation","task":{"validationProfile":""}}),
        json!({"assignmentId":"a","agentProfile":"p","priority":0,"taskType":"issue-validation","task":{}}),
        json!({"assignmentId":"a","agentProfile":"p","priority":0,"taskType":"issue-validation","task":{"validationProfile":"v"},"assignedBy":"x"}),
        json!({"assignmentId":"a","agentProfile":"p","priority":0,"taskType":"unknown","task":{"validationProfile":"v"}}),
    ] {
        assert_eq!(
            invalid_code(&envelope("WorkGraphAssignment/v1", patch)),
            error_code::INVALID_ASSIGNMENT_PAYLOAD
        );
    }
    for bad in [
        json!({"assignmentId":"a","taskType":"issue-risk-profile","outcome":"partial","summary":"s","result":{"dimensions":[]}}),
        json!({"assignmentId":"a","taskType":"issue-risk-profile","outcome":"failed","summary":"s","result":{"dimensions":[{"dimension":"d","score":101,"rationale":"r"}]}}),
        json!({"assignmentId":"a","taskType":"issue-validation","outcome":"failed","summary":"","result":{"criteria":[]}}),
        json!({"assignmentId":"a","taskType":"issue-validation","outcome":"failed","summary":"Do not repeat WorkGraphResult/v1 in the summary.","result":{"criteria":[{"criterion":"c","passed":true,"evidence":"e"}]}}),
        json!({"assignmentId":"a","taskType":"issue-validation","outcome":"failed","summary":"s","result":{"criteria":[{"criterion":"c","passed":true,"evidence":"e"}]},"resultId":"r"}),
    ] {
        assert_eq!(
            invalid_code(&envelope("WorkGraphResult/v1", bad)),
            error_code::INVALID_RESULT_PAYLOAD
        );
    }

    assert_eq!(
        invalid_code(&envelope(
            "WorkGraphAssignment/v1",
            json!({"assignmentId":"a","agentProfile":"p","priority":0,
                "taskType":"issue-validation","task":{"validationProfile":null}})
        )),
        error_code::INVALID_ASSIGNMENT_PAYLOAD
    );
}

#[test]
fn issue_validation_assignment_rejects_stale_criteria() {
    let stale_criteria = envelope(
        "WorkGraphAssignment/v1",
        json!({"assignmentId":"a","agentProfile":"p","priority":0,
            "taskType":"issue-validation",
            "task":{"validationProfile":"v","criteria":["c"]}}),
    );
    let Classification::Invalid(error) = classify(&stale_criteria) else {
        panic!("legacy task.criteria must be rejected");
    };

    assert_eq!(error.code, error_code::INVALID_ASSIGNMENT_PAYLOAD);
    assert!(error.message.contains("unknown field `criteria`"));
}

fn config() -> GitHubWorkGraphSourceConfig {
    GitHubWorkGraphSourceConfig {
        organization: "acme".into(),
        webhook: WebhookConfig {
            secret: "secret".into(),
            ..WebhookConfig::default()
        },
        durability: DurabilityConfig {
            enabled: true,
            max_events: 10_000,
            capacity_policy: CapacityPolicy::RejectIncoming,
        },
    }
}

#[test]
fn config_and_signature_contracts() {
    assert!(config().validate().is_ok());
    let custom: GitHubWorkGraphSourceConfig = serde_json::from_value(json!({
        "organization":"acme","webhook":{"secret":"s"},
        "durability":{"enabled":true,"maxEvents":123,"capacityPolicy":"RejectIncoming"}
    }))
    .unwrap();
    assert_eq!(custom.durability.max_events, 123);
    for bad in [
        json!({"organization":"","webhook":{"secret":"s"}}),
        json!({"organization":"acme/x","webhook":{"secret":"s"}}),
        json!({"organization":"acme","webhook":{"secret":"s","path":"relative"}}),
        json!({"organization":"acme","webhook":{"secret":"s","path":"/:"}}),
        json!({"organization":"acme","webhook":{"secret":"s"},"token":"x"}),
        json!({"organization":"acme","webhook":{"secret":"s"},"durability":{"enabled":true,"max_events":123}}),
    ] {
        assert!(serde_json::from_value::<GitHubWorkGraphSourceConfig>(bad)
            .and_then(|value| value.validate().map_err(serde::de::Error::custom))
            .is_err());
    }
    let source = crate::GitHubWorkGraphSourceBuilder::new("gh")
        .with_config(config())
        .build()
        .unwrap();
    assert_eq!(
        drasi_lib::Source::properties(&source)["webhook"]["secret"],
        json!("[REDACTED]")
    );
    let schema = GitHubWorkGraphSourceDescriptor.config_schema_json();
    assert!(schema.contains("\"maxEvents\"") && !schema.contains("\"max_events\""));
    let signature = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
    assert!(verify_signature(b"It's a Secret to Everybody", b"Hello, World!", signature).is_ok());
    assert!(verify_signature(b"wrong", b"Hello, World!", signature).is_err());
}
