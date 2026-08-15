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

//! Source-wide extraction and promotion of `WorkGraphEvent/v1` comments.
//!
//! The pipeline recognises exactly the strict comment grammar
//!
//! ```text
//! WorkGraphEvent/v1<LF><LF><summary><LF><LF><json>
//! ```
//!
//! and promotes the common envelope onto graph elements. Anything else — an
//! ordinary comment, a legacy pure-JSON comment, a legacy fenced comment, a
//! multi-line or over-long summary, or a body with trailing text — passes
//! through untouched and never becomes an event. There is no fallback parser
//! and no migration path for the retired formats.
//!
//! Extraction is a coarse pre-filter for *querying*: it is slightly more
//! permissive than `drasi_workgraph_common::comment::parse_comment` about the
//! summary line's contents. Nothing is authorized on the strength of promotion —
//! every component re-parses the comment with the shared parser, and verifies
//! the author's immutable identity, before acting on an event.

use std::{collections::HashSet, sync::Arc};

use drasi_core::{
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::ElementIndex,
    middleware::{MiddlewareContainer, MiddlewareTypeRegistry, SourceMiddlewarePipeline},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
        SourceMiddlewareConfig,
    },
};
use serde_json::{json, Value};

use crate::{jq::JQFactory, parse_json::ParseJsonFactory, regex_extract::RegexExtractFactory};

/// The exact outer grammar, tolerating the CRLF that GitHub's web UI submits.
///
/// * the marker is a whole first line;
/// * line two is empty;
/// * line three is one non-empty summary of at most 120 characters; and
/// * the JSON object runs to end-of-comment, so trailing text never matches.
const WORKGRAPH_COMMENT_PATTERN: &str = concat!(
    r"(?s)^WorkGraphEvent/v1\r?\n\r?\n",
    r"(?<summary>[^\r\n]{1,120})\r?\n\r?\n",
    r"(?<payload>\{.*\})$"
);

const ITEM: &str = "PVTI_lADOABCDEF4AbcDEzgXYZ123";
const SUBJECT: &str = "I_kwDOABCDEF6ABCDE";
const RUN_ID: &str = "run:sha256:775813253e0b6106e5a5f40ea02dcee45021121ce3f79f2d23c180d9b3027664";
const EVENT_ID: &str =
    "event:sha256:0852afc26f01ca9ce28d446b98d913776fc8dbe88a42cb3fb9b4b1761f5ecb7f";

/// The canonical `ExecutionStarted` document from the shared crate's
/// cross-language vectors (`components/workgraph-common/vectors`).
fn canonical_event_json() -> String {
    json!({
        "schemaVersion": "workgraph.event/v1",
        "eventId": EVENT_ID,
        "eventType": "ExecutionStarted",
        "runId": RUN_ID,
        "projectItemNodeId": ITEM,
        "subjectNodeId": SUBJECT,
        "payload": {
            "executionId": "execution:6f0a6b2f-6f1e-52b2-9f34-6d21b1f9a5c7",
            "taskId": "agent-task-1234"
        }
    })
    .to_string()
}

fn comment_body(summary: &str) -> String {
    format!(
        "WorkGraphEvent/v1\n\n{summary}\n\n{}",
        canonical_event_json()
    )
}

fn middleware_config(name: &str, kind: &str, config: Value) -> Arc<SourceMiddlewareConfig> {
    Arc::new(SourceMiddlewareConfig {
        name: Arc::from(name),
        kind: Arc::from(kind),
        config: config
            .as_object()
            .expect("middleware test configuration must be an object")
            .clone(),
    })
}

