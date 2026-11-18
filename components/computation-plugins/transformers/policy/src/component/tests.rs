// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::{digest::canonical, PolicyFactory};
use anyhow::Context;
use drasi_computation_contracts::{
    ComponentId, ComponentUpdateData, OutputReadError, SnapshotProvider, StreamId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::{DisposeReason, StateIntent, StopMode},
    testing::{contexts, update_request},
    ExportedPlugin, Factory,
};
use drasi_core_types::models::{ElementMetadata, ElementPropertyMap, ElementValue};
use serde_json::{json, Value};

const ID: &str = "8f0d5d3c-4a8e-4f7e-9d43-3b0f0e3f2a11";

const PLACEMENT: &str = r#"package gpu.placement
import rego.v1

region_allowed if input.cluster.region in input.policy.allowed_regions
region_allowed if input.policy.allowed_regions == ["*"]

reasons contains "region-not-permitted" if not region_allowed
reasons contains "purpose-not-permitted" if not input.workload.purpose in input.policy.allowed_purposes
reasons contains "classification-not-permitted" if not input.data_profile.classification in input.policy.allowed_classifications
reasons contains "customer-mismatch" if input.data_profile.customer_id != input.policy.customer_id

default allow := false
allow if count(reasons) == 0

decision := {"allow": allow, "reasons": sort(reasons)}
"#;

/// The demo's context validation and Allow/Deny/Unknown mapping, as a
/// second module over the unchanged placement module.
const ELIGIBILITY: &str = r#"package gpu.eligibility
import rego.v1

valid_context if {
    input.data_profile.classification in data.valid_classifications
    input.data_profile.revision > 0
    input.data_profile.authority_ref != ""
    input.workload.purpose in data.valid_purposes
}

default result := {"authorization": "Unknown", "reasons": ["invalid-policy-context"]}

result := {"authorization": "Allow", "reasons": []} if {
    valid_context
    data.gpu.placement.decision.allow
}

result := {"authorization": "Deny", "reasons": data.gpu.placement.decision.reasons} if {
    valid_context
    not data.gpu.placement.decision.allow
}
"#;

fn base(extra: Value) -> Value {
    let mut value = json!({
        "stream": "policy/out",
        "modules": [{"name": "t.rego", "source": "package t\nimport rego.v1\nbig := input.n > data.limit\n"}],
        "data": {"limit": 10},
        "query": "data.t.big",
        "labels": ["Item"],
        "output_properties": {"big": "$decision", "n": "$input.n"},
        "input": {"n": "$properties.n"},
    });
    let (Value::Object(base), Value::Object(extra)) = (&mut value, extra) else {
        unreachable!()
    };
    base.extend(extra);
    value
}

fn create(configuration: Value) -> Result<PolicyTransformer> {
    PolicyTransformer::new(
        PolicyFactory
            .metadata()
            .descriptor(ComponentId::try_new(ID)?)?,
        configuration,
        None,
        &[],
        None,
    )
}

async fn start(configuration: Value) -> Result<PolicyTransformer> {
    let mut policy = create(configuration)?;
    policy.initialize(&contexts::initialize()).await?;
    policy.start(&mut contexts::start()).await?;
    Ok(policy)
}

fn node(id: &str, label: &str, time: u64, properties: Value) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: vec![Arc::from(label)].into(),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

fn item(id: &str, time: u64, n: i64) -> Element {
    node(id, "Item", time, json!({"n": n}))
}

fn insert(element: Element) -> SourceChange {
    SourceChange::Insert { element }
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
            None,
        )?,
    })
}

fn decode(
    mut outputs: Vec<OutputEnvelope>,
    policy: &PolicyTransformer,
) -> Result<Vec<SourceChange>> {
    assert_eq!(outputs.len(), 1);
    let output = outputs.pop().context("missing output")?;
    assert_eq!(output.port.as_str(), "out");
    let progress = GraphProducerProgress::from_envelope(&output.envelope)?
        .context("missing policy progress")?;
    assert_eq!(progress.sequence(), output.envelope.system().sequence());
    assert_eq!(progress.sequence(), policy.sequence);
    assert_eq!(progress.identity().component_id().as_str(), ID);
    Ok(GraphChangeCodec::decode_changes(&output.envelope)?)
}

async fn emit(
    policy: &mut PolicyTransformer,
    changes: &[SourceChange],
) -> Result<Vec<SourceChange>> {
    let envelope = input(changes, policy.sequence + 1)?;
    let outputs = policy.transform(envelope).await?;
    decode(outputs, policy)
}

fn property(element: &Element, name: &str) -> Value {
    Value::from(element.get_property(name))
}

fn element(change: &SourceChange) -> &Element {
    match change {
        SourceChange::Insert { element } | SourceChange::Replace { element } => element,
        other => panic!("expected an element change, got {other:?}"),
    }
}

fn reader(policy: &PolicyTransformer) -> Result<Arc<dyn SnapshotProvider>> {
    policy
        .output_readers()
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|readers| readers.snapshot())
        .cloned()
        .context("missing snapshot reader")
}

async fn snapshot_rows(policy: &PolicyTransformer) -> Result<HashMap<ElementReference, Element>> {
    let snapshot = reader(policy)?.snapshot().await?;
    assert_eq!(snapshot.cursor().sequence(), policy.sequence);
    snapshot
        .records()
        .values()
        .map(|record| {
            let element = Element::try_from(GraphChangeCodec::decode_element(record)?)?;
            Ok((element.get_reference().clone(), element))
        })
        .collect()
}

