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

use std::{collections::HashSet, sync::Arc};

use drasi_core::{
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::ElementIndex,
    middleware::{MiddlewareContainer, MiddlewareTypeRegistry, SourceMiddlewarePipeline},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        SourceMiddlewareConfig,
    },
};
use serde_json::{json, Value};

use crate::{jq::JQFactory, parse_json::ParseJsonFactory, regex_extract::RegexExtractFactory};

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
    let configs = vec![
        middleware_config(
            "extract",
            "regex_extract",
            json!({
                "target_property": "body",
                "pattern": r"(?s)^WorkGraphEvent/v1\s*\n```json\s*(?<payload>.*?)\s*```",
                "capture_group": "payload",
                "output_property": "event_payload",
                "on_missing": "passthrough",
                "on_no_match": "passthrough",
                "on_error": "fail"
            }),
        ),
        middleware_config(
            "parse",
            "parse_json",
            json!({
                "target_property": "event_payload",
                "output_property": "event",
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
                        "insert": [
                            {
                                "op": "Insert",
                                "elementType": "Node",
                                "label": "\"WorkItem\"",
                                "id": ".id",
                                "query": "if .event then .event.work_item else empty end"
                            },
                            {
                                "op": "Insert",
                                "elementType": {
                                    "relation": {
                                        "inNodeId": ".issue_id",
                                        "outNodeId": ".work_item_id"
                                    }
                                },
                                "label": "\"TRACKS\"",
                                "id": ".id",
                                "query": "if .event then .event.relation else empty end"
                            }
                        ],
                        "update": [
                            {
                                "op": "Update",
                                "elementType": "Node",
                                "label": "\"WorkItem\"",
                                "id": ".id",
                                "query": "if .event then .event.work_item else empty end"
                            },
                            {
                                "op": "Update",
                                "elementType": {
                                    "relation": {
                                        "inNodeId": ".issue_id",
                                        "outNodeId": ".work_item_id"
                                    }
                                },
                                "label": "\"TRACKS\"",
                                "id": ".id",
                                "query": "if .event then .event.relation else empty end"
                            }
                        ]
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

#[tokio::test]
async fn source_wide_pipeline_passes_unrelated_elements_and_reconciles_events() {
    let pipeline = pipeline();
    let index = Arc::new(InMemoryElementIndex::new());

    for unrelated in [
        node_change("insert", "issue:1", "Issue", json!({"title": "Issue"})),
        node_change(
            "insert",
            "comment:ordinary",
            "GitHubComment",
            json!({"body": "ordinary comment"}),
        ),
    ] {
        let output = pipeline
            .process(unrelated.clone(), index.clone())
            .await
            .expect("unrelated input should pass through");
        assert_eq!(output, vec![unrelated]);
    }

    let event = node_change(
        "insert",
        "comment:event",
        "GitHubComment",
        json!({
            "body": "WorkGraphEvent/v1\n```json\n{\"work_item\":{\"id\":\"work:1\",\"title\":\"Build\"},\"relation\":{\"id\":\"tracks:1\",\"issue_id\":\"issue:1\",\"work_item_id\":\"work:1\"}}\n```"
        }),
    );
    let created = pipeline
        .process(event, index.clone())
        .await
        .expect("event should fan out");
    assert_eq!(
        created
            .iter()
            .filter(|change| matches!(change, SourceChange::Insert { .. }))
            .count(),
        3
    );
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
        HashSet::from(["work:1".to_string(), "tracks:1".to_string()])
    );
    assert!(matches!(
        invalidated.last(),
        Some(SourceChange::Update { element })
            if element.get_reference().element_id.as_ref() == "comment:event"
    ));
}
