// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::Context;
use chrono::{DateTime, Utc};
use drasi_computation_contracts::{
    ChangeEnvelope, ChangeOperation, ChangeSet, ComponentId, ContextEntry, ContextValue,
    ElementImage, OutputProgress, OutputReadError, OutputSnapshot, SnapshotProvider,
};
use drasi_computation_plugin_sdk::{
    Component, ControlSender, CreateRequest, ExportedPlugin, Factory,
};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::Notify;

use crate::ViewFactory;

fn identity_configuration() -> Value {
    json!({"stream": "view/out", "middleware": [], "pipeline": []})
}

fn mapped_configuration(rules: Value) -> Value {
    json!({
        "stream": "view/out",
        "middleware": [{"name": "project", "kind": "map", "config": {"Input": {"insert": rules}}}],
        "pipeline": ["project"]
    })
}

fn key_configuration() -> Value {
    mapped_configuration(json!([{
        "id": "$.key",
        "label": "Projected",
        "properties": {"value": "$.value", "owner": "$.owner"}
    }]))
}

fn create(configuration: Value) -> Result<View> {
    View::new(
        ViewFactory
            .metadata()
            .descriptor(ComponentId::try_new("view")?)?,
        configuration,
        Some(&Scope {
            instance_id: "instance".into(),
            graph_id: "graph".into(),
            generation: 1,
        }),
    )
}

async fn start(configuration: Value) -> Result<View> {
    let mut view = create(configuration)?;
    view.start().await?;
    Ok(view)
}

fn node(source: &str, id: &str, label: &str, time: u64, properties: Value) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, id),
            labels: vec![Arc::from(label)].into(),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

fn row(id: &str, time: u64, properties: Value) -> Element {
    node("source", id, "Input", time, properties)
}

fn projected(id: &str, time: u64, properties: Value) -> Element {
    node("source", id, "Projected", time, properties)
}

fn deletion(id: &str, time: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: Arc::from([]),
            effective_from: time,
        },
    }
}

fn input(changes: &[SourceChange], sequence: u64) -> Result<InputEnvelope> {
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_changes(
            changes,
            StreamId::try_new("source/out")?,
            sequence,
            Some(DateTime::<Utc>::UNIX_EPOCH),
        )?,
    })
}

async fn emit(view: &mut View, changes: &[SourceChange]) -> Result<Vec<SourceChange>> {
    let input = input(changes, view.sequence + 1)?;
    let mut outputs = view.transform(input).await?;
    assert_eq!(outputs.len(), 1);
    let output = outputs.pop().context("missing output")?;
    assert_eq!(output.port.as_str(), "out");
    let progress =
        GraphProducerProgress::from_envelope(&output.envelope)?.context("missing view progress")?;
    assert!(!progress.identity().persistent());
    assert_eq!(progress.sequence(), output.envelope.system().sequence());
    assert_eq!(progress.identity().component_id().as_str(), "view");
    GraphChangeCodec::decode_changes(&output.envelope).map_err(Into::into)
}

fn reader(view: &View) -> Result<Arc<dyn SnapshotProvider>> {
    view.output_readers()
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|readers| readers.snapshot())
        .cloned()
        .context("missing snapshot reader")
}

fn rows(snapshot: &OutputSnapshot) -> Result<HashMap<ElementReference, Element>> {
    snapshot
        .records()
        .values()
        .map(|record| {
            let element = Element::try_from(GraphChangeCodec::decode_element(record)?)?;
            Ok((element.get_reference().clone(), element))
        })
        .collect()
}

async fn assert_rows(view: &View, expected: &[Element]) -> Result<()> {
    let snapshot = reader(view)?.snapshot().await?;
    let expected = expected
        .iter()
        .cloned()
        .map(|element| (element.get_reference().clone(), element))
        .collect::<HashMap<_, _>>();
    assert_eq!(rows(&snapshot)?, expected);
    assert_eq!(snapshot.cursor().sequence(), view.sequence);
    Ok(())
}

#[tokio::test]
async fn patches_preserve_omitted_values_distinguish_null_and_drop_replaced_properties(
) -> Result<()> {
    let mut view = start(identity_configuration()).await?;
    let initial = row(
        "one",
        1,
        json!({"keep": "original", "remove": 1, "nullable": 2}),
    );
    assert_eq!(
        emit(
            &mut view,
            &[SourceChange::Insert {
                element: initial.clone()
            }]
        )
        .await?,
        vec![SourceChange::Insert { element: initial }]
    );
    let patched = row(
        "one",
        2,
        json!({"keep": "original", "nullable": null, "added": true}),
    );
    assert_eq!(
        emit(
            &mut view,
            &[SourceChange::Update {
                element: row("one", 2, json!({"nullable": null, "added": true})),
                removed_properties: ["remove".into()].into(),
            }],
        )
        .await?,
        vec![SourceChange::Replace {
            element: patched.clone()
        }]
    );
    assert_rows(&view, &[patched]).await?;
    let replacement = row("one", 3, json!({"only": 3}));
    assert_eq!(
        emit(
            &mut view,
            &[SourceChange::Replace {
                element: replacement.clone()
            }]
        )
        .await?,
        vec![SourceChange::Replace {
            element: replacement.clone()
        }]
    );
    assert_rows(&view, &[replacement]).await
}

