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
const ASSIGN: &str = "<details>\n<summary>WorkGraph Assignment</summary>\n\nWorkGraphAssignment/v1\n\nAutomatically validate this newly opened Issue.\n\n```json\n{\n  \"assignmentId\": \"{{after.assignmentId}}\",\n  \"agentProfile\": \"issue-validator\",\n  \"priority\": 10,\n  \"taskType\": \"issue-validation\",\n  \"task\": {\n    \"validationProfile\": \"new-issue-default\",\n    \"criteria\": [\n      \"The Issue has a non-empty title\",\n      \"The Issue body is present\"\n    ]\n  }\n}\n```\n</details>\n";
const RESULT: &str = "<details>\n<summary>WorkGraph Result</summary>\n\nWorkGraphResult/v1\n\nEvaluated both requested validation criteria.\n\n```json\n{\n  \"assignmentId\": \"assignment-validation-001\",\n  \"taskType\": \"issue-validation\",\n  \"outcome\": \"succeeded\",\n  \"summary\": \"Evaluated both requested validation criteria.\",\n  \"result\": {\n    \"criteria\": [\n      {\n        \"criterion\": \"The issue defines acceptance criteria\",\n        \"passed\": true,\n        \"evidence\": \"The body contains an acceptance checklist.\"\n      },\n      {\n        \"criterion\": \"The issue identifies an owner\",\n        \"passed\": false,\n        \"evidence\": \"The title and body do not identify an owner.\"\n      }\n    ]\n  }\n}\n```\n</details>";
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
    let mut marked_review = review("submitted");
    marked_review["review"]["body"] = json!(ASSIGN);
    assert_eq!(
        label(&convert("pull_request_review", &marked_review)[0]),
        "GitHubPullRequestReview"
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
        "workgraph-assignment:O_1:%7B%7Bafter.assignmentId%7D%7D"
    );
    assert_eq!(
        prop(&assignment[0], "priority"),
        Some(ElementValue::Integer(10))
    );
    assert!(matches!(
        prop(&assignment[0], "task"),
        Some(ElementValue::Object(_))
    ));
    let result = convert("issue_comment", &comment_event("created", RESULT, false));
    assert_eq!(result.len(), 3);
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
    assert_eq!(
        text(&invalid[0], "sourceCommentBody").as_deref(),
        Some("WorkGraphAssignment/v1\n\nbad")
    );

    let ordinary_to_assignment = changes("issue_comment", &edited("plain", ASSIGN));
    assert!(
        ordinary_to_assignment.starts_with("D:COMMENT_ON")
            && ordinary_to_assignment
                .ends_with("workgraph-assignment:O_1:%7B%7Bafter.assignmentId%7D%7D>I_42")
    );
    let renamed = ASSIGN.replace("{{after.assignmentId}}", "a43");
    let rename = changes("issue_comment", &edited(ASSIGN, &renamed));
    assert!(
        rename.contains(
            "D:WorkGraphAssignment:workgraph-assignment:O_1:%7B%7Bafter.assignmentId%7D%7D"
        ) && rename.contains("I:WorkGraphAssignment:workgraph-assignment:O_1:a43")
    );
    let retarget = changes(
        "issue_comment",
        &edited(RESULT, &RESULT.replace("assignment-validation-001", "a43")),
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
    let payload = serde_json::to_string_pretty(&value).unwrap();
    canonical(marker, &payload)
}
fn canonical(marker: &str, payload: &str) -> String {
    let (summary, newline) = if marker.starts_with("WorkGraphAssignment/") {
        ("WorkGraph Assignment", "\n")
    } else {
        ("WorkGraph Result", "")
    };
    format!(
        "<details>\n<summary>{summary}</summary>\n\n{marker}\n\nsummary\n\n```json\n{payload}\n```\n</details>{newline}"
    )
}