async fn apply(
    policy: &mut PolicyTransformer,
    current: Value,
    proposed: Value,
) -> Result<(), ApplyError> {
    let request = update_request(
        &PolicyFactory,
        &ComponentId::try_new(ID).expect("id"),
        current,
        proposed,
    );
    let assessment = NativeManagement::assess(policy, &request)?;
    let update = ApplyUpdateRequest {
        operation_id: "op".into(),
        request,
        plan: assessment.plan().context("plan")?,
        fingerprint: None,
    };
    policy
        .management()
        .context("management")?
        .apply(&contexts::apply(UpdateBoundary::Pause), &update)
        .await
}

async fn drain(policy: &mut PolicyTransformer) -> Result<Vec<SourceChange>> {
    let mut changes = Vec::new();
    while policy.has_pending_emissions() {
        let outputs = policy.continue_transform().await?;
        changes.extend(decode(outputs, policy)?);
    }
    Ok(changes)
}

#[test]
fn plugin_exports_and_constructs_through_the_factory() -> Result<()> {
    ExportedPlugin::new(crate::plugin()?)?;
    let metadata = PolicyFactory.metadata();
    assert_eq!(&*metadata.implementation.name, "drasi.standard/policy");
    assert!(metadata.capabilities.management && metadata.capabilities.continuations);
    assert!(metadata.capabilities.startup_continuation && metadata.capabilities.binds_inputs);
    assert!(PolicyFactory.supports_update_planning());
    let request = drasi_computation_plugin_sdk::testing::create_request(
        &PolicyFactory,
        &ComponentId::try_new(ID)?,
        base(json!({})),
    );
    PolicyFactory.create(
        &request,
        drasi_computation_plugin_sdk::ControlSender::default(),
    )?;
    let mut invalid = request.clone();
    invalid.configuration = base(json!({"query": "data.t.missing"}));
    assert!(PolicyFactory
        .create(
            &invalid,
            drasi_computation_plugin_sdk::ControlSender::default()
        )
        .is_err());
    Ok(())
}

#[test]
fn configuration_is_validated_up_front_and_normalized() -> Result<()> {
    for invalid in [
        base(json!({"unknown": 1})),
        base(json!({"modules": []})),
        base(
            json!({"modules": [{"name": "a", "source": "package a"}, {"name": "a", "source": "package b"}]}),
        ),
        base(json!({"modules": [{"name": "t.rego", "source": "package t\nbig := {"}]})),
        base(json!({"modules": [{"name": "t.rego", "source": "package t\nbig { true }"}]})),
        base(json!({"query": "data.t.missing"})),
        base(json!({"query": "t.big"})),
        base(json!({"rego_version": "v2"})),
        base(json!({"on_error": "ignore"})),
        base(json!({"timeout_ms": 0})),
        base(json!({"timeout_ms": 60001})),
        base(json!({"input": "$nope"})),
        base(json!({"output_properties": {"x": "$"}})),
        base(json!({"output_properties": {"x": "$properties.n"}})),
        base(json!({"output_properties": {"policy_status": "$status"}})),
        base(json!({"output_label": ""})),
        base(json!({"labels": [""]})),
        base(json!({"max_policy_bytes": 8})),
        base(json!({"max_data_bytes": 4})),
        base(json!({"modules": "package t"})),
    ] {
        assert!(create(invalid.clone()).is_err(), "{invalid}");
    }
    let rego_v0 = base(json!({
        "rego_version": "v0",
        "modules": [{"name": "t.rego", "source": "package t\nbig { input.n > data.limit }"}],
    }));
    create(rego_v0)?;

    let policy = create(json!({
        "stream": "policy/out",
        "modules": [{"name": "t.rego", "source": PLACEMENT}],
        "query": "data.gpu.placement.decision",
    }))?;
    let normalized = policy.configuration()?;
    assert_eq!(normalized["input"], json!("$properties"));
    assert_eq!(normalized["output_label"], json!("PolicyDecision"));
    assert_eq!(
        normalized["output_properties"],
        json!({"decision": "$decision"})
    );
    assert_eq!(normalized["on_error"], json!("emit"));
    assert_eq!(normalized["rego_version"], json!("v1"));
    assert_eq!(normalized["timeout_ms"], json!(100));
    assert_eq!(create(normalized.clone())?.configuration()?, normalized);
    Ok(())
}