#[tokio::test]
async fn filter_retractions_retain_full_inputs_for_later_patches() -> Result<()> {
    let mut view = start(mapped_configuration(json!([{
        "condition": "$[?(@.active == true)]",
        "id": "$.key",
        "label": "Projected",
        "properties": {"value": "$.value"}
    }])))
    .await?;
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: row("one", 1, json!({"key": "kept", "active": true, "value": 7})),
        }],
    )
    .await?;
    let retracted = emit(
        &mut view,
        &[SourceChange::update(row(
            "one",
            2,
            json!({"active": false}),
        ))],
    )
    .await?;
    assert_eq!(
        retracted,
        vec![SourceChange::Delete {
            metadata: projected("kept", 2, json!({})).get_metadata().clone(),
        }]
    );
    assert_rows(&view, &[]).await?;
    assert_eq!(
        emit(
            &mut view,
            &[SourceChange::update(row("one", 3, json!({"active": true})))]
        )
        .await?,
        vec![SourceChange::Insert {
            element: projected("kept", 3, json!({"value": 7}))
        }]
    );
    let removed = emit(
        &mut view,
        &[SourceChange::Update {
            element: row("one", 4, json!({})),
            removed_properties: ["active".into()].into(),
        }],
    )
    .await?;
    assert_eq!(removed.len(), 1);
    assert!(matches!(&removed[0], SourceChange::Delete { .. }));
    assert_rows(&view, &[]).await?;
    assert!(emit(&mut view, &[deletion("one", 5)]).await?.is_empty());
    assert!(view
        .transform(input(
            &[SourceChange::update(row("one", 6, json!({"active": true})))],
            6
        )?)
        .await
        .is_err());
    Ok(())
}

#[tokio::test]
async fn one_to_many_removes_old_children_and_replaces_child_images() -> Result<()> {
    let mut view = start(mapped_configuration(json!([{
        "selector": "$.children[*]",
        "id": "$['$selected'].id",
        "label": "Projected",
        "properties": {"value": "$['$selected'].value", "optional": "$['$selected'].optional"}
    }])))
    .await?;
    let first = emit(
        &mut view,
        &[SourceChange::Insert {
            element: row(
                "parent",
                1,
                json!({"children": [
                    {"id": "a", "value": 1},
                    {"id": "b", "value": 2, "optional": true}
                ]}),
            ),
        }],
    )
    .await?;
    assert_eq!(first.len(), 2);
    let changes = emit(
        &mut view,
        &[SourceChange::update(row(
            "parent",
            2,
            json!({
                "children": [{"id": "b", "value": 3}, {"id": "c", "value": 4}]
            }),
        ))],
    )
    .await?;
    assert_eq!(
        changes,
        vec![
            SourceChange::Delete {
                metadata: projected("a", 2, json!({})).get_metadata().clone()
            },
            SourceChange::Replace {
                element: projected("b", 2, json!({"value": 3}))
            },
            SourceChange::Insert {
                element: projected("c", 2, json!({"value": 4}))
            },
        ]
    );
    assert_rows(
        &view,
        &[
            projected("b", 2, json!({"value": 3})),
            projected("c", 2, json!({"value": 4})),
        ],
    )
    .await?;
    let removals = emit(
        &mut view,
        &[SourceChange::Update {
            element: row("parent", 3, json!({})),
            removed_properties: ["children".into()].into(),
        }],
    )
    .await?;
    assert_eq!(removals.len(), 2);
    assert!(removals
        .iter()
        .all(|change| matches!(change, SourceChange::Delete { .. })));
    assert_rows(&view, &[]).await
}