fn pipeline() -> SourceMiddlewarePipeline {
    // Only `workgraph.event/v1` documents are promoted, and only the common
    // envelope is promoted from them.
    let promote_event = concat!(
        "if .workgraphEvent and .workgraphEvent.schemaVersion == \"workgraph.event/v1\" ",
        "then {id: .workgraphEvent.eventId, eventId: .workgraphEvent.eventId, ",
        "eventType: .workgraphEvent.eventType, runId: .workgraphEvent.runId, ",
        "projectItemNodeId: .workgraphEvent.projectItemNodeId, ",
        "subjectNodeId: .workgraphEvent.subjectNodeId} else empty end"
    );
    let promote_subject_edge = concat!(
        "if .workgraphEvent and .workgraphEvent.schemaVersion == \"workgraph.event/v1\" ",
        "then {id: (\"about:\" + .workgraphEvent.eventId), eventId: .workgraphEvent.eventId, ",
        "subjectNodeId: .workgraphEvent.subjectNodeId} else empty end"
    );
    let event_mappings = json!([
        {
            "op": "Insert",
            "elementType": "Node",
            "label": "\"WorkGraphEvent\"",
            "id": ".id",
            "query": promote_event
        },
        {
            "op": "Insert",
            "elementType": {
                "relation": {
                    "inNodeId": ".subjectNodeId",
                    "outNodeId": ".eventId"
                }
            },
            "label": "\"ABOUT\"",
            "id": ".id",
            "query": promote_subject_edge
        }
    ]);
    let update_mappings = json!([
        {
            "op": "Update",
            "elementType": "Node",
            "label": "\"WorkGraphEvent\"",
            "id": ".id",
            "query": promote_event
        },
        {
            "op": "Update",
            "elementType": {
                "relation": {
                    "inNodeId": ".subjectNodeId",
                    "outNodeId": ".eventId"
                }
            },
            "label": "\"ABOUT\"",
            "id": ".id",
            "query": promote_subject_edge
        }
    ]);

    let configs = vec![
        middleware_config(
            "extract",
            "regex_extract",
            json!({
                "target_property": "body",
                "pattern": WORKGRAPH_COMMENT_PATTERN,
                "capture_group": "payload",
                "output_property": "workgraphEventJson",
                "on_missing": "passthrough",
                "on_no_match": "passthrough",
                "on_error": "fail"
            }),
        ),
        middleware_config(
            "parse",
            "parse_json",
            json!({
                "target_property": "workgraphEventJson",
                "output_property": "workgraphEvent",
                "on_missing": "passthrough",
                "on_error": "fail"
            }),
        ),
        middleware_config(
            "derive",
            "jq",
            json!({
                "preserve_input": true,
                "reconcile": true,
                "mappings": {
                    "GitHubComment": {
                        "insert": event_mappings,
                        "update": update_mappings
                    }
                }
            }),
        ),
    ];
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(RegexExtractFactory::new()));
    registry.register(Arc::new(ParseJsonFactory::new()));
    registry.register(Arc::new(JQFactory::new()));
    let container =
        MiddlewareContainer::new(&registry, configs).expect("middleware container should build");
    SourceMiddlewarePipeline::new(
        &container,
        vec!["extract".into(), "parse".into(), "derive".into()],
    )
    .expect("middleware pipeline should build")
}

fn node_change(operation: &str, id: &str, label: &str, properties: Value) -> SourceChange {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("github", id),
            labels: Arc::new([Arc::from(label)]),
            effective_from: 10,
        },
        properties: ElementPropertyMap::from(properties),
    };
    match operation {
        "insert" => SourceChange::Insert { element },
        "update" => SourceChange::Update { element },
        _ => panic!("unsupported operation"),
    }
}

fn comment_change(id: &str, body: &str) -> SourceChange {
    node_change("insert", id, "GitHubComment", json!({ "body": body }))
}

async fn index_outputs(index: &InMemoryElementIndex, changes: &[SourceChange]) {
    for change in changes {
        if let SourceChange::Insert { element } | SourceChange::Update { element } = change {
            index
                .set_element(element, &Vec::new())
                .await
                .expect("test output should be indexed");
        }
    }
}

fn inserted_elements(changes: &[SourceChange]) -> Vec<&Element> {
    changes
        .iter()
        .filter_map(|change| match change {
            SourceChange::Insert { element } => Some(element),
            _ => None,
        })
        .collect()
}

fn promoted_event(changes: &[SourceChange]) -> Option<&Element> {
    inserted_elements(changes).into_iter().find(|element| {
        element
            .get_metadata()
            .labels
            .iter()
            .any(|label| label.as_ref() == "WorkGraphEvent")
    })
}

fn property(element: &Element, key: &str) -> String {
    match element.get_properties().get(key) {
        Some(ElementValue::String(value)) => value.to_string(),
        other => panic!("expected string property '{key}', got {other:?}"),
    }
}

#[tokio::test]
async fn strict_workgraph_comments_are_extracted_and_promoted() {
    let pipeline = pipeline();
    let index = Arc::new(InMemoryElementIndex::new());

    let body = comment_body("WorkGraph started the issue validation agent for owner/repo#742");
    let outputs = pipeline
        .process(comment_change("comment:event", &body), index.clone())
        .await
        .expect("strict comment should fan out");

    assert_eq!(
        outputs.len(),
        3,
        "event node, subject edge, and the comment"
    );
    let event = promoted_event(&outputs).expect("event node is promoted");
    assert_eq!(event.get_reference().element_id.as_ref(), EVENT_ID);
    assert_eq!(property(event, "eventType"), "ExecutionStarted");
    assert_eq!(property(event, "runId"), RUN_ID);
    assert_eq!(property(event, "projectItemNodeId"), ITEM);
    assert_eq!(property(event, "subjectNodeId"), SUBJECT);

    let edge = inserted_elements(&outputs)
        .into_iter()
        .find(|element| matches!(element, Element::Relation { .. }))
        .expect("subject edge is promoted");
    let Element::Relation {
        metadata,
        in_node,
        out_node,
        ..
    } = edge
    else {
        unreachable!("filtered to relations");
    };
    assert_eq!(
        metadata.labels.first().map(|label| label.as_ref()),
        Some("ABOUT")
    );
    assert_eq!(in_node.element_id.as_ref(), SUBJECT);
    assert_eq!(out_node.element_id.as_ref(), EVENT_ID);
}