#[test]
fn envelopes_and_payloads_are_strictly_typed() {
    for body in [
        "plain",
        "Mention WorkGraphAssignment/v1 inline.",
        "<details>\n<summary>Notes</summary>\n\nordinary\n</details>",
        "<details>\n<summary>Discussion</summary>\n\nI tried WorkGraphAssignment/v1 inline.\n</details>",
    ] {
        assert_eq!(classify(body), Classification::Ordinary);
    }
    for (valid, assignment) in [
        (ASSIGN.to_owned(), true),
        (ASSIGN.trim_end_matches('\n').to_owned(), true),
        (ASSIGN.replace('\n', "\r\n"), true),
        (RESULT.to_owned(), false),
        (format!("{RESULT}\n"), false),
    ] {
        let actual = classify(&valid);
        assert!(
            matches!(
                (&actual, assignment),
                (&Classification::Assignment(_), true) | (&Classification::Result(_), false)
            ),
            "{actual:?}: {valid:?}"
        );
    }
    let compact = canonical(
        "WorkGraphAssignment/v1",
        r#"{"assignmentId":"a42","agentProfile":"validator","priority":10,"taskType":"issue-validation","task":{"validationProfile":"default","criteria":["Reproduces"]}}"#,
    );
    for invalid in [
        "WorkGraphAssignment/v1\n\nsummary".to_owned(),
        ASSIGN.replacen("<details>", "<details open>", 1),
        ASSIGN.replace("WorkGraph Assignment", "WorkGraph Result"),
        ASSIGN.replace("WorkGraphAssignment/v1", "WorkGraphResult/v1"),
        ASSIGN.replace("</details>", ""),
        ASSIGN.replace("</details>", "</detail>"),
        ASSIGN.replacen("</summary>\n\n", "</summary>\n", 1),
        ASSIGN.replacen("/v1\n\n", "/v1\n", 1),
        ASSIGN.replacen(
            "Automatically validate this newly opened Issue.\n\n",
            "Automatically validate this newly opened Issue.\n",
            1,
        ),
        compact,
        canonical("WorkGraphAssignment/v1", "[]"),
        format!("{ASSIGN}\n"),
        format!("{ASSIGN}prose"),
        ASSIGN.replace("</details>", "```yaml\nx\n```\n</details>"),
        ASSIGN.replace('\n', "\\n"),
    ] {
        assert!(
            matches!(classify(&invalid), Classification::Invalid(_)),
            "{invalid}"
        );
    }
    assert_eq!(
        invalid_code(&ASSIGN.replace("/v1", "/v2")),
        error_code::UNSUPPORTED_VERSION
    );

    for patch in [
        json!({"assignmentId":"","agentProfile":"p","priority":0,"taskType":"issue-validation","task":{"validationProfile":"v","criteria":["c"]}}),
        json!({"assignmentId":"a","agentProfile":"p","priority":-1,"taskType":"issue-validation","task":{"validationProfile":"v","criteria":["c"]}}),
        json!({"assignmentId":"a","agentProfile":"p","priority":0,"taskType":"issue-validation","task":{"validationProfile":"v","criteria":[]},"assignedBy":"x"}),
    ] {
        assert_eq!(
            invalid_code(&envelope("WorkGraphAssignment/v1", patch)),
            error_code::INVALID_ASSIGNMENT_PAYLOAD
        );
    }
    let risk = canonical(
        "WorkGraphAssignment/v1",
        "{\n  \"assignmentId\": \"a\",\n  \"agentProfile\": \"p\",\n  \"priority\": 1,\n  \"taskType\": \"issue-risk-profile\",\n  \"task\": {\n    \"riskProfile\": \"r\",\n    \"dimensions\": [\n      \"security\"\n    ]\n  }\n}",
    );
    assert!(matches!(classify(&risk), Classification::Assignment(_)));
    let result = canonical(
        "WorkGraphResult/v1",
        "{\n  \"assignmentId\": \"a\",\n  \"taskType\": \"issue-risk-profile\",\n  \"outcome\": \"blocked\",\n  \"summary\": \"s\",\n  \"result\": {\n    \"dimensions\": [\n      {\n        \"dimension\": \"security\",\n        \"score\": 100,\n        \"rationale\": \"r\"\n      }\n    ]\n  }\n}",
    );
    let Classification::Result(parsed) = classify(&result) else {
        panic!("valid result");
    };
    assert_eq!(parsed.outcome, Outcome::Blocked);
    assert_eq!(parsed.task_type, TaskType::IssueRiskProfile);
    for bad in [
        json!({"assignmentId":"a","taskType":"issue-risk-profile","outcome":"partial","summary":"s","result":{"dimensions":[]}}),
        json!({"assignmentId":"a","taskType":"issue-risk-profile","outcome":"failed","summary":"s","result":{"dimensions":[{"dimension":"d","score":101,"rationale":"r"}]}}),
        json!({"assignmentId":"a","taskType":"issue-validation","outcome":"failed","summary":"","result":{"criteria":[]}}),
    ] {
        assert_eq!(
            invalid_code(&envelope("WorkGraphResult/v1", bad)),
            error_code::INVALID_RESULT_PAYLOAD
        );
    }
    assert_eq!(
        assignment_element_id("O_1", "a:b /ü"),
        "workgraph-assignment:O_1:a%3Ab%20%2F%C3%BC"
    );
    assert_eq!(encode_id_component("a:b"), "a%3Ab");
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