#[tokio::test]
async fn changed_keys_retract_old_identity_and_identity_only_delete_needs_no_mapping_data(
) -> Result<()> {
    let mut view = start(key_configuration()).await?;
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: row("input", 1, json!({"key": "old", "value": 8})),
        }],
    )
    .await?;
    assert_eq!(
        emit(
            &mut view,
            &[SourceChange::update(row("input", 2, json!({"key": "new"})))]
        )
        .await?,
        vec![
            SourceChange::Delete {
                metadata: projected("old", 2, json!({})).get_metadata().clone()
            },
            SourceChange::Insert {
                element: projected("new", 2, json!({"value": 8}))
            },
        ]
    );

    let ordinary = input(&[deletion("input", 3)], 3)?;
    let reference = GraphChangeCodec::element_reference(&ElementReference::new("source", "input"))?;
    let changes = ChangeSet::try_new(
        ordinary.envelope.changes().id().clone(),
        GraphChangeCodec::schema().descriptor().clone(),
        vec![ChangeOperation::Deleted {
            ordinal: 0,
            identity: reference,
            before: None,
        }],
    )?;
    let mut system = ordinary.envelope.system().as_ref().clone();
    system =
        system.with_timestamp(DateTime::<Utc>::from_timestamp_millis(3).context("valid time")?);
    let outputs = view
        .transform(InputEnvelope {
            port: ordinary.port,
            envelope: ChangeEnvelope::new(ordinary.envelope.id().clone(), changes, system),
        })
        .await?;
    assert_eq!(outputs.len(), 1);
    assert_eq!(
        GraphChangeCodec::decode_changes(&outputs[0].envelope)?,
        vec![SourceChange::Delete {
            metadata: projected("new", 3, json!({})).get_metadata().clone()
        }]
    );
    assert_rows(&view, &[]).await
}

#[tokio::test]
async fn maps_nodes_and_relationships_and_retracts_both_when_filtered_out() -> Result<()> {
    let mut view = start(mapped_configuration(json!([
        {
            "id": "$.key", "label": "Projected",
            "properties": {"value": "$.value"}
        },
        {
            "id": "$.link", "label": "Connects",
            "elementType": {"Relation": {"inNodeId": "$.from", "outNodeId": "$.key"}},
            "properties": {"weight": "$.weight"}
        }
    ])))
    .await?;
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: row(
                "input",
                1,
                json!({
                    "key": "child", "value": 9, "link": "edge", "from": "parent", "weight": 4
                }),
            ),
        }],
    )
    .await?;
    let relation = |time, target: &str, weight| Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "edge"),
            labels: vec![Arc::from("Connects")].into(),
            effective_from: time,
        },
        in_node: ElementReference::new("source", "parent"),
        out_node: ElementReference::new("source", target),
        properties: ElementPropertyMap::from(json!({"weight": weight})),
    };
    assert_rows(
        &view,
        &[
            projected("child", 1, json!({"value": 9})),
            relation(1, "child", 4),
        ],
    )
    .await?;
    let changed = emit(
        &mut view,
        &[SourceChange::update(row(
            "input",
            2,
            json!({
                "key": "other", "weight": 5
            }),
        ))],
    )
    .await?;
    assert_eq!(changed.len(), 3);
    assert!(changed.contains(&SourceChange::Replace {
        element: relation(2, "other", 5)
    }));
    assert_rows(
        &view,
        &[
            projected("other", 2, json!({"value": 9})),
            relation(2, "other", 5),
        ],
    )
    .await?;
    let removed = emit(
        &mut view,
        &[SourceChange::Update {
            element: row("input", 3, json!({})),
            removed_properties: ["key".into()].into(),
        }],
    )
    .await?;
    assert_eq!(removed.len(), 2);
    assert!(removed
        .iter()
        .all(|change| matches!(change, SourceChange::Delete { .. })));
    assert_rows(&view, &[]).await
}

#[tokio::test]
async fn relationship_patches_use_the_shared_patch_contract_and_relabel_pipeline() -> Result<()> {
    let mut view = start(json!({
        "stream": "view/out",
        "middleware": [{"name": "labels", "kind": "relabel", "config": {"labelMappings": {"Input": "Projected"}}}],
        "pipeline": ["labels"]
    })).await?;
    let relation = |time, target: &str, properties: Value| Element::Relation {
        metadata: row("edge", time, json!({})).get_metadata().clone(),
        in_node: ElementReference::new("upstream", "parent"),
        out_node: ElementReference::new("downstream", target),
        properties: ElementPropertyMap::from(properties),
    };
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: relation(1, "first", json!({"keep": 1, "remove": 2})),
        }],
    )
    .await?;
    let mut expected = relation(2, "second", json!({"keep": 1, "added": 3}));
    if let Element::Relation { metadata, .. } = &mut expected {
        metadata.labels = vec![Arc::from("Projected")].into();
    }
    assert_eq!(
        emit(
            &mut view,
            &[SourceChange::Update {
                element: relation(2, "second", json!({"added": 3})),
                removed_properties: ["remove".into()].into(),
            }]
        )
        .await?,
        vec![SourceChange::Replace {
            element: expected.clone()
        }]
    );
    assert_rows(&view, &[expected]).await?;
    assert_eq!(emit(&mut view, &[deletion("edge", 3)]).await?.len(), 1);
    assert_rows(&view, &[]).await
}