#[tokio::test]
async fn decisions_follow_inserts_patches_replaces_and_deletes() -> Result<()> {
    let mut policy = start(base(json!({}))).await?;
    let changes = emit(
        &mut policy,
        &[insert(item("a", 1, 5)), insert(item("b", 1, 20))],
    )
    .await?;
    assert_eq!(changes.len(), 2);
    let a = element(&changes[0]);
    assert!(matches!(changes[0], SourceChange::Insert { .. }));
    assert_eq!(a.get_reference(), &ElementReference::new("source", "a"));
    assert_eq!(&*a.get_metadata().labels, &[Arc::from("PolicyDecision")]);
    assert_eq!(a.get_effective_from(), 1);
    assert_eq!(property(a, "big"), json!(false));
    assert_eq!(property(a, "n"), json!(5));
    assert_eq!(property(a, "policy_status"), json!("decided"));
    assert_eq!(property(a, "policy_error"), Value::Null);
    let fingerprint = crate::digest::sha256_hex(canonical(&json!({"n": 5})).as_bytes());
    assert_eq!(property(a, "policy_input_fingerprint"), json!(fingerprint));
    let hash = property(a, "policy_hash");
    assert!(hash
        .as_str()
        .is_some_and(|hash| hash.starts_with("sha256:")));
    assert_eq!(property(element(&changes[1]), "big"), json!(true));
    assert_eq!(property(element(&changes[1]), "policy_hash"), hash);

    // An unrelated property change leaves the decision unchanged: nothing is emitted.
    let unchanged = emit(
        &mut policy,
        &[SourceChange::update(node(
            "a",
            "Item",
            2,
            json!({"other": 1}),
        ))],
    )
    .await?;
    assert!(unchanged.is_empty());
    assert_eq!(
        policy.inputs[&ElementReference::new("source", "a")].get_property("other"),
        &ElementValue::Integer(1)
    );

    // A patch that changes the decision replaces it.
    let changes = emit(
        &mut policy,
        &[SourceChange::update(node("a", "Item", 3, json!({"n": 11})))],
    )
    .await?;
    assert!(matches!(changes[..], [SourceChange::Replace { .. }]));
    assert_eq!(property(element(&changes[0]), "big"), json!(true));
    assert_eq!(element(&changes[0]).get_effective_from(), 3);

    // Removing the input property makes the input selector omit `n`, so the rule is undefined.
    let changes = emit(
        &mut policy,
        &[SourceChange::update_with_removals(
            node("a", "Item", 4, json!({})),
            ["n".into()].into(),
        )?],
    )
    .await?;
    let undefined = element(&changes[0]);
    assert_eq!(property(undefined, "policy_status"), json!("undefined"));
    assert_eq!(property(undefined, "big"), Value::Null);
    assert_eq!(property(undefined, "n"), Value::Null);

    // Relabelling out of the filter retracts; deleting an unknown element is ignored.
    let changes = emit(
        &mut policy,
        &[
            SourceChange::Replace {
                element: node("a", "Other", 5, json!({"n": 1})),
            },
            deletion("unknown", 5),
        ],
    )
    .await?;
    let SourceChange::Delete { metadata } = &changes[..].first().context("delete")? else {
        panic!("expected a delete, got {changes:?}");
    };
    assert_eq!(changes.len(), 1);
    assert_eq!(metadata.reference, ElementReference::new("source", "a"));
    assert_eq!(metadata.effective_from, 5);
    assert!(!policy
        .inputs
        .contains_key(&ElementReference::new("source", "a")));

    // Elements outside the filter are never retained or evaluated.
    assert!(emit(
        &mut policy,
        &[insert(node("c", "Other", 6, json!({"n": 50})))]
    )
    .await?
    .is_empty());
    assert!(emit(
        &mut policy,
        &[SourceChange::update(node("c", "Other", 7, json!({"n": 1})))]
    )
    .await?
    .is_empty());

    // Several changes to one element in a batch net to one change.
    let changes = emit(
        &mut policy,
        &[
            insert(item("d", 8, 1)),
            SourceChange::update(node("d", "Item", 8, json!({"n": 99}))),
            deletion("b", 8),
        ],
    )
    .await?;
    assert_eq!(changes.len(), 2);
    assert_eq!(property(element(&changes[0]), "big"), json!(true));
    assert!(
        matches!(&changes[1], SourceChange::Delete { metadata } if metadata.reference.element_id.as_ref() == "b")
    );
    let rows = snapshot_rows(&policy).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        property(&rows[&ElementReference::new("source", "d")], "big"),
        json!(true)
    );
    Ok(())
}

#[tokio::test]
async fn element_documents_copy_properties_and_relations() -> Result<()> {
    let mut policy = start(base(json!({
        "labels": [],
        "modules": [{"name": "t.rego", "source": "package t\nimport rego.v1\nbig := input\n"}],
        "input": "$",
        "output_label": "Echo",
        "output_properties": {"kind": "$decision.kind", "out": "$element.out_node.id", "status": "$status", "literal": "$$decision"},
        "copy_input_properties": true,
        "provenance": false,
    })))
    .await?;
    let relation = Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "r"),
            labels: vec![Arc::from("LINKS")].into(),
            effective_from: 1,
        },
        in_node: ElementReference::new("source", "x"),
        out_node: ElementReference::new("source", "y"),
        properties: ElementPropertyMap::from(json!({"w": 2, "policy_status": "forged"})),
    };
    let changes = emit(&mut policy, &[insert(relation)]).await?;
    let decision = element(&changes[0]);
    assert!(matches!(decision, Element::Node { .. }));
    assert_eq!(property(decision, "kind"), json!("relation"));
    assert_eq!(property(decision, "out"), json!("y"));
    assert_eq!(property(decision, "status"), json!("decided"));
    assert_eq!(property(decision, "literal"), json!("$decision"));
    assert_eq!(property(decision, "w"), json!(2));
    assert_eq!(property(decision, "policy_status"), json!("decided"));
    assert_eq!(property(decision, "policy_hash"), Value::Null);
    Ok(())
}

