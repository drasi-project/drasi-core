// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use chrono::{TimeZone, Utc};

fn result(query: &str, changes: Vec<ResultDiff>) -> QueryResult {
    QueryResult::new(
        query.into(),
        9,
        Utc.timestamp_millis_opt(123).unwrap(),
        changes,
        HashMap::from([("origin".into(), json!("test"))]),
    )
}

fn added() -> ResultDiff {
    ResultDiff::Add {
        data: json!({"id": 1, "group": "north", "enabled": true}),
        row_signature: 42,
    }
}

fn config(body: &str, path: Option<&str>) -> QueryConfig {
    QueryConfig {
        added: Some(TemplateSpec::with_extension(
            body,
            SseExtension {
                path: path.map(str::to_owned),
            },
        )),
        ..Default::default()
    }
}

fn render(renderer: &SseRenderer, result: &QueryResult) -> Vec<SseEvent> {
    renderer.render(result, 456, |error| {
        panic!("unexpected rendering error: {error}")
    })
}

#[test]
fn default_payload_preserves_the_complete_result_batch_and_row_signatures() {
    let renderer = SseRenderer::new("/events".into(), HashMap::new(), None);
    let result = result(
        "q",
        vec![
            added(),
            ResultDiff::Delete {
                data: json!({"id": 2}),
                row_signature: 43,
            },
        ],
    );
    let events = render(&renderer, &result);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].path, "/events");
    let payload: Value = serde_json::from_str(&events[0].payload).unwrap();
    assert_eq!(payload, default_payload(&result, 456));
    assert_eq!(payload["results"][0]["row_signature"], 42);
    assert_eq!(payload["results"][1]["row_signature"], 43);
}

#[test]
fn full_query_routes_precede_last_segments_and_default_routes() {
    let renderer = SseRenderer::new(
        "/events".into(),
        HashMap::from([
            ("source.q".into(), config("full", Some("/full"))),
            ("q".into(), config("short", Some("short"))),
        ]),
        Some(config("default", None)),
    );
    for (query, path, payload) in [
        ("source.q", "/full", "full"),
        ("other.q", "/events/short", "short"),
        ("other", "/events", "default"),
    ] {
        assert_eq!(
            render(&renderer, &result(query, vec![added()])),
            vec![SseEvent {
                path: path.into(),
                payload: payload.into(),
            }]
        );
    }
}

#[test]
fn empty_templates_use_the_legacy_singular_result_shape() {
    let renderer = SseRenderer::new(
        "/events".into(),
        HashMap::new(),
        Some(config("", Some("items"))),
    );
    let events = render(&renderer, &result("q", vec![added()]));
    assert_eq!(events[0].path, "/events/items");
    let payload: Value = serde_json::from_str(&events[0].payload).unwrap();
    assert_eq!(
        payload,
        json!({"queryId": "q", "result": added(), "timestamp": 456})
    );
    assert!(payload.get("results").is_none());
}

#[test]
fn dynamic_paths_and_json_helpers_share_the_real_result_context() {
    let renderer = SseRenderer::new(
        "/events".into(),
        HashMap::from([(
            "q".into(),
            config(
                r#"{"row":{{json after}},"metadata":{{json metadata}},"query":"{{query_id}}","name":"{{query_name}}","operation":"{{operation}}","time":{{timestamp}}}"#,
                Some("groups/{{after.group}}"),
            ),
        )]),
        None,
    );
    assert_eq!(renderer.static_paths(), BTreeSet::from(["/events".into()]));
    let events = render(&renderer, &result("q", vec![added()]));
    assert_eq!(events[0].path, "/events/groups/north");
    let payload: Value = serde_json::from_str(&events[0].payload).unwrap();
    assert_eq!(
        payload["row"],
        json!({"id":1,"group":"north","enabled":true})
    );
    assert_eq!(payload["metadata"], json!({"origin":"test"}));
    assert_eq!(payload["query"], "q");
    assert_eq!(payload["name"], "q");
    assert_eq!(payload["operation"], "ADD");
    assert_eq!(payload["time"], 456);
}

