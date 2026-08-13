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

use std::sync::Arc;

use drasi_core::{
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::{MiddlewareSetupError, SourceMiddlewareFactory},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
        SourceMiddlewareConfig,
    },
};
use serde_json::{json, Value};

use super::RegexExtractFactory;

fn middleware(config: Value) -> Arc<dyn drasi_core::interface::SourceMiddleware> {
    let config = SourceMiddlewareConfig {
        name: "extract".into(),
        kind: "regex_extract".into(),
        config: config.as_object().unwrap().clone(),
    };
    RegexExtractFactory::new().create(&config).unwrap()
}

fn change(properties: Value) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("github", "comment:1"),
                labels: Arc::new([Arc::from("Comment")]),
                effective_from: 42,
            },
            properties: ElementPropertyMap::from(properties),
        },
    }
}

fn base_config() -> Value {
    json!({
        "target_property": "body",
        "pattern": r"(?s)^Header\n```json\s*(?<payload>.*?)\s*```",
        "capture_group": "payload",
        "output_property": "payload"
    })
}

#[tokio::test]
async fn extracts_named_capture_and_preserves_metadata() {
    let input = change(json!({"body": "Header\n```json\n{\"id\":1}\n```"}));
    let expected_metadata = match &input {
        SourceChange::Insert { element } => element.get_metadata().clone(),
        _ => unreachable!(),
    };
    let output = middleware(base_config())
        .process(input, &InMemoryElementIndex::new())
        .await
        .unwrap();

    let SourceChange::Insert { element } = &output[0] else {
        panic!("expected insert");
    };
    assert_eq!(element.get_metadata(), &expected_metadata);
    assert_eq!(
        element.get_properties().get("payload"),
        Some(&ElementValue::String(Arc::from("{\"id\":1}")))
    );
}

#[tokio::test]
async fn missing_and_no_match_pass_through_by_default() {
    for input in [
        change(json!({"title": "ordinary"})),
        change(json!({"body": "ordinary comment"})),
    ] {
        let expected = input.clone();
        let output = middleware(base_config())
            .process(input, &InMemoryElementIndex::new())
            .await
            .unwrap();
        assert_eq!(output, vec![expected]);
    }
}

#[tokio::test]
async fn policies_drop_or_fail_as_configured() {
    let drop = middleware(json!({
        "target_property": "body",
        "pattern": "(?<value>match)",
        "capture_group": "value",
        "output_property": "value",
        "on_no_match": "drop"
    }));
    assert!(drop
        .process(
            change(json!({"body": "nope"})),
            &InMemoryElementIndex::new()
        )
        .await
        .unwrap()
        .is_empty());

    let fail = middleware(json!({
        "target_property": "body",
        "pattern": "(?<value>match)?",
        "capture_group": "value",
        "output_property": "value",
        "on_error": "fail"
    }));
    let error = fail
        .process(change(json!({"body": ""})), &InMemoryElementIndex::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("did not participate"));
}

#[tokio::test]
async fn enforces_capture_size_and_collision_policy() {
    let oversize = middleware(json!({
        "target_property": "body",
        "pattern": "(.*)",
        "capture_group": 1,
        "output_property": "capture",
        "max_capture_size": 3
    }));
    assert!(oversize
        .process(
            change(json!({"body": "four"})),
            &InMemoryElementIndex::new()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("max_capture_size"));

    let collision = middleware(json!({
        "target_property": "body",
        "pattern": "(.*)",
        "capture_group": 1,
        "output_property": "capture"
    }));
    assert!(collision
        .process(
            change(json!({"body": "new", "capture": "old"})),
            &InMemoryElementIndex::new()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("already exists"));

    let overwrite = middleware(json!({
        "target_property": "body",
        "pattern": "(.*)",
        "capture_group": 1,
        "output_property": "capture",
        "on_collision": "overwrite"
    }));
    let output = overwrite
        .process(
            change(json!({"body": "new", "capture": "old"})),
            &InMemoryElementIndex::new(),
        )
        .await
        .unwrap();
    let SourceChange::Insert { element } = &output[0] else {
        panic!("expected insert");
    };
    assert_eq!(
        element.get_properties().get("capture"),
        Some(&ElementValue::String(Arc::from("new")))
    );
}

#[test]
fn validates_pattern_and_capture_group_at_setup() {
    for config in [
        json!({
            "target_property": "body",
            "pattern": "(",
            "capture_group": 1,
            "output_property": "capture"
        }),
        json!({
            "target_property": "body",
            "pattern": "(value)",
            "capture_group": "missing",
            "output_property": "capture"
        }),
        json!({
            "target_property": "body",
            "pattern": "(value)",
            "capture_group": 2,
            "output_property": "capture"
        }),
    ] {
        let config = SourceMiddlewareConfig {
            name: "extract".into(),
            kind: "regex_extract".into(),
            config: config.as_object().unwrap().clone(),
        };
        assert!(matches!(
            RegexExtractFactory::new().create(&config),
            Err(MiddlewareSetupError::InvalidConfiguration(_))
        ));
    }
}

#[tokio::test]
async fn update_is_typed_and_delete_passes_through() {
    let insert = change(json!({"body": "Header\n```json\n{}\n```"}));
    let SourceChange::Insert { element } = insert else {
        unreachable!()
    };
    let update = SourceChange::Update { element };
    let output = middleware(base_config())
        .process(update, &InMemoryElementIndex::new())
        .await
        .unwrap();
    assert!(matches!(output.as_slice(), [SourceChange::Update { .. }]));

    let delete = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("github", "comment:1"),
            labels: Arc::new([Arc::from("Comment")]),
            effective_from: 43,
        },
    };
    let output = middleware(base_config())
        .process(delete.clone(), &InMemoryElementIndex::new())
        .await
        .unwrap();
    assert_eq!(output, vec![delete]);
}

#[tokio::test]
async fn supports_full_match_and_in_place_output() {
    let subject = middleware(json!({
        "target_property": "body",
        "pattern": "value",
        "capture_group": 0,
        "output_property": "body"
    }));
    let output = subject
        .process(
            change(json!({"body": "value"})),
            &InMemoryElementIndex::new(),
        )
        .await
        .unwrap();
    let SourceChange::Insert { element } = &output[0] else {
        panic!("expected insert");
    };
    assert_eq!(
        element.get_properties().get("body"),
        Some(&ElementValue::String(Arc::from("value")))
    );
}