#[tokio::test]
async fn evaluation_errors_emit_or_fail_as_configured() -> Result<()> {
    let failing = json!({
        "modules": [{"name": "t.rego", "source": "package t\nimport rego.v1\nbig := 10 / input.n\n"}],
        "strict_builtin_errors": true,
        "output_properties": {"big": "$decision", "error": "$error"},
    });
    let mut emitting = start(base(failing.clone())).await?;
    let changes = emit(
        &mut emitting,
        &[insert(item("a", 1, 0)), insert(item("b", 1, 5))],
    )
    .await?;
    let error = element(&changes[0]);
    assert_eq!(property(error, "policy_status"), json!("error"));
    assert!(property(error, "policy_error")
        .as_str()
        .is_some_and(|m| !m.is_empty()));
    assert_eq!(property(error, "error"), property(error, "policy_error"));
    assert_eq!(property(element(&changes[1]), "big"), json!(2));

    let mut config = failing;
    config["on_error"] = json!("fail");
    let mut strict = start(base(config)).await?;
    assert!(strict
        .transform(input(
            &[insert(item("b", 1, 5)), insert(item("a", 1, 0))],
            1
        )?)
        .await
        .is_err());
    assert!(strict.inputs.is_empty() && strict.published.is_empty() && strict.sequence == 0);
    let changes = emit(&mut strict, &[insert(item("b", 1, 5))]).await?;
    assert_eq!(property(element(&changes[0]), "big"), json!(2));
    Ok(())
}

#[tokio::test]
async fn limits_bound_inputs_decisions_time_and_retention() -> Result<()> {
    let mut limited = start(base(json!({
        "modules": [{"name": "t.rego", "source": "package t\nimport rego.v1\nbig := input\n"}],
        "input": "$properties",
        "max_input_bytes": 16,
        "max_retained_inputs": 2,
    })))
    .await?;
    let changes = emit(
        &mut limited,
        &[insert(node(
            "a",
            "Item",
            1,
            json!({"n": "a long string value"}),
        ))],
    )
    .await?;
    assert!(property(element(&changes[0]), "policy_error")
        .as_str()
        .is_some_and(|m| m.contains("max_input_bytes")));
    emit(&mut limited, &[insert(item("b", 1, 1))]).await?;
    assert!(limited
        .transform(input(&[insert(item("c", 1, 1))], 3)?)
        .await
        .is_err());
    // Replacing a retained element does not grow retention.
    emit(
        &mut limited,
        &[
            insert(item("b", 2, 2)),
            deletion("a", 2),
            insert(item("c", 2, 1)),
        ],
    )
    .await?;

    let mut slow = start(base(json!({
        "modules": [{"name": "t.rego", "source": "package t\nimport rego.v1\nbig := count([x | some x in numbers.range(1, 100000000)])\n"}],
        "timeout_ms": 5,
        "timeout_check_interval": 1,
    })))
    .await?;
    let changes = emit(&mut slow, &[insert(item("a", 1, 1))]).await?;
    assert_eq!(
        property(element(&changes[0]), "policy_status"),
        json!("error")
    );
    Ok(())
}

#[tokio::test]
async fn lifecycle_gates_input_and_erase_retires_state() -> Result<()> {
    let mut policy = create(base(json!({})))?;
    assert!(policy
        .transform(input(&[insert(item("a", 1, 1))], 1)?)
        .await
        .is_err());
    assert!(matches!(
        reader(&policy)?.snapshot().await,
        Err(OutputReadError::NotReady(_))
    ));
    policy.initialize(&contexts::initialize()).await?;
    policy.start(&mut contexts::start()).await?;
    assert!(policy.start(&mut contexts::start()).await.is_err());
    assert!(policy.transform(input(&[], 1)?).await.is_ok());
    let mut control = input(&[], 2)?;
    control.port = PortId::try_new("other")?;
    assert!(policy.transform(control).await.is_err());
    emit(&mut policy, &[insert(item("a", 1, 50))]).await?;
    policy.pause(&contexts::pause()).await?;
    assert!(policy
        .transform(input(&[insert(item("b", 1, 1))], 3)?)
        .await
        .is_err());
    policy.resume(&contexts::resume()).await?;
    emit(&mut policy, &[insert(item("b", 1, 1))]).await?;
    assert!(policy
        .dispose(&contexts::dispose(
            DisposeReason::Removal,
            StateIntent::Erase,
            1
        )?)
        .await
        .is_err());
    policy.stop(&contexts::stop(StopMode::Drain)).await?;
    // Stop preserves state for a later start.
    policy.start(&mut contexts::start()).await?;
    assert_eq!(snapshot_rows(&policy).await?.len(), 2);
    policy.stop(&contexts::stop(StopMode::Abort)).await?;
    policy
        .dispose(&contexts::dispose(
            DisposeReason::Removal,
            StateIntent::Erase,
            1,
        )?)
        .await?;
    policy
        .dispose(&contexts::dispose(
            DisposeReason::Removal,
            StateIntent::Erase,
            2,
        )?)
        .await?;
    assert!(policy.inputs.is_empty() && policy.published.is_empty());
    assert!(matches!(
        reader(&policy)?.snapshot().await,
        Err(OutputReadError::Retired)
    ));
    assert!(policy.start(&mut contexts::start()).await.is_err());
    Ok(())
}