#[tokio::test]
async fn full_kind_changes_delete_then_add_but_patch_kind_changes_fail() -> Result<()> {
    let mut view = start(identity_configuration()).await?;
    let initial = row("same", 1, json!({"before": true}));
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: initial.clone(),
        }],
    )
    .await?;
    let relation = Element::Relation {
        metadata: row("same", 2, json!({})).get_metadata().clone(),
        in_node: ElementReference::new("source", "left"),
        out_node: ElementReference::new("source", "right"),
        properties: ElementPropertyMap::from(json!({"after": true})),
    };
    let error = view
        .transform(input(&[SourceChange::update(relation.clone())], 2)?)
        .await
        .expect_err("kind-changing patch must fail");
    assert!(error
        .downcast_ref::<drasi_computation_contracts::ElementPatchError>()
        .is_some());
    assert_rows(&view, std::slice::from_ref(&initial)).await?;
    let mut metadata = initial.get_metadata().clone();
    metadata.effective_from = 2;
    assert_eq!(
        emit(
            &mut view,
            &[SourceChange::Replace {
                element: relation.clone()
            }]
        )
        .await?,
        vec![
            SourceChange::Delete { metadata },
            SourceChange::Insert {
                element: relation.clone()
            }
        ]
    );
    assert_rows(&view, &[relation]).await
}

#[tokio::test]
async fn unknown_base_rejects_the_whole_envelope_without_advancing_inputs_or_sequence() -> Result<()>
{
    let mut view = start(identity_configuration()).await?;
    let error = view
        .transform(input(
            &[
                SourceChange::Insert {
                    element: row("known", 1, json!({"value": 1})),
                },
                SourceChange::update(row("unknown", 1, json!({"value": 2}))),
            ],
            1,
        )?)
        .await
        .expect_err("missing base must fail");
    assert!(format!("{error:#}").contains("no retained full input image"));
    assert_eq!(view.sequence, 0);
    assert_rows(&view, &[]).await?;
    assert!(view
        .transform(input(
            &[SourceChange::update(row("known", 2, json!({})))],
            2
        )?)
        .await
        .is_err());
    let full = row("known", 3, json!({"complete": true}));
    assert_eq!(
        emit(
            &mut view,
            &[SourceChange::Replace {
                element: full.clone()
            }]
        )
        .await?,
        vec![SourceChange::Insert {
            element: full.clone()
        }]
    );
    assert_eq!(view.sequence, 1);
    assert_rows(&view, &[full]).await
}

#[tokio::test]
async fn duplicate_outputs_are_errors_not_last_writer_wins() -> Result<()> {
    let mut view = start(key_configuration()).await?;
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: row("first", 1, json!({"key": "shared", "owner": "first"})),
        }],
    )
    .await?;
    let error = view
        .transform(input(
            &[SourceChange::Insert {
                element: row("second", 2, json!({"key": "shared", "owner": "second"})),
            }],
            2,
        )?)
        .await
        .expect_err("conflicting ownership must fail");
    assert!(format!("{error:#}").contains("emitted more than once"));
    assert_eq!(view.sequence, 1);
    assert_rows(&view, &[projected("shared", 1, json!({"owner": "first"}))]).await?;
    assert!(view
        .transform(input(
            &[SourceChange::update(row(
                "second",
                3,
                json!({"key": "different"})
            ))],
            3
        )?)
        .await
        .is_err());
    assert_eq!(emit(&mut view, &[deletion("first", 4)]).await?.len(), 1);
    assert_rows(&view, &[]).await?;

    let mut duplicate = start(mapped_configuration(json!([
        {"id": "$.key"}, {"id": "$.key"}
    ])))
    .await?;
    assert!(duplicate
        .transform(input(
            &[SourceChange::Insert {
                element: row("input", 1, json!({"key": "twice"})),
            }],
            1
        )?)
        .await
        .is_err());
    assert_rows(&duplicate, &[]).await
}

#[tokio::test]
async fn one_batch_can_exchange_output_ownership_and_still_delete_the_correct_child() -> Result<()>
{
    let mut view = start(key_configuration()).await?;
    emit(
        &mut view,
        &[
            SourceChange::Insert {
                element: row("a", 1, json!({"key": "left", "owner": "a"})),
            },
            SourceChange::Insert {
                element: row("b", 1, json!({"key": "right", "owner": "b"})),
            },
        ],
    )
    .await?;
    let changed = emit(
        &mut view,
        &[
            SourceChange::update(row("a", 2, json!({"key": "right"}))),
            SourceChange::update(row("b", 2, json!({"key": "left"}))),
        ],
    )
    .await?;
    assert_eq!(changed.len(), 2);
    assert!(changed
        .iter()
        .all(|change| matches!(change, SourceChange::Replace { .. })));
    assert_rows(
        &view,
        &[
            projected("left", 2, json!({"owner": "b"})),
            projected("right", 2, json!({"owner": "a"})),
        ],
    )
    .await?;
    assert_eq!(
        emit(&mut view, &[deletion("a", 3)]).await?,
        vec![SourceChange::Delete {
            metadata: projected("right", 3, json!({})).get_metadata().clone()
        }]
    );
    assert_rows(&view, &[projected("left", 2, json!({"owner": "b"}))]).await
}