#[tokio::test]
async fn crlf_bodies_are_extracted() {
    let pipeline = pipeline();
    let index = Arc::new(InMemoryElementIndex::new());

    let body = comment_body("WorkGraph started the issue validation agent").replace('\n', "\r\n");
    let outputs = pipeline
        .process(comment_change("comment:crlf", &body), index)
        .await
        .expect("CRLF comment should fan out");

    let event = promoted_event(&outputs).expect("event node is promoted from CRLF body");
    assert_eq!(event.get_reference().element_id.as_ref(), EVENT_ID);
}

#[tokio::test]
async fn unrelated_and_legacy_bodies_pass_through_untouched() {
    let pipeline = pipeline();
    let index = Arc::new(InMemoryElementIndex::new());
    let json = canonical_event_json();

    let ignored = [
        (
            "issue",
            node_change("insert", "issue:1", "Issue", json!({"title": "Issue"})),
        ),
        (
            "ordinary comment",
            comment_change("comment:plain", "ordinary comment"),
        ),
        // Retired format 1: a pure-JSON comment body.
        (
            "legacy pure JSON",
            comment_change("comment:legacy-json", &json),
        ),
        // Retired format 2: marker plus a fenced JSON block.
        (
            "legacy fenced",
            comment_change(
                "comment:legacy-fence",
                &format!("WorkGraphEvent/v1\n```json\n{json}\n```"),
            ),
        ),
        (
            "fenced json slot",
            comment_change(
                "comment:fenced-slot",
                &format!("WorkGraphEvent/v1\n\nSummary line\n\n```json\n{json}\n```"),
            ),
        ),
        (
            "missing summary",
            comment_change(
                "comment:no-summary",
                &format!("WorkGraphEvent/v1\n\n{json}"),
            ),
        ),
        (
            "empty summary",
            comment_change(
                "comment:empty-summary",
                &format!("WorkGraphEvent/v1\n\n\n\n{json}"),
            ),
        ),
        (
            "multiline summary",
            comment_change(
                "comment:multiline-summary",
                &format!("WorkGraphEvent/v1\n\nline one\nline two\n\n{json}"),
            ),
        ),
        (
            "over-long summary",
            comment_change(
                "comment:long-summary",
                &format!("WorkGraphEvent/v1\n\n{}\n\n{json}", "s".repeat(121)),
            ),
        ),
        (
            "trailing text",
            comment_change(
                "comment:trailing-text",
                &format!("WorkGraphEvent/v1\n\nSummary line\n\n{json}\n\nthanks!"),
            ),
        ),
        (
            "trailing newline",
            comment_change(
                "comment:trailing-newline",
                &format!("WorkGraphEvent/v1\n\nSummary line\n\n{json}\n"),
            ),
        ),
        (
            "marker is not a whole line",
            comment_change(
                "comment:marker-suffix",
                &format!("WorkGraphEvent/v10\n\nSummary line\n\n{json}"),
            ),
        ),
    ];

    for (name, change) in ignored {
        let outputs = pipeline
            .process(change.clone(), index.clone())
            .await
            .unwrap_or_else(|error| panic!("{name} should pass through, got {error:?}"));
        assert_eq!(
            outputs,
            vec![change],
            "{name} must pass through byte-identically"
        );
    }
}

#[tokio::test]
async fn foreign_schema_versions_are_never_promoted() {
    let pipeline = pipeline();
    let index = Arc::new(InMemoryElementIndex::new());

    let body = comment_body("Summary line").replace("workgraph.event/v1", "workgraph.event/v2");
    let outputs = pipeline
        .process(comment_change("comment:v2", &body), index)
        .await
        .expect("foreign schema version should not fail the pipeline");

    assert!(
        promoted_event(&outputs).is_none(),
        "only workgraph.event/v1 documents may be promoted"
    );
    assert_eq!(outputs.len(), 1, "only the original comment is emitted");
}

#[tokio::test]
async fn editing_a_comment_out_of_the_format_reconciles_promoted_elements() {
    let pipeline = pipeline();
    let index = Arc::new(InMemoryElementIndex::new());

    let body = comment_body("WorkGraph started the issue validation agent");
    let created = pipeline
        .process(comment_change("comment:event", &body), index.clone())
        .await
        .expect("event should fan out");
    index_outputs(index.as_ref(), &created).await;

    let invalidated = pipeline
        .process(
            node_change(
                "update",
                "comment:event",
                "GitHubComment",
                json!({"body": "edited into an ordinary comment"}),
            ),
            index.clone(),
        )
        .await
        .expect("event invalidation should reconcile");

    let deleted: HashSet<String> = invalidated
        .iter()
        .filter_map(|change| match change {
            SourceChange::Delete { metadata } => Some(metadata.reference.element_id.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        deleted,
        HashSet::from([EVENT_ID.to_string(), format!("about:{EVENT_ID}")])
    );
    assert!(matches!(
        invalidated.last(),
        Some(SourceChange::Update { element })
            if element.get_reference().element_id.as_ref() == "comment:event"
    ));
}