#[test]
fn assessment_classifies_changes() -> Result<()> {
    let id = ComponentId::try_new(ID)?;
    let assess = |proposed: Value| {
        PolicyFactory.assess(&update_request(
            &PolicyFactory,
            &id,
            base(json!({})),
            proposed,
        ))
    };
    let boundary = |assessment: Assessment| assessment.plan().map(|plan| plan.mode);
    let pause = boundary(Assessment::supported(
        UpdateBoundary::Pause,
        ComponentUpdateData::Preserve,
    ));
    let replace = boundary(Assessment::supported(
        UpdateBoundary::Replace,
        ComponentUpdateData::Preserve,
    ));
    assert_eq!(boundary(assess(base(json!({})))?), pause);
    for in_place in [
        json!({"data": {"limit": 1}}),
        json!({"modules": [{"name": "t.rego", "source": "package t\nimport rego.v1\nbig := input.n > 1\n"}]}),
        json!({"output_label": "X"}),
        json!({"on_error": "fail"}),
        json!({"timeout_ms": 7}),
        json!({"input": {"n": "$properties.m"}}),
    ] {
        assert_eq!(
            boundary(assess(base(in_place.clone()))?),
            pause,
            "{in_place}"
        );
    }
    for replaced in [
        json!({"stream": "policy/other"}),
        json!({"labels": ["Other"]}),
    ] {
        assert_eq!(
            boundary(assess(base(replaced.clone()))?),
            replace,
            "{replaced}"
        );
    }
    assert!(assess(base(json!({"query": "data.t.none"}))).is_err());
    let mut other = update_request(&PolicyFactory, &id, base(json!({})), base(json!({})));
    other.proposed.id = ComponentId::try_new("0a6f6a51-2b3e-4f26-8c4f-0e8b7c0e7d11")?;
    assert!(PolicyFactory.assess(&other).is_err());
    Ok(())
}

#[tokio::test]
async fn managed_updates_reevaluate_and_continue_with_changed_decisions_only() -> Result<()> {
    let current = base(json!({"provenance": false}));
    let mut policy = start(current.clone()).await?;
    emit(
        &mut policy,
        &[
            insert(item("a", 1, 5)),
            insert(item("b", 2, 50)),
            insert(item("c", 3, 15)),
        ],
    )
    .await?;

    // Updates require a completed pause.
    let proposed = base(json!({"provenance": false, "data": {"limit": 20}}));
    assert!(matches!(
        apply(&mut policy, current.clone(), proposed.clone()).await,
        Err(ApplyError::Refused { .. })
    ));
    policy.pause(&contexts::pause()).await?;
    // A stale current configuration is refused.
    assert!(apply(
        &mut policy,
        base(json!({"provenance": false, "data": {"limit": 1}})),
        proposed.clone()
    )
    .await
    .is_err());
    apply(&mut policy, current, proposed.clone()).await?;
    assert_eq!(
        policy.configuration()?,
        PolicyConfig::parse(proposed.clone()).map(|c| serde_json::to_value(&c))??
    );
    assert!(
        !policy.has_pending_emissions(),
        "nothing is emitted while paused"
    );
    policy.resume(&contexts::resume()).await?;
    let changes = drain(&mut policy).await?;
    // Only `c` (15 > 20 is now false) changed.
    assert_eq!(changes.len(), 1);
    let c = element(&changes[0]);
    assert!(matches!(changes[0], SourceChange::Replace { .. }));
    assert_eq!(c.get_reference(), &ElementReference::new("source", "c"));
    assert_eq!(property(c, "big"), json!(false));
    assert_eq!(c.get_effective_from(), 3);
    assert_eq!(
        property(
            &snapshot_rows(&policy).await?[&ElementReference::new("source", "c")],
            "big"
        ),
        json!(false)
    );

    // An input that touches a pending row supersedes the re-evaluation.
    policy.pause(&contexts::pause()).await?;
    let relabelled =
        base(json!({"provenance": false, "data": {"limit": 20}, "output_label": "Big"}));
    apply(&mut policy, proposed, relabelled.clone()).await?;
    assert_eq!(policy.pending.len(), 3);
    policy.resume(&contexts::resume()).await?;
    let changes = emit(&mut policy, &[insert(item("a", 4, 5))]).await?;
    assert_eq!(
        &*element(&changes[0]).get_metadata().labels,
        &[Arc::from("Big")]
    );
    assert_eq!(policy.pending.len(), 2);
    let changes = drain(&mut policy).await?;
    assert_eq!(changes.len(), 2);
    assert!(changes
        .iter()
        .all(|change| element(change).get_effective_from() == 4));

    // A failing re-evaluation under on_error=fail leaves the transformer unchanged.
    policy.pause(&contexts::pause()).await?;
    let failing = base(json!({
        "provenance": false, "data": {"limit": 20}, "output_label": "Big", "on_error": "fail",
        "strict_builtin_errors": true,
        "modules": [{"name": "t.rego", "source": "package t\nimport rego.v1\nbig := 1 / (input.n - 5)\n"}],
    }));
    assert!(matches!(
        apply(&mut policy, relabelled.clone(), failing).await,
        Err(ApplyError::Failed(_))
    ));
    assert_eq!(
        policy.configuration()?,
        PolicyConfig::parse(relabelled.clone()).map(|c| serde_json::to_value(&c))??
    );
    assert!(policy.pending.is_empty());

    // Lowering retention below the live count needs a new instance.
    let request = update_request(
        &PolicyFactory,
        &ComponentId::try_new(ID)?,
        relabelled.clone(),
        base(
            json!({"provenance": false, "data": {"limit": 20}, "output_label": "Big", "max_retained_inputs": 1}),
        ),
    );
    assert_eq!(
        NativeManagement::assess(&policy, &request)?
            .plan()
            .map(|plan| plan.mode),
        Assessment::supported(UpdateBoundary::Replace, ComponentUpdateData::Preserve)
            .plan()
            .map(|plan| plan.mode)
    );
    Ok(())
}