#[tokio::test]
async fn source_namespaces_and_typed_input_values_are_preserved() -> Result<()> {
    let mut view = start(identity_configuration()).await?;
    let mut first = row("same", 1, json!({"integer": i64::MAX}));
    if let Element::Node { properties, .. } = &mut first {
        properties.insert(
            "time",
            ElementValue::LocalDateTime(DateTime::<Utc>::UNIX_EPOCH.naive_utc()),
        );
    }
    let second = node("other-source", "same", "Input", 1, json!({"integer": 7}));
    emit(
        &mut view,
        &[
            SourceChange::Insert {
                element: first.clone(),
            },
            SourceChange::Insert {
                element: second.clone(),
            },
        ],
    )
    .await?;
    emit(
        &mut view,
        &[SourceChange::update(row("same", 2, json!({"added": null})))],
    )
    .await?;
    first.update_effective_time(2);
    if let Element::Node { properties, .. } = &mut first {
        properties.insert("added", ElementValue::Null);
    }
    assert_rows(&view, &[first, second.clone()]).await?;
    emit(&mut view, &[deletion("same", 3)]).await?;
    assert_rows(&view, &[second]).await
}

struct ChangingMapper {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Mapper for ChangingMapper {
    async fn map(&self, element: Element) -> Result<Vec<Element>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if element.get_property("filtered") == &ElementValue::Bool(true) {
            return Ok(Vec::new());
        }
        Ok(["a", "b"]
            .into_iter()
            .map(|suffix| {
                projected(
                    &format!("actual-{call}-{suffix}"),
                    element.get_effective_from(),
                    json!({"call": call}),
                )
            })
            .collect())
    }
}

#[tokio::test]
async fn nondeterministic_mapping_is_never_rerun_to_recover_historical_outputs() -> Result<()> {
    let mut view = start(identity_configuration()).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    view.mapper = Box::new(ChangingMapper {
        calls: calls.clone(),
    });
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: row("input", 1, json!({})),
        }],
    )
    .await?;
    let changed = emit(
        &mut view,
        &[SourceChange::update(row("input", 2, json!({"value": 1})))],
    )
    .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(changed.len(), 4);
    assert_eq!(changed[0].get_reference().element_id.as_ref(), "actual-1-a");
    assert_eq!(changed[1].get_reference().element_id.as_ref(), "actual-1-b");
    assert_rows(
        &view,
        &[
            projected("actual-2-a", 2, json!({"call": 2})),
            projected("actual-2-b", 2, json!({"call": 2})),
        ],
    )
    .await?;
    let filtered = emit(
        &mut view,
        &[SourceChange::update(row(
            "input",
            3,
            json!({"filtered": true}),
        ))],
    )
    .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(filtered.len(), 2);
    assert_eq!(
        filtered[0].get_reference().element_id.as_ref(),
        "actual-2-a"
    );
    assert_eq!(
        filtered[1].get_reference().element_id.as_ref(),
        "actual-2-b"
    );
    assert!(emit(&mut view, &[deletion("input", 4)]).await?.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: row("input", 5, json!({})),
        }],
    )
    .await?;
    let deleted = emit(&mut view, &[deletion("input", 6)]).await?;
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(deleted.len(), 2);
    assert_eq!(deleted[0].get_reference().element_id.as_ref(), "actual-4-a");
    assert_eq!(deleted[1].get_reference().element_id.as_ref(), "actual-4-b");
    assert_rows(&view, &[]).await
}

struct PausedMapper {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Mapper for PausedMapper {
    async fn map(&self, element: Element) -> Result<Vec<Element>> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(vec![element])
    }
}

#[tokio::test]
async fn cancelled_staging_keeps_the_previous_snapshot_and_input_base() -> Result<()> {
    let mut view = start(identity_configuration()).await?;
    let initial = row("input", 1, json!({"value": "original", "keep": 7}));
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: initial.clone(),
        }],
    )
    .await?;
    let snapshot_reader = reader(&view)?;
    let before = snapshot_reader.snapshot().await?;
    let entered = Arc::new(Notify::new());
    view.mapper = Box::new(PausedMapper {
        entered: entered.clone(),
        release: Arc::new(Notify::new()),
    });
    let mut pending = Box::pin(view.transform(input(
        &[SourceChange::update(row(
            "input",
            2,
            json!({"value": "cancelled"}),
        ))],
        2,
    )?));
    tokio::select! {
        result = &mut pending => anyhow::bail!("expected a suspended mapping, got {result:?}"),
        () = entered.notified() => {}
    }
    let during = snapshot_reader.snapshot().await?;
    assert_eq!(before.cursor(), during.cursor());
    assert_eq!(rows(&before)?, rows(&during)?);
    drop(pending);
    assert_eq!(view.sequence, 1);
    view.mapper = Box::new(Pipeline::new(&[], &[])?);
    assert_rows(&view, &[initial]).await?;
    emit(
        &mut view,
        &[SourceChange::update(row("input", 3, json!({"keep": 8})))],
    )
    .await?;
    assert_rows(
        &view,
        &[row("input", 3, json!({"value": "original", "keep": 8}))],
    )
    .await?;
    assert_eq!(
        rows(&before)?[&ElementReference::new("source", "input")].get_property("keep"),
        &ElementValue::Integer(7)
    );
    Ok(())
}