#[test]
fn update_delete_and_aggregation_use_the_existing_operation_templates() {
    let updated = TemplateSpec::with_extension(
        r#"{"operation":"{{operation}}","before":{{json before}},"after":{{json after}},"data":{{json data}}}"#,
        SseExtension { path: None },
    );
    let renderer = SseRenderer::new(
        "/events".into(),
        HashMap::new(),
        Some(QueryConfig {
            added: None,
            updated: Some(updated),
            deleted: Some(TemplateSpec::with_extension(
                "{{json before}}",
                SseExtension { path: None },
            )),
        }),
    );
    let events = render(
        &renderer,
        &result(
            "q",
            vec![
                ResultDiff::Update {
                    data: json!({"raw": 3}),
                    before: json!({"count": 1}),
                    after: json!({"count": 2}),
                    grouping_keys: None,
                    row_signature: 7,
                },
                ResultDiff::Aggregation {
                    before: None,
                    after: json!({"sum": 5}),
                    row_signature: 8,
                },
                ResultDiff::Delete {
                    data: json!({"id": 1}),
                    row_signature: 42,
                },
                ResultDiff::Noop,
            ],
        ),
    );
    assert_eq!(events.len(), 3);
    let update: Value = serde_json::from_str(&events[0].payload).unwrap();
    assert_eq!(update["operation"], "UPDATE");
    assert_eq!(update["data"], json!({"raw": 3}));
    assert_eq!(update["before"], json!({"count": 1}));
    assert_eq!(update["after"], json!({"count": 2}));
    let aggregation: Value = serde_json::from_str(&events[1].payload).unwrap();
    assert_eq!(aggregation["operation"], "AGGREGATION");
    assert!(aggregation["before"].is_null());
    assert_eq!(aggregation["after"], json!({"sum": 5}));
    assert_eq!(events[2].payload, r#"{"id":1}"#);
}

#[test]
fn static_paths_include_all_operations_and_default_templates() {
    let renderer = SseRenderer::new(
        "/events".into(),
        HashMap::from([("q".into(), config("", Some("items")))]),
        Some(config("", Some("/default"))),
    );
    assert_eq!(
        renderer.static_paths(),
        BTreeSet::from(["/events".into(), "/events/items".into(), "/default".into(),])
    );
}

#[test]
fn configured_missing_operations_remain_filtered_instead_of_using_another_route() {
    let renderer = SseRenderer::new(
        "/events".into(),
        HashMap::from([("q".into(), QueryConfig::default())]),
        Some(config("must not be used", None)),
    );
    assert!(render(&renderer, &result("q", vec![added()])).is_empty());
}

#[test]
fn validation_covers_body_and_path_templates_and_query_names() {
    for route in [
        config("{{invalid syntax", None),
        config("", Some("{{invalid path")),
    ] {
        assert!(SseRenderer::validate(
            &HashMap::from([("q".into(), route)]),
            None,
            &["source.q".into()],
        )
        .is_err());
    }
    assert!(SseRenderer::validate(
        &HashMap::from([("missing".into(), config("", None))]),
        None,
        &["q".into()],
    )
    .is_err());
    SseRenderer::validate(
        &HashMap::from([("q".into(), config("{{json after}}", None))]),
        None,
        &["source.q".into()],
    )
    .unwrap();
}

#[test]
fn runtime_render_failures_report_the_default_payload_fallback() {
    let routes = HashMap::from([(
        "q".into(),
        config("{{#if after.enabled}}{{missing after}}{{/if}}", None),
    )]);
    SseRenderer::validate(&routes, None, &["q".into()]).unwrap();
    let renderer = SseRenderer::new("/events".into(), routes, None);
    let mut errors = Vec::new();
    let events = renderer.render(&result("q", vec![added()]), 456, |error| errors.push(error));
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("using default result payload"));
    let payload: Value = serde_json::from_str(&events[0].payload).unwrap();
    assert_eq!(payload["result"], json!(added()));
}