#[tokio::test]
async fn provenance_hash_changes_with_the_policy() -> Result<()> {
    let current = base(json!({}));
    let mut policy = start(current.clone()).await?;
    let changes = emit(&mut policy, &[insert(item("a", 1, 5))]).await?;
    let before = property(element(&changes[0]), "policy_hash");
    policy.pause(&contexts::pause()).await?;
    apply(&mut policy, current, base(json!({"data": {"limit": 11}}))).await?;
    policy.resume(&contexts::resume()).await?;
    let changes = drain(&mut policy).await?;
    assert_eq!(changes.len(), 1);
    let after = property(element(&changes[0]), "policy_hash");
    assert_ne!(before, after);
    assert_eq!(property(element(&changes[0]), "big"), json!(false));
    Ok(())
}

fn customer_eu() -> Value {
    json!({
        "policy_id": "customer-eu", "name": "Customer EU", "customer_id": "customer-eu",
        "allowed_regions": ["northeurope", "westeurope"],
        "allowed_purposes": ["customer-support"],
        "allowed_classifications": ["restricted"],
        "authority_ref": "contract://customer-eu/2026", "revision": 1
    })
}

fn demo_open() -> Value {
    json!({
        "policy_id": "demo-open", "name": "Demo open", "customer_id": "demo",
        "allowed_regions": ["*"], "allowed_purposes": ["demo"],
        "allowed_classifications": ["synthetic"],
        "authority_ref": "policy://demo-open", "revision": 1
    })
}

fn profile(customer: &str, classification: &str, policy: &str) -> Value {
    json!({
        "data_profile_id": format!("{customer}-{classification}"), "customer_id": customer,
        "classification": classification, "policy_id": policy,
        "authority_ref": format!("profile://{customer}"), "revision": 1
    })
}

fn cluster(id: &str, region: &str) -> Value {
    json!({"cluster_id": id, "name": id, "region": region, "revision": 1})
}

/// One workload/cluster pair row, as an upstream continuous query returns it.
fn pair(workload: &str, purpose: &str, profile: Value, policy: Value, cluster: Value) -> Element {
    let id = format!(
        "{workload}/{}",
        cluster["cluster_id"].as_str().expect("cluster id")
    );
    node(
        &id,
        "PlacementPair",
        1,
        json!({
            "workload_id": workload,
            "data_profile_id": profile["data_profile_id"],
            "purpose": purpose,
            "data_profile": profile,
            "policy": policy,
            "cluster": cluster,
        }),
    )
}

fn placement_input() -> Value {
    json!({
        "workload": {
            "workload_id": "$properties.workload_id",
            "data_profile_id": "$properties.data_profile_id",
            "purpose": "$properties.purpose"
        },
        "data_profile": "$properties.data_profile",
        "policy": "$properties.policy",
        "cluster": "$properties.cluster"
    })
}

fn placement_pairs() -> Vec<SourceChange> {
    let eu = profile("customer-eu", "restricted", "customer-eu");
    let demo = profile("demo", "synthetic", "demo-open");
    vec![
        insert(pair(
            "w-eu",
            "customer-support",
            eu.clone(),
            customer_eu(),
            cluster("eu-primary", "westeurope"),
        )),
        insert(pair(
            "w-eu",
            "customer-support",
            eu.clone(),
            customer_eu(),
            cluster("eu-recovery", "northeurope"),
        )),
        insert(pair(
            "w-eu",
            "customer-support",
            eu.clone(),
            customer_eu(),
            cluster("us-spare", "eastus"),
        )),
        insert(pair(
            "w-eu-demo",
            "demo",
            eu,
            customer_eu(),
            cluster("eu-primary", "westeurope"),
        )),
        insert(pair(
            "w-mixed",
            "customer-support",
            profile("demo", "synthetic", "customer-eu"),
            customer_eu(),
            cluster("eu-primary", "westeurope"),
        )),
        insert(pair(
            "w-demo",
            "demo",
            demo,
            demo_open(),
            cluster("us-spare", "eastus"),
        )),
    ]
}

fn by_id(changes: &[SourceChange]) -> HashMap<String, Element> {
    changes
        .iter()
        .map(|change| {
            let element = element(change).clone();
            (element.get_reference().element_id.to_string(), element)
        })
        .collect()
}

/// gpu-cluster-lab's placement.rego, unchanged, driven purely by configuration.
#[tokio::test]
async fn gpu_lab_placement_policy_runs_from_configuration() -> Result<()> {
    let mut policy = start(json!({
        "stream": "placement-eligibility",
        "modules": [{"name": "placement.rego", "source": PLACEMENT}],
        "query": "data.gpu.placement.decision",
        "labels": ["PlacementPair"],
        "input": placement_input(),
        "output_label": "PlacementEligibility",
        "output_properties": {
            "workload_id": "$input.workload.workload_id",
            "cluster_id": "$input.cluster.cluster_id",
            "region": "$input.cluster.region",
            "allowed": "$decision.allow",
            "reasons": "$decision.reasons"
        },
        "timeout_ms": 50,
        "timeout_check_interval": 64,
        "on_error": "emit"
    }))
    .await?;
    let decisions = by_id(&emit(&mut policy, &placement_pairs()).await?);
    let expect = |id: &str, allowed: bool, reasons: Value| {
        let decision = &decisions[id];
        assert_eq!(
            &*decision.get_metadata().labels,
            &[Arc::from("PlacementEligibility")],
            "{id}"
        );
        assert_eq!(property(decision, "allowed"), json!(allowed), "{id}");
        assert_eq!(property(decision, "reasons"), reasons, "{id}");
        assert_eq!(
            property(decision, "policy_status"),
            json!("decided"),
            "{id}"
        );
    };
    expect("w-eu/eu-primary", true, json!([]));
    expect("w-eu/eu-recovery", true, json!([]));
    expect("w-eu/us-spare", false, json!(["region-not-permitted"]));
    expect(
        "w-eu-demo/eu-primary",
        false,
        json!(["purpose-not-permitted"]),
    );
    expect(
        "w-mixed/eu-primary",
        false,
        json!(["classification-not-permitted", "customer-mismatch"]),
    );
    expect("w-demo/us-spare", true, json!([]));
    assert_eq!(
        property(&decisions["w-eu/us-spare"], "cluster_id"),
        json!("us-spare")
    );
    assert_eq!(
        property(&decisions["w-eu/us-spare"], "workload_id"),
        json!("w-eu")
    );

    // A policy edit arriving through the upstream rows re-decides only that pair.
    let mut widened = customer_eu();
    widened["allowed_regions"] = json!(["eastus", "northeurope", "westeurope"]);
    widened["revision"] = json!(2);
    let changes = emit(
        &mut policy,
        &[SourceChange::update(node(
            "w-eu/us-spare",
            "PlacementPair",
            2,
            json!({"policy": widened}),
        ))],
    )
    .await?;
    assert_eq!(changes.len(), 1);
    assert_eq!(property(element(&changes[0]), "allowed"), json!(true));
    assert_eq!(property(element(&changes[0]), "reasons"), json!([]));
    Ok(())
}