#[tokio::test]
async fn batch_maps_final_images_once_and_empty_changes_still_advance_progress() -> Result<()> {
    let mut view = start(identity_configuration()).await?;
    let calls = Arc::new(AtomicUsize::new(0));
    view.mapper = Box::new(ChangingMapper {
        calls: calls.clone(),
    });
    emit(
        &mut view,
        &[
            SourceChange::Insert {
                element: row("input", 1, json!({"value": 1})),
            },
            SourceChange::update(row("input", 2, json!({"value": 2}))),
            SourceChange::Insert {
                element: row("ephemeral", 2, json!({})),
            },
            deletion("ephemeral", 3),
        ],
    )
    .await?;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(view.sequence, 1);
    assert!(emit(&mut view, &[]).await?.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(view.sequence, 2);
    assert_rows(
        &view,
        &[
            projected("actual-1-a", 2, json!({"call": 1})),
            projected("actual-1-b", 2, json!({"call": 1})),
        ],
    )
    .await
}

#[tokio::test]
async fn lifecycle_retains_same_instance_state_but_never_claims_reconstruction_recovery(
) -> Result<()> {
    let mut view = create(identity_configuration())?;
    let snapshot_reader = reader(&view)?;
    assert!(matches!(
        snapshot_reader.snapshot().await,
        Err(OutputReadError::NotReady(_))
    ));
    assert!(view.transform(input(&[], 1)?).await.is_err());
    view.start().await?;
    let expected = row("input", 1, json!({"value": 7}));
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: expected.clone(),
        }],
    )
    .await?;
    let before = snapshot_reader.snapshot().await?;
    assert_eq!(before.cursor().producer().construction_scope(), "instance");
    assert_eq!(before.cursor().producer().graph_id(), "graph");
    assert!(!before.cursor().producer().persistent());
    assert!(view.deprovision().await.is_err());
    view.stop().await?;
    assert!(matches!(
        snapshot_reader.snapshot().await,
        Err(OutputReadError::NotReady(_))
    ));
    assert!(view.transform(input(&[], 2)?).await.is_err());
    view.start().await?;
    assert_eq!(before.cursor(), snapshot_reader.snapshot().await?.cursor());
    assert_rows(&view, &[expected]).await?;
    let fresh = start(identity_configuration()).await?;
    assert_rows(&fresh, &[]).await?;
    assert_ne!(
        before.cursor().epoch(),
        reader(&fresh)?.snapshot().await?.cursor().epoch()
    );
    view.stop().await?;
    view.deprovision().await?;
    view.deprovision().await?;
    assert!(view.start().await.is_err());
    assert!(matches!(
        snapshot_reader.snapshot().await,
        Err(OutputReadError::Retired)
    ));
    let fresh_reader = reader(&fresh)?;
    drop(fresh);
    assert!(matches!(
        fresh_reader.snapshot().await,
        Err(OutputReadError::Retired)
    ));
    assert_eq!(rows(&before)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn provenance_and_configuration_remain_unchanged_while_output_owns_volatile_progress(
) -> Result<()> {
    let configuration = key_configuration();
    let mut view = start(configuration.clone()).await?;
    let mut incoming = input(
        &[SourceChange::Insert {
            element: row("input", 1, json!({"key": "projected", "value": 3})),
        }],
        90,
    )?;
    incoming.envelope.append_annotation(ContextEntry::try_new(
        ComponentId::try_new("upstream")?,
        "test.provenance",
        ContextValue::String("original".into()),
    )?)?;
    let input_id = incoming.envelope.id().clone();
    let outputs = view.transform(incoming).await?;
    assert_eq!(outputs.len(), 1);
    let envelope = &outputs[0].envelope;
    assert_eq!(envelope.system().stream().as_str(), "view/out");
    assert_eq!(envelope.system().sequence(), 1);
    assert_eq!(
        envelope.lineage().context("lineage missing")?.envelope_id(),
        &input_id
    );
    assert!(envelope
        .annotations()
        .entries()
        .any(|entry| entry.key() == "test.provenance"));
    assert_eq!(view.configuration()?, configuration);
    Ok(())
}

#[tokio::test]
async fn unknown_ports_controls_and_excessive_lineage_do_not_publish_state() -> Result<()> {
    let mut view = start(identity_configuration()).await?;
    let mut unknown = input(&[], 1)?;
    unknown.port = PortId::try_new("unknown")?;
    assert!(view.transform(unknown).await.is_err());
    let control = GraphChangeCodec::encode_futures_due(
        StreamId::try_new("source/out")?,
        1,
        DateTime::<Utc>::UNIX_EPOCH,
    )?;
    assert!(view
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: control
        })
        .await
        .is_err());
    let changes = vec![SourceChange::Insert {
        element: row("input", 1, json!({})),
    }];
    let mut deep = input(&changes, 1)?;
    for sequence in 1..=drasi_computation_contracts::MAX_LINEAGE_DEPTH {
        deep.envelope = GraphChangeCodec::derive_changes(
            &deep.envelope,
            &changes,
            StreamId::try_new("source/out")?,
            sequence as u64,
        )?;
    }
    assert!(view.transform(deep).await.is_err());
    assert_eq!(view.sequence, 0);
    assert_rows(&view, &[]).await
}

#[tokio::test]
async fn metadata_and_factory_advertise_only_implemented_capabilities() -> Result<()> {
    let metadata = ViewFactory.metadata();
    assert_eq!(
        metadata.capabilities.output_progress,
        Some(OutputProgress::Origin)
    );
    assert_eq!(
        metadata.capabilities.output_snapshots,
        vec![PortId::try_new("out")?]
    );
    assert!(metadata.capabilities.deprovision);
    assert!(!metadata.capabilities.transactional);
    assert!(!metadata.capabilities.durable_output);
    assert!(!metadata.capabilities.indexed_processing);
    assert!(metadata.capabilities.output_replays.is_empty());
    assert!(metadata.capabilities.output_follows.is_empty());
    assert!(metadata.capabilities.snapshot_inputs.is_empty());
    assert!(metadata.capabilities.quiescent_bootstrap.is_none());
    assert!(metadata.capabilities.durable_bootstrap.is_none());
    assert!(!metadata.capabilities.wakeups);
    let request = CreateRequest {
        id: ComponentId::try_new("factory-view")?,
        implementation: metadata.implementation.clone(),
        configuration_version: 1,
        configuration: identity_configuration(),
        scope: None,
        inputs: Vec::new(),
        activation_credentials_bound: false,
    };
    let created = ViewFactory.create(&request, ControlSender::default())?;
    let Component::Transformer(mut transformer) = created.component else {
        anyhow::bail!("view factory returned a different role")
    };
    assert!(transformer.supports_deprovision());
    assert_eq!(transformer.configuration()?, request.configuration);
    let readers = transformer.output_readers();
    readers.validate(transformer.descriptor())?;
    assert!(readers.ports()[&PortId::try_new("out")?].replay().is_none());
    assert!(transformer.resume_delivery().is_err());
    transformer.start().await?;
    transformer.stop().await?;
    transformer.deprovision().await?;
    let exported = ExportedPlugin::new(crate::plugin()?)?;
    assert!(!exported.metadata().is_null());
    Ok(())
}

#[test]
fn invalid_or_event_oriented_mapping_configuration_fails_at_construction() -> Result<()> {
    for invalid in [
        json!({"stream": "", "middleware": [], "pipeline": []}),
        json!({"stream": "view/out", "middleware": [], "pipeline": [], "durable": true}),
        json!({"stream": "view/out", "middleware": [], "pipeline": ["missing"]}),
        json!({"stream": "view/out", "middleware": [{"name": "x", "kind": "jq", "config": {}}], "pipeline": ["x"]}),
        mapped_configuration(json!([{"op": "Update"}])),
        mapped_configuration(json!([{"op": "Delete"}])),
    ] {
        assert!(
            create(invalid.clone()).is_err(),
            "accepted invalid configuration: {invalid}"
        );
    }
    let mut update_rules = key_configuration();
    update_rules["middleware"][0]["config"]["Input"]["update"] = json!([]);
    assert!(create(update_rules).is_err());
    let mut duplicate = key_configuration();
    let repeated = duplicate["middleware"][0].clone();
    duplicate["middleware"]
        .as_array_mut()
        .context("middleware array")?
        .push(repeated);
    assert!(create(duplicate).is_err());
    Ok(())
}