/// The demo's Allow/Deny/Unknown authorization, context validation and a
/// managed policy-data update that re-emits only the affected decisions.
#[tokio::test]
async fn gpu_lab_eligibility_with_managed_policy_updates() -> Result<()> {
    let configuration = |purposes: Value| {
        json!({
            "stream": "placement-eligibility",
            "modules": [
                {"name": "placement.rego", "source": PLACEMENT},
                {"name": "eligibility.rego", "source": ELIGIBILITY}
            ],
            "data": {"valid_classifications": ["restricted", "synthetic"], "valid_purposes": purposes},
            "query": "data.gpu.eligibility.result",
            "labels": ["PlacementPair"],
            "input": placement_input(),
            "output_label": "PlacementEligibility",
            "output_properties": {
                "workload_id": "$input.workload.workload_id",
                "cluster_id": "$input.cluster.cluster_id",
                "authorization": "$decision.authorization",
                "reasons": "$decision.reasons"
            },
            "provenance": false,
            "timeout_ms": 50
        })
    };
    let current = configuration(json!(["customer-support", "demo"]));
    let mut policy = start(current.clone()).await?;
    let mut pairs = placement_pairs();
    pairs.push(insert(pair(
        "w-training",
        "training",
        profile("demo", "synthetic", "demo-open"),
        demo_open(),
        cluster("us-spare", "eastus"),
    )));
    let decisions = by_id(&emit(&mut policy, &pairs).await?);
    let authorization = |decisions: &HashMap<String, Element>, id: &str| {
        (
            property(&decisions[id], "authorization"),
            property(&decisions[id], "reasons"),
        )
    };
    assert_eq!(
        authorization(&decisions, "w-eu/eu-primary"),
        (json!("Allow"), json!([]))
    );
    assert_eq!(
        authorization(&decisions, "w-eu/us-spare"),
        (json!("Deny"), json!(["region-not-permitted"]))
    );
    assert_eq!(
        authorization(&decisions, "w-demo/us-spare"),
        (json!("Allow"), json!([]))
    );
    assert_eq!(
        authorization(&decisions, "w-training/us-spare"),
        (json!("Unknown"), json!(["invalid-policy-context"]))
    );

    policy.pause(&contexts::pause()).await?;
    apply(
        &mut policy,
        current,
        configuration(json!(["customer-support", "demo", "training"])),
    )
    .await?;
    policy.resume(&contexts::resume()).await?;
    let changes = drain(&mut policy).await?;
    assert_eq!(changes.len(), 1);
    let decisions = by_id(&changes);
    assert_eq!(
        authorization(&decisions, "w-training/us-spare"),
        (json!("Deny"), json!(["purpose-not-permitted"]))
    );
    Ok(())
}

/// An upstream output whose snapshot the test controls.
struct Upstream {
    snapshot: std::sync::Mutex<(drasi_computation_contracts::OutputCursor, Vec<Element>)>,
}

#[async_trait]
impl SnapshotSource for Upstream {
    async fn read(
        &self,
        output: &drasi_computation_contracts::OutputEndpoint,
    ) -> Result<(
        drasi_computation_contracts::OutputCursor,
        Vec<drasi_computation_contracts::Record>,
    )> {
        assert_eq!(output.component.as_str(), UPSTREAM);
        let (cursor, elements) = self.snapshot.lock().unwrap().clone();
        let records = elements
            .into_iter()
            .map(|element| {
                Ok(GraphChangeCodec::element_record(&ElementImage::try_from(
                    element,
                )?)?)
            })
            .collect::<Result<_>>()?;
        Ok((cursor, records))
    }
}

const UPSTREAM: &str = "5d7e0f4b-2a64-4bb0-9a3c-6f1e2d3c4b5a";

fn new_producer() -> Result<GraphProducerIdentity> {
    GraphProducerIdentity::volatile(
        "instance".into(),
        "graph".into(),
        ComponentId::try_new(UPSTREAM)?,
        StreamId::try_new("source/out")?,
    )
}