#[tokio::test]
async fn existing_map_remains_a_stateless_event_mapper() -> Result<()> {
    let rule = json!({
        "condition": "$[?(@.active == true)]",
        "id": "$.key", "label": "Projected", "properties": {"value": "$.value"}
    });
    let config: SourceMiddlewareConfig = serde_json::from_value(json!({
        "name": "existing", "kind": "map",
        "config": {"Input": {"insert": [rule.clone()], "update": [rule]}}
    }))?;
    let mapper = drasi_middleware::map::MapFactory::new().build(&config)?;
    assert_eq!(
        mapper
            .apply(SourceChange::Insert {
                element: row(
                    "input",
                    1,
                    json!({"active": true, "key": "old", "value": 1})
                ),
            })
            .await?
            .len(),
        1
    );
    assert!(mapper
        .apply(SourceChange::update(row(
            "input",
            2,
            json!({
                "active": false, "key": "old"
            })
        )))
        .await?
        .is_empty());
    assert!(mapper
        .apply(SourceChange::Update {
            element: row("input", 3, json!({"active": true, "key": "new"})),
            removed_properties: ["value".into()].into(),
        })
        .await?
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn canonical_output_comparison_preserves_float_bits_instead_of_element_equality() -> Result<()>
{
    use drasi_core_types::data_value::DataValue;

    let mut view = start(identity_configuration()).await?;
    let float_image = |bits| ElementImage::Node {
        metadata: row("float", 1, json!({})).get_metadata().clone(),
        properties: [("value".to_owned(), DataValue::Float(bits))].into(),
    };
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: Element::try_from(float_image(0.0_f64.to_bits()))?,
        }],
    )
    .await?;
    for bits in [
        (-0.0_f64).to_bits(),
        0x7ff8_0000_0000_0001,
        0x7ff8_0000_0000_0002,
    ] {
        let changes = emit(
            &mut view,
            &[SourceChange::update(Element::try_from(float_image(bits))?)],
        )
        .await?;
        assert_eq!(changes.len(), 1);
        let SourceChange::Replace { element } = &changes[0] else {
            anyhow::bail!("changed float bits must replace the output image")
        };
        assert_eq!(ElementImage::try_from(element)?, float_image(bits));
        let snapshot = reader(&view)?.snapshot().await?;
        let record = snapshot
            .records()
            .values()
            .next()
            .context("missing output")?;
        assert_eq!(GraphChangeCodec::decode_element(record)?, float_image(bits));
    }
    assert!(emit(
        &mut view,
        &[SourceChange::update(Element::try_from(float_image(
            0x7ff8_0000_0000_0002
        ))?,)]
    )
    .await?
    .is_empty());
    Ok(())
}

#[tokio::test]
async fn every_pipeline_step_projects_a_complete_image_using_insert_rules() -> Result<()> {
    let mut view = start(json!({
        "stream": "view/out",
        "middleware": [
            {"name": "first", "kind": "map", "config": {"Input": {"insert": [{
                "op": "Replace", "id": "$.key", "label": "Intermediate",
                "properties": {"value": "$.value", "key": "$.key"}
            }]}}},
            {"name": "second", "kind": "map", "config": {"Intermediate": {"insert": [{
                "label": "Result", "properties": {"value": "$.value"}
            }]}}},
            {"name": "labels", "kind": "relabel", "config": {"labelMappings": {"Result": "Projected"}}}
        ],
        "pipeline": ["first", "second", "labels"]
    })).await?;
    assert_eq!(
        emit(
            &mut view,
            &[SourceChange::Insert {
                element: row("input", 1, json!({"key": "output", "value": 1})),
            }]
        )
        .await?,
        vec![SourceChange::Insert {
            element: projected("output", 1, json!({"value": 1}))
        }]
    );
    assert_eq!(
        emit(
            &mut view,
            &[SourceChange::update(row("input", 2, json!({"value": 2})))]
        )
        .await?,
        vec![SourceChange::Replace {
            element: projected("output", 2, json!({"value": 2}))
        }]
    );
    assert_rows(&view, &[projected("output", 2, json!({"value": 2}))]).await
}

struct RejectingMapper;

#[async_trait]
impl Mapper for RejectingMapper {
    async fn map(&self, element: Element) -> Result<Vec<Element>> {
        anyhow::ensure!(
            element.get_reference().element_id.as_ref() != "reject",
            "injected mapping failure"
        );
        Ok(vec![element])
    }
}

#[tokio::test]
async fn mapping_failure_rolls_back_all_staged_inputs_and_output_associations() -> Result<()> {
    let mut view = start(key_configuration()).await?;
    emit(
        &mut view,
        &[SourceChange::Insert {
            element: row("original", 1, json!({"key": "recorded", "value": 1})),
        }],
    )
    .await?;
    view.mapper = Box::new(RejectingMapper);
    let error = view
        .transform(input(
            &[
                SourceChange::update(row("original", 2, json!({"key": "wrong", "value": 2}))),
                SourceChange::Insert {
                    element: row("reject", 2, json!({})),
                },
            ],
            2,
        )?)
        .await
        .expect_err("mapping failure must abort the envelope");
    assert!(format!("{error:#}").contains("injected mapping failure"));
    assert_eq!(view.sequence, 1);
    assert_rows(&view, &[projected("recorded", 1, json!({"value": 1}))]).await?;
    assert_eq!(
        emit(&mut view, &[deletion("original", 3)]).await?,
        vec![SourceChange::Delete {
            metadata: projected("recorded", 3, json!({})).get_metadata().clone(),
        }]
    );
    assert!(view
        .transform(input(
            &[SourceChange::update(row("reject", 3, json!({})))],
            3
        )?)
        .await
        .is_err());
    assert_rows(&view, &[]).await
}