fn binding() -> Result<InputBinding> {
    Ok(InputBinding {
        input_port: PortId::try_new("in")?,
        output: drasi_computation_contracts::OutputEndpoint::new(
            ComponentId::try_new(UPSTREAM)?,
            PortId::try_new("out")?,
        ),
        stream: StreamId::try_new("source/out")?,
    })
}

fn set_snapshot(
    upstream: &Upstream,
    producer: &GraphProducerIdentity,
    sequence: u64,
    elements: Vec<Element>,
) -> Result<()> {
    *upstream.snapshot.lock().unwrap() = (
        drasi_computation_contracts::OutputCursor::try_new(producer.clone(), sequence)?,
        elements,
    );
    Ok(())
}

async fn start_with(upstream: Arc<Upstream>) -> Result<PolicyTransformer> {
    let mut policy = PolicyTransformer::new(
        PolicyFactory
            .metadata()
            .descriptor(ComponentId::try_new(ID)?)?,
        base(json!({})),
        None,
        &[binding()?],
        Some(upstream),
    )?;
    policy.initialize(&contexts::initialize()).await?;
    policy.start(&mut contexts::start()).await?;
    Ok(policy)
}

/// A live envelope from `producer` at `sequence`.
async fn live(
    policy: &mut PolicyTransformer,
    producer: &GraphProducerIdentity,
    sequence: u64,
    changes: &[SourceChange],
) -> Result<Vec<SourceChange>> {
    let mut envelope = input(changes, sequence)?.envelope;
    GraphProducerProgress::annotate(&mut envelope, producer, sequence)?;
    let outputs = policy
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope,
        })
        .await?;
    decode(outputs, policy)
}

fn bigs(rows: &HashMap<ElementReference, Element>) -> Vec<(String, Value)> {
    let mut bigs: Vec<_> = rows
        .iter()
        .map(|(reference, row)| (reference.element_id.to_string(), property(row, "big")))
        .collect();
    bigs.sort_by(|a, b| a.0.cmp(&b.0));
    bigs
}

#[tokio::test]
async fn initializes_from_upstream_snapshots_and_skips_covered_input() -> Result<()> {
    let producer = new_producer()?;
    let upstream = Arc::new(Upstream {
        snapshot: std::sync::Mutex::new((
            drasi_computation_contracts::OutputCursor::try_new(producer.clone(), 0)?,
            vec![],
        )),
    });
    set_snapshot(
        &upstream,
        &producer,
        5,
        vec![item("a", 10, 20), item("b", 10, 5)],
    )?;
    let mut policy = start_with(upstream.clone()).await?;

    // The snapshot's decisions are emitted as a continuation.
    assert!(policy.has_pending_emissions());
    let initial = drain(&mut policy).await?;
    assert_eq!(initial.len(), 2);
    assert!(initial
        .iter()
        .all(|change| matches!(change, SourceChange::Insert { .. })));
    assert_eq!(
        bigs(&snapshot_rows(&policy).await?),
        [("a".into(), json!(true)), ("b".into(), json!(false))]
    );

    // An envelope the snapshot already covers changes nothing.
    let covered = live(&mut policy, &producer, 4, &[insert(item("a", 11, 1))]).await?;
    assert!(covered.is_empty());
    assert_eq!(
        property(&policy.inputs[&ElementReference::new("source", "a")], "n"),
        json!(20)
    );

    // A later one is applied.
    let changes = live(&mut policy, &producer, 6, &[insert(item("a", 12, 1))]).await?;
    assert_eq!(changes.len(), 1);
    assert_eq!(property(element(&changes[0]), "big"), json!(false));

    // Stop and start keep the position: nothing is read or emitted again.
    policy.stop(&contexts::stop(StopMode::Drain)).await?;
    set_snapshot(&upstream, &producer, 9, vec![])?;
    policy.start(&mut contexts::start()).await?;
    assert!(!policy.has_pending_emissions());
    Ok(())
}

#[tokio::test]
async fn reads_an_upstream_again_when_it_restarts() -> Result<()> {
    let producer = new_producer()?;
    let upstream = Arc::new(Upstream {
        snapshot: std::sync::Mutex::new((
            drasi_computation_contracts::OutputCursor::try_new(producer.clone(), 0)?,
            vec![],
        )),
    });
    set_snapshot(
        &upstream,
        &producer,
        3,
        vec![item("a", 10, 20), item("b", 10, 5)],
    )?;
    let mut policy = start_with(upstream.clone()).await?;
    drain(&mut policy).await?;

    // The upstream restarts: a new producer epoch whose snapshot no longer has `a`.
    let restarted = new_producer()?;
    assert_ne!(restarted, producer);
    set_snapshot(&upstream, &restarted, 1, vec![item("b", 20, 50)])?;
    let changes = live(&mut policy, &restarted, 2, &[insert(item("c", 21, 1))]).await?;
    assert_eq!(changes.len(), 3, "{changes:#?}");
    assert!(changes
        .iter()
        .any(|change| matches!(change, SourceChange::Delete { metadata } if &*metadata.reference.element_id == "a")));
    assert_eq!(
        bigs(&snapshot_rows(&policy).await?),
        [("b".into(), json!(true)), ("c".into(), json!(false))]
    );

    // Without snapshot reads (no dependency bound), the same input is applied as it is.
    let mut plain = start(base(json!({}))).await?;
    assert!(!plain.has_pending_emissions());
    let changes = live(&mut plain, &restarted, 1, &[insert(item("c", 21, 1))]).await?;
    assert_eq!(changes.len(), 1);
    Ok(())
}
