// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{
    ComponentId, ComponentRole, ComponentUpdateMode, OutputProgress, OutputReadError,
    SnapshotProvider, StreamId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::{DisposeReason, StateIntent, StopMode},
    testing::{self, contexts},
    ExportedPlugin, Factory,
};
use drasi_core_types::models::{ElementMetadata, ElementPropertyMap};
use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::{plugin, OptimizationFactory};

const ID: &str = "0d6f4c1e-8a3b-4f0e-9c55-2f1b7a9e6d10";
const STEP: Duration = Duration::from_secs(20);

type Snapshot = BTreeMap<String, (String, Value)>;

fn create(configuration: Value) -> Result<Optimization> {
    Optimization::new(
        OptimizationFactory
            .metadata()
            .descriptor(ComponentId::try_new(ID)?)?,
        OptimizationConfig::parse(configuration)?,
        Some(&Scope {
            instance_id: "instance".into(),
            graph_id: "graph".into(),
            generation: 1,
        }),
    )
}

async fn started(configuration: Value) -> Result<Optimization> {
    let mut optimization = create(configuration)?;
    optimization.start(&mut contexts::start()).await?;
    Ok(optimization)
}

fn node(label: &str, id: &str, properties: Value) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: vec![Arc::from(label)].into(),
            effective_from: 1,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

fn delete(id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: Arc::from([]),
            effective_from: 2,
        },
    }
}

fn insert(element: Element) -> SourceChange {
    SourceChange::Insert { element }
}

async fn send(optimization: &mut Optimization, changes: &[SourceChange]) -> Result<()> {
    let sequence = optimization.sequence + 1000;
    let outputs = optimization
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_changes(
                changes,
                StreamId::try_new("source/out")?,
                sequence,
                None,
            )?,
        })
        .await?;
    assert!(outputs.is_empty(), "transform defers output to wakeups");
    Ok(())
}

/// Drives wakeups until no work is pending; returns every emitted batch.
async fn settle(optimization: &mut Optimization) -> Result<Vec<Vec<SourceChange>>> {
    let wake = optimization.wakeup_source().context("wakeup source")?;
    let mut batches = Vec::new();
    while wake.has_pending().await? {
        tokio::time::timeout(STEP, wake.wait())
            .await
            .context("wakeup timed out")??;
        for output in optimization.on_wakeup().await? {
            assert_eq!(output.port.as_str(), "out");
            let progress = GraphProducerProgress::from_envelope(&output.envelope)?
                .context("missing progress")?;
            assert_eq!(progress.sequence(), output.envelope.system().sequence());
            assert_eq!(progress.identity().component_id().as_str(), ID);
            batches.push(GraphChangeCodec::decode_changes(&output.envelope)?);
        }
    }
    Ok(batches)
}

async fn snapshot(optimization: &Optimization) -> Result<Snapshot> {
    let reader: Arc<dyn SnapshotProvider> = optimization
        .output_readers()
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|readers| readers.snapshot())
        .cloned()
        .context("snapshot reader")?;
    let snapshot = reader.snapshot().await?;
    snapshot
        .records()
        .values()
        .map(|record| {
            let element = Element::try_from(GraphChangeCodec::decode_element(record)?)?;
            let metadata = element.get_metadata();
            assert_eq!(&*metadata.reference.namespace, ID);
            let properties = element
                .get_properties()
                .map_iter(|name, value| (name.to_string(), Value::from(value)))
                .collect::<serde_json::Map<_, _>>();
            Ok((
                metadata.reference.element_id.to_string(),
                (metadata.labels[0].to_string(), Value::Object(properties)),
            ))
        })
        .collect()
}

fn status(rows: &Snapshot) -> &Value {
    &rows["status"].1
}

fn labelled<'a>(rows: &'a Snapshot, label: &str) -> Vec<(&'a str, &'a Value)> {
    rows.iter()
        .filter(|(_, (row_label, _))| row_label == label)
        .map(|(id, (_, properties))| (id.as_str(), properties))
        .collect()
}

// ---------------------------------------------------------------- knapsack

fn knapsack(capacity: f64) -> Value {
    json!({
        "stream": "plan/out",
        "debounceMs": 5,
        "maxDelayMs": 50,
        "sets": [{"name": "items", "label": "Item", "key": ["name"]}],
        "variables": [{
            "name": "take", "type": "binary", "over": "items",
            "output": {"label": "Taken", "properties": ["name"]}
        }],
        "constraints": [{
            "name": "weight",
            "terms": [{"variable": "take", "coefficient": {"property": "weight"}}],
            "op": "<=", "rhs": capacity
        }],
        "objectives": [{
            "name": "value", "sense": "maximize",
            "terms": [{"variable": "take", "coefficient": {"property": "value"}}]
        }]
    })
}

fn item(name: &str, weight: f64, value: f64) -> Element {
    node(
        "Item",
        name,
        json!({"name": name, "weight": weight, "value": value}),
    )
}

fn items() -> Vec<SourceChange> {
    vec![
        insert(item("a", 5.0, 10.0)),
        insert(item("b", 4.0, 40.0)),
        insert(item("c", 6.0, 30.0)),
        insert(item("d", 3.0, 50.0)),
    ]
}

fn taken(rows: &Snapshot) -> Vec<String> {
    labelled(rows, "Taken")
        .into_iter()
        .map(|(_, properties)| properties["name"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn solves_and_publishes_a_knapsack() -> Result<()> {
    let mut optimization = started(knapsack(10.0)).await?;
    assert!(
        settle(&mut optimization).await?.is_empty(),
        "no solve before input"
    );
    send(&mut optimization, &items()).await?;
    let batches = settle(&mut optimization).await?;
    assert_eq!(batches.len(), 1);
    assert!(batches[0]
        .iter()
        .all(|change| matches!(change, SourceChange::Insert { .. })));
    let rows = snapshot(&optimization).await?;
    assert_eq!(taken(&rows), ["b", "d"]);
    assert_eq!(
        rows["take/b"].1,
        json!({"name": "b", "variable": "take", "value": 1})
    );
    let status = status(&rows);
    assert_eq!(status["status"], "optimal");
    assert_eq!(status["objective"], 90.0);
    assert_eq!(status["objectives"], json!({"value": 90.0}));
    assert_eq!(status["variables"], 4);
    assert_eq!(status["constraints"], 1);
    assert!(status["signature"].is_string());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn provenance_identifies_the_inputs_of_a_solution() -> Result<()> {
    let mut configuration = knapsack(10.0);
    configuration["sets"]
        .as_array_mut()
        .context("sets")?
        .push(json!({"name": "context", "label": "Context", "key": ["name"]}));
    configuration["provenance"] = json!({"set": "context", "properties": ["version"]});
    let mut optimization = started(configuration).await?;
    send(&mut optimization, &items()).await?;
    settle(&mut optimization).await?;
    assert_eq!(
        status(&snapshot(&optimization).await?)["provenance"],
        Value::Null
    );
    send(
        &mut optimization,
        &[insert(node(
            "Context",
            "context",
            json!({"name": "demo", "version": 7, "other": true}),
        ))],
    )
    .await?;
    settle(&mut optimization).await?;
    let rows = snapshot(&optimization).await?;
    assert_eq!(taken(&rows), ["b", "d"]);
    assert_eq!(status(&rows)["provenance"], json!({"version": 7}));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn input_changes_retract_stale_solution_rows() -> Result<()> {
    let mut optimization = started(knapsack(10.0)).await?;
    send(&mut optimization, &items()).await?;
    settle(&mut optimization).await?;
    // d becomes heavy: {b, c} (70) now beats every set containing d.
    send(
        &mut optimization,
        &[SourceChange::Replace {
            element: item("d", 9.0, 50.0),
        }],
    )
    .await?;
    let batches = settle(&mut optimization).await?;
    assert_eq!(batches.len(), 1);
    let changes = &batches[0];
    let deleted: Vec<_> = changes
        .iter()
        .filter_map(|change| match change {
            SourceChange::Delete { metadata } => Some(metadata.reference.element_id.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(deleted, ["take/d"]);
    assert!(changes.iter().any(|change| matches!(
        change,
        SourceChange::Insert { element } if &*element.get_reference().element_id == "take/c"
    )));
    let rows = snapshot(&optimization).await?;
    assert_eq!(taken(&rows), ["b", "c"]);
    assert_eq!(status(&rows)["objective"], 70.0);
    // Deleting all items leaves an empty, trivially optimal model.
    send(&mut optimization, &["a", "b", "c", "d"].map(delete)).await?;
    settle(&mut optimization).await?;
    let rows = snapshot(&optimization).await?;
    assert!(taken(&rows).is_empty());
    assert_eq!(status(&rows)["status"], "optimal");
    assert_eq!(status(&rows)["objective"], 0.0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn infeasible_unbounded_and_limit_statuses() -> Result<()> {
    // Infeasible: requiring at least five of four items.
    let mut configuration = knapsack(100.0);
    configuration["constraints"]
        .as_array_mut()
        .context("constraints")?
        .push(json!({"name": "many", "terms": [{"variable": "take"}], "op": ">=", "rhs": 5}));
    let mut optimization = started(configuration).await?;
    send(&mut optimization, &items()).await?;
    settle(&mut optimization).await?;
    let rows = snapshot(&optimization).await?;
    assert_eq!(status(&rows)["status"], "infeasible");
    assert!(taken(&rows).is_empty());
    assert!(status(&rows)["objective"].is_null());

    // Unbounded: an unbounded continuous variable is maximized.
    let mut optimization = started(json!({
        "stream": "plan/out", "debounceMs": 0, "maxDelayMs": 0,
        "sets": [],
        "variables": [{"name": "y", "type": "continuous"}],
        "objectives": [{"name": "grow", "sense": "maximize", "terms": [{"variable": "y"}]}]
    }))
    .await?;
    send(&mut optimization, &[insert(item("x", 1.0, 1.0))]).await?;
    settle(&mut optimization).await?;
    let rows = snapshot(&optimization).await?;
    assert_eq!(status(&rows)["status"], "unbounded");
    assert_eq!(rows.len(), 1);

    // Limit: the model is larger than maxVariables.
    let mut configuration = knapsack(10.0);
    configuration["maxVariables"] = json!(3);
    let mut optimization = started(configuration).await?;
    send(&mut optimization, &items()).await?;
    settle(&mut optimization).await?;
    let rows = snapshot(&optimization).await?;
    assert_eq!(status(&rows)["status"], "limit");
    assert!(status(&rows)["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("maxVariables")));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_resolve_retracts_the_previous_solution() -> Result<()> {
    let mut optimization = started(knapsack(10.0)).await?;
    send(&mut optimization, &items()).await?;
    settle(&mut optimization).await?;
    // A missing coefficient property is a model error.
    send(
        &mut optimization,
        &[SourceChange::Replace {
            element: node("Item", "a", json!({"name": "a", "value": 1})),
        }],
    )
    .await?;
    settle(&mut optimization).await?;
    let rows = snapshot(&optimization).await?;
    assert_eq!(status(&rows)["status"], "error");
    assert!(status(&rows)["detail"]
        .as_str()
        .is_some_and(|detail| detail.contains("weight")));
    assert!(taken(&rows).is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rapid_changes_are_coalesced_and_unchanged_models_skipped() -> Result<()> {
    let mut configuration = knapsack(10.0);
    configuration["debounceMs"] = json!(200);
    configuration["maxDelayMs"] = json!(2000);
    let mut optimization = started(configuration).await?;
    for change in items() {
        send(&mut optimization, &[change]).await?;
    }
    let batches = settle(&mut optimization).await?;
    assert_eq!(batches.len(), 1, "one solve for the burst");
    assert_eq!(optimization.sequence, 1);
    // A record outside every set changes nothing in the model.
    send(&mut optimization, &[insert(node("Other", "o", json!({})))]).await?;
    assert!(settle(&mut optimization).await?.is_empty());
    assert_eq!(optimization.sequence, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ties_resolve_identically_regardless_of_input_order() -> Result<()> {
    let ties = ["p", "q", "r", "s"].map(|name| item(name, 1.0, 1.0));
    let mut results = Vec::new();
    for order in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1]] {
        let mut optimization = started(knapsack(2.0)).await?;
        for index in order {
            send(&mut optimization, &[insert(ties[index].clone())]).await?;
        }
        settle(&mut optimization).await?;
        results.push(snapshot(&optimization).await?);
    }
    assert_eq!(taken(&results[0]).len(), 2);
    assert!(results.windows(2).all(|pair| pair[0] == pair[1]));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_gates_publication_until_resume() -> Result<()> {
    let mut optimization = started(knapsack(10.0)).await?;
    send(&mut optimization, &items()).await?;
    optimization.pause(&contexts::pause()).await?;
    let wake = optimization.wakeup_source().context("wakeup")?;
    assert!(!wake.has_pending().await?);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), wake.wait())
            .await
            .is_err(),
        "a paused optimization does not wake"
    );
    assert!(optimization.on_wakeup().await?.is_empty());
    optimization.resume(&contexts::resume()).await?;
    assert_eq!(settle(&mut optimization).await?.len(), 1);
    assert_eq!(taken(&snapshot(&optimization).await?), ["b", "d"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_restart_and_erase() -> Result<()> {
    let mut optimization = started(knapsack(10.0)).await?;
    send(&mut optimization, &items()).await?;
    optimization.stop(&contexts::stop(StopMode::Abort)).await?;
    assert!(!optimization.wake().is_ready());
    let wake = optimization.wakeup_source().context("wakeup")?;
    assert!(!wake.has_pending().await?);
    assert!(matches!(
        snapshot(&optimization).await,
        Err(error) if error.downcast_ref::<OutputReadError>().is_some_and(|error| matches!(error, OutputReadError::NotReady(_)))
    ));
    // The retained dataset is re-solved after a restart.
    optimization.start(&mut contexts::start()).await?;
    assert!(optimization.start(&mut contexts::start()).await.is_err());
    settle(&mut optimization).await?;
    assert_eq!(taken(&snapshot(&optimization).await?), ["b", "d"]);
    assert!(optimization
        .dispose(&contexts::dispose(
            DisposeReason::Removal,
            StateIntent::Erase,
            1
        )?)
        .await
        .is_err());
    optimization.stop(&contexts::stop(StopMode::Drain)).await?;
    optimization
        .dispose(&contexts::dispose(
            DisposeReason::Removal,
            StateIntent::Erase,
            1,
        )?)
        .await?;
    optimization
        .dispose(&contexts::dispose(
            DisposeReason::Removal,
            StateIntent::Erase,
            2,
        )?)
        .await?;
    assert!(matches!(
        snapshot(&optimization).await,
        Err(error) if error.downcast_ref::<OutputReadError>().is_some_and(|error| matches!(error, OutputReadError::Retired))
    ));
    assert!(optimization.start(&mut contexts::start()).await.is_err());
    let mut cancelled = create(knapsack(10.0))?;
    assert!(cancelled
        .start(&mut contexts::cancelled_start())
        .await
        .is_err());
    cancelled.start(&mut contexts::start()).await?;
    Ok(())
}

// ---------------------------------------------------------------- management

#[test]
fn metadata_is_valid() -> Result<()> {
    ExportedPlugin::new(plugin()?)?;
    let metadata = OptimizationFactory.metadata();
    assert_eq!(&*metadata.implementation.name, crate::OPTIMIZATION);
    assert_eq!(metadata.role, ComponentRole::Transformer);
    let capabilities = &metadata.capabilities;
    assert!(capabilities.wakeups && capabilities.management && capabilities.erases_owned_state);
    assert_eq!(capabilities.output_progress, Some(OutputProgress::Origin));
    assert_eq!(
        capabilities.update_modes,
        [ComponentUpdateMode::InPlace, ComponentUpdateMode::Recreate]
    );
    let details = OptimizationFactory
        .configuration_details()
        .context("details")?;
    details.validate_schema(&metadata.configuration)?;
    metadata.configuration.validate(&knapsack(10.0))?;
    Ok(())
}

#[test]
fn factory_creates_and_round_trips_configuration() -> Result<()> {
    let id = ComponentId::try_new(ID)?;
    let created = OptimizationFactory.create(
        &testing::create_request(&OptimizationFactory, &id, knapsack(10.0)),
        drasi_computation_plugin_sdk::ControlSender::default(),
    );
    let drasi_computation_plugin_sdk::Component::Transformer(transformer) = created?.component
    else {
        anyhow::bail!("expected a transformer");
    };
    assert!(transformer.wakeup_source().is_some());
    assert!(OptimizationFactory
        .create(
            &testing::create_request(&OptimizationFactory, &id, json!({"stream": "s"})),
            drasi_computation_plugin_sdk::ControlSender::default(),
        )
        .is_err());
    let optimization = create(knapsack(10.0))?;
    let normalized = optimization.configuration()?;
    assert_eq!(normalized["debounceMs"], 5);
    assert_eq!(normalized["timeLimitMs"], 2000);
    assert_eq!(normalized["statusLabel"], "OptimizationStatus");
    assert_eq!(OptimizationConfig::parse(normalized)?, *optimization.config);
    Ok(())
}

#[test]
fn configuration_is_validated() {
    let invalid = [
        json!({"stream": "s", "sets": [], "variables": [], "extra": 1}),
        json!({"stream": "s", "sets": [], "variables": [{"name": "x", "type": "binary", "over": "missing"}]}),
        json!({"stream": "s", "sets": [], "variables": [{"name": "x", "type": "binary", "upper": 3}]}),
        json!({"stream": "s", "sets": [], "variables": [{"name": "status", "type": "continuous"}]}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A"}, {"name": "a", "label": "B"}], "variables": []}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A", "where": [{"inSet": "b", "match": {"k": "k"}}]}, {"name": "b", "label": "B"}], "variables": []}),
        json!({"stream": "s", "sets": [], "variables": [{"name": "x", "type": "continuous"}],
               "constraints": [{"name": "c", "terms": [{"variable": "y"}], "op": "<=", "rhs": 1}]}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A"}], "variables": [{"name": "x", "type": "binary", "over": "a"}],
               "constraints": [{"name": "c", "forEach": "a", "groupBy": ["k"], "terms": [{"variable": "x"}], "op": "<=", "rhs": 1}]}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A"}], "variables": [{"name": "x", "type": "binary", "over": "a"}],
               "constraints": [{"name": "c", "terms": [{"variable": "x"}], "op": "<=", "rhs": {"property": "cap"}}]}),
        json!({"stream": "s", "debounceMs": 100, "maxDelayMs": 10, "sets": [], "variables": []}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A"}], "variables": [],
               "scenarios": [{"name": "loss", "set": "missing", "property": "p"}]}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A"}], "variables": [{"name": "x", "type": "binary", "over": "a"}],
               "preconditions": [{"name": "p", "set": "a"}]}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A"}], "variables": [{"name": "x", "type": "binary", "over": "a"}],
               "preconditions": [{"name": "p", "set": "a", "min": 2, "max": 1}]}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A"}], "variables": [{"name": "x", "type": "binary", "over": "a"}],
               "preconditions": [{"name": "p", "set": "missing", "max": 0}]}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A"}], "variables": [{"name": "x", "type": "binary", "over": "a"}],
               "summaries": [{"name": "s", "variable": "missing"}]}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A"}], "variables": [{"name": "x", "type": "continuous"}],
               "summaries": [{"name": "s", "variable": "x", "properties": ["p"]}]}),
        json!({"stream": "s", "sets": [{"name": "a", "label": "A"}], "variables": [{"name": "x", "type": "binary", "over": "a"}],
               "summaries": [{"name": "s", "variable": "x"}, {"name": "s", "variable": "x"}]}),
    ];
    for configuration in invalid {
        assert!(
            OptimizationConfig::parse(configuration.clone()).is_err(),
            "accepted {configuration}"
        );
    }
    assert!(OptimizationConfig::parse(gpu_model()).is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_are_planned_and_applied_at_a_pause() -> Result<()> {
    let id = ComponentId::try_new(ID)?;
    let factory = OptimizationFactory;
    let request =
        |current: Value, proposed: Value| testing::update_request(&factory, &id, current, proposed);
    let pause = factory.assess(&request(knapsack(10.0), knapsack(4.0)))?;
    assert!(matches!(
        pause,
        Assessment::Supported {
            boundary: UpdateBoundary::Pause,
            ..
        }
    ));
    let mut restream = knapsack(10.0);
    restream["stream"] = json!("other/out");
    let replace = factory.assess(&request(knapsack(10.0), restream))?;
    assert!(matches!(
        replace,
        Assessment::Supported {
            boundary: UpdateBoundary::Replace,
            ..
        }
    ));
    let mut invalid = knapsack(10.0);
    invalid["variables"][0]["over"] = json!("missing");
    assert!(factory.assess(&request(knapsack(10.0), invalid)).is_err());
    let mut moved = request(knapsack(10.0), knapsack(4.0));
    moved.proposed.id = ComponentId::try_new("a8a5f3a0-2b1e-4d61-9a1f-0b7c3d2e4f50")?;
    assert!(factory.assess(&moved).is_err());

    let mut optimization = started(knapsack(10.0)).await?;
    send(&mut optimization, &items()).await?;
    settle(&mut optimization).await?;
    let update = drasi_computation_plugin_sdk::ApplyUpdateRequest {
        operation_id: "update-1".into(),
        request: request(knapsack(10.0), knapsack(4.0)),
        plan: pause.plan().context("plan")?,
        fingerprint: None,
    };
    let management = optimization.management().context("management")?;
    assert!(management.assess(&update.request).is_ok());
    assert!(matches!(
        management
            .apply(&contexts::apply(UpdateBoundary::Pause), &update)
            .await,
        Err(ApplyError::Refused { .. })
    ));
    optimization.pause(&contexts::pause()).await?;
    let stale = drasi_computation_plugin_sdk::ApplyUpdateRequest {
        request: request(knapsack(7.0), knapsack(4.0)),
        ..update.clone()
    };
    let management = optimization.management().context("management")?;
    assert!(management
        .apply(&contexts::apply(UpdateBoundary::Pause), &stale)
        .await
        .is_err());
    management
        .apply(&contexts::apply(UpdateBoundary::Pause), &update)
        .await
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    optimization.resume(&contexts::resume()).await?;
    settle(&mut optimization).await?;
    let rows = snapshot(&optimization).await?;
    assert_eq!(taken(&rows), ["d"]);
    assert_eq!(status(&rows)["objective"], 50.0);
    assert_eq!(optimization.configuration()?["constraints"][0]["rhs"], 4.0);
    Ok(())
}

// ---------------------------------------------------------------- gpu-cluster-lab

/// The gpu-cluster-lab placement and resilience model, as configuration.
///
/// Inputs are the rows the demo's upstream continuous queries produce:
/// `Gpu` (observed capacity), `Workload`, `Pair` (one per workload and GPU,
/// joining their properties, with the policy and model-compatibility verdicts)
/// and `Assignment` (the current plan).
fn gpu_model() -> Value {
    json!({
        "stream": "placement/out",
        "debounceMs": 0,
        "maxDelayMs": 0,
        "timeLimitMs": 10000,
        "sets": [
            {"name": "gpus", "label": "Gpu", "key": ["gpu_id"], "where": [
                {"property": "eligible", "op": "==", "value": true}
            ]},
            {"name": "workloads", "label": "Workload", "key": ["workload_id"]},
            {"name": "replicas", "from": "workloads",
             "expand": {"count": "replicas", "index": "replica_index"}},
            {"name": "previous", "label": "Assignment", "key": ["workload_id", "replica_index"]},
            {"name": "pairs", "label": "Pair", "key": ["workload_id", "gpu_id"], "where": [
                {"inSet": "gpus", "match": {"gpu_id": "gpu_id"}},
                {"property": "scheduling_enabled", "op": "==", "value": true},
                {"property": "model_allowed", "op": "==", "value": true},
                {"property": "policy_allowed", "op": "==", "value": true},
                {"property": "gpu_memory_mib", "op": ">=", "valueProperty": "memory_mib"},
                {"property": "gpu_compute_units", "op": ">=", "valueProperty": "compute_units"}
            ]},
            {"name": "edges", "from": "pairs",
             "expand": {"count": "replicas", "index": "replica_index"}}
        ],
        "variables": [
            {"name": "assign", "type": "binary", "over": "edges",
             "output": {"label": "Assignment", "properties": ["workload_id", "replica_index", "gpu_id"]}},
            {"name": "peak", "type": "continuous", "lower": 0, "output": {"emit": false}}
        ],
        "constraints": [
            {"name": "one-gpu-per-replica", "forEach": "replicas",
             "terms": [{"variable": "assign",
                        "match": {"workload_id": "workload_id", "replica_index": "replica_index"}}],
             "op": "==", "rhs": 1},
            {"name": "observed-memory", "forEach": "gpus",
             "terms": [{"variable": "assign", "match": {"gpu_id": "gpu_id"},
                        "coefficient": {"property": "memory_mib", "scale": 0.0009765625}}],
             "op": "<=", "rhs": {"property": "memory_mib", "scale": 0.0009765625}},
            {"name": "installed-memory", "forEach": "gpus",
             "terms": [{"variable": "assign", "match": {"gpu_id": "gpu_id"},
                        "coefficient": {"property": "memory_mib", "scale": 0.0009765625}}],
             "op": "<=", "rhs": {"property": "installed_memory_mib", "scale": 0.0009765625}},
            {"name": "observed-compute", "forEach": "gpus",
             "terms": [{"variable": "assign", "match": {"gpu_id": "gpu_id"},
                        "coefficient": {"property": "compute_units"}}],
             "op": "<=", "rhs": {"property": "compute_units"}},
            {"name": "planning-compute", "forEach": "gpus",
             "terms": [{"variable": "assign", "match": {"gpu_id": "gpu_id"},
                        "coefficient": {"property": "compute_units"}}],
             "op": "<=", "rhs": 85},
            {"name": "peak-utilization", "forEach": "gpus",
             "where": [{"property": "compute_units", "op": ">", "value": 0}],
             "terms": [
                {"variable": "assign", "match": {"gpu_id": "gpu_id"},
                 "coefficient": {"property": "compute_units"}},
                {"variable": "peak",
                 "coefficient": {"property": "compute_units", "of": "group", "scale": -1}}
             ],
             "op": "<=", "rhs": 0},
            {"name": "spread-failure-domains", "groupBy": ["workload_id", "failure_domain"],
             "terms": [{"variable": "assign",
                        "where": [{"property": "spread", "op": "==", "value": true}]}],
             "op": "<=", "rhs": 1}
        ],
        "objectives": [
            {"name": "moves", "sense": "minimize", "terms": [{"variable": "assign", "where": [
                {"inSet": "previous", "match": {"workload_id": "workload_id", "replica_index": "replica_index"}},
                {"notInSet": "previous", "match": {"workload_id": "workload_id", "replica_index": "replica_index", "gpu_id": "gpu_id"}}
            ]}]},
            {"name": "peak", "sense": "minimize", "terms": [{"variable": "peak"}]}
        ],
        "scenarios": [
            {"name": "worker", "set": "gpus", "property": "host_id", "removeFrom": ["gpus", "pairs"]},
            {"name": "region", "set": "gpus", "property": "region", "removeFrom": ["gpus", "pairs"]}
        ]
    })
}

#[derive(Clone)]
struct Gpu {
    host: &'static str,
    index: u32,
    background: u32,
}

impl Gpu {
    fn id(&self) -> String {
        format!("{}/{}", self.host, self.index)
    }
}

#[derive(Clone)]
struct Workload {
    id: &'static str,
    replicas: u32,
    memory_mib: u32,
    compute_units: u32,
}

fn profile(id: &'static str, profile: &str, replicas: u32) -> Workload {
    let (memory_mib, compute_units) = match profile {
        "assistant-v1" => (77_824, 55),
        "chat-v1" => (24_576, 30),
        "embeddings-v1" => (4_096, 20),
        "reranker-v1" => (4_096, 25),
        _ => unreachable!("unknown profile"),
    };
    Workload {
        id,
        replicas,
        memory_mib,
        compute_units,
    }
}

fn hosts(names: &[&'static str]) -> Vec<Gpu> {
    names
        .iter()
        .flat_map(|host| {
            (0..2).map(|index| Gpu {
                host,
                index,
                background: 10,
            })
        })
        .collect()
}

struct Lab {
    gpus: Vec<Gpu>,
    workloads: Vec<Workload>,
    assignments: Vec<(String, u32, String)>,
}

impl Lab {
    /// The rows the upstream queries maintain for this state.
    fn records(&self) -> BTreeMap<String, Element> {
        let mut records = BTreeMap::new();
        let mut add = |label: &str, id: String, properties: Value| {
            records.insert(id.clone(), node(label, &id, properties));
        };
        for gpu in &self.gpus {
            add(
                "Gpu",
                format!("gpu:{}", gpu.id()),
                json!({
                    "gpu_id": gpu.id(), "host_id": gpu.host, "region": "westeurope",
                    "installed_memory_mib": 81_920, "memory_mib": 81_920,
                    "compute_units": 85 - gpu.background, "eligible": true
                }),
            );
        }
        for workload in &self.workloads {
            add(
                "Workload",
                format!("workload:{}", workload.id),
                json!({"workload_id": workload.id, "replicas": workload.replicas}),
            );
            for gpu in &self.gpus {
                add(
                    "Pair",
                    format!("pair:{}:{}", workload.id, gpu.id()),
                    json!({
                        "workload_id": workload.id, "gpu_id": gpu.id(),
                        "host_id": gpu.host, "failure_domain": gpu.host, "region": "westeurope",
                        "replicas": workload.replicas, "spread": true,
                        "memory_mib": workload.memory_mib, "compute_units": workload.compute_units,
                        "gpu_memory_mib": 81_920, "gpu_compute_units": 85 - gpu.background,
                        "scheduling_enabled": true, "model_allowed": true, "policy_allowed": true
                    }),
                );
            }
        }
        for (workload, replica, gpu) in &self.assignments {
            add(
                "Assignment",
                format!("assignment:{workload}:{replica}"),
                json!({"workload_id": workload, "replica_index": replica, "gpu_id": gpu}),
            );
        }
        records
    }

    fn assign(&mut self, layout: &[(&str, u32, &str)]) {
        self.assignments = layout
            .iter()
            .map(|(workload, replica, gpu)| ((*workload).to_owned(), *replica, (*gpu).to_owned()))
            .collect();
    }
}

fn baseline() -> Lab {
    let mut lab = Lab {
        gpus: hosts(&["inference-a", "inference-b", "inference-c"]),
        workloads: vec![
            profile("assistant", "assistant-v1", 2),
            profile("chat", "chat-v1", 2),
            profile("embeddings", "embeddings-v1", 2),
            profile("reranker", "reranker-v1", 2),
        ],
        assignments: Vec::new(),
    };
    lab.assign(&[
        ("assistant", 0, "inference-a/0"),
        ("chat", 0, "inference-a/1"),
        ("assistant", 1, "inference-b/0"),
        ("embeddings", 0, "inference-b/1"),
        ("reranker", 0, "inference-b/1"),
        ("chat", 1, "inference-c/0"),
        ("reranker", 1, "inference-c/0"),
        ("embeddings", 1, "inference-c/1"),
    ]);
    lab
}

/// Sends the changes turning `current` into `next`.
async fn sync(
    optimization: &mut Optimization,
    current: &mut BTreeMap<String, Element>,
    next: BTreeMap<String, Element>,
) -> Result<()> {
    let mut changes = Vec::new();
    for id in current.keys() {
        if !next.contains_key(id) {
            changes.push(delete(id));
        }
    }
    for (id, element) in &next {
        match current.get(id) {
            None => changes.push(insert(element.clone())),
            Some(previous) if previous != element => changes.push(SourceChange::Replace {
                element: element.clone(),
            }),
            Some(_) => {}
        }
    }
    *current = next;
    send(optimization, &changes).await?;
    settle(optimization).await?;
    Ok(())
}

type Plan = Vec<(String, u32, String)>;

fn plan(rows: &Snapshot) -> Plan {
    let mut plan: Plan = labelled(rows, "Assignment")
        .into_iter()
        .map(|(_, properties)| {
            (
                properties["workload_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                properties["replica_index"].as_u64().unwrap_or_default() as u32,
                properties["gpu_id"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    plan.sort();
    plan
}

/// (assignments, moved, new) as gpu-cluster-lab reports them.
fn movement(previous: &Plan, plan: &Plan) -> (usize, usize, usize) {
    let before: BTreeMap<_, _> = previous
        .iter()
        .map(|(workload, replica, gpu)| ((workload.clone(), *replica), gpu.clone()))
        .collect();
    let moved = plan
        .iter()
        .filter(|(workload, replica, gpu)| {
            before
                .get(&(workload.clone(), *replica))
                .is_some_and(|old| old != gpu)
        })
        .count();
    let new = plan
        .iter()
        .filter(|(workload, replica, _)| !before.contains_key(&(workload.clone(), *replica)))
        .count();
    (plan.len(), moved, new)
}

/// Feasibility of each scenario value, by scenario name.
fn scenarios(rows: &Snapshot) -> BTreeMap<String, Vec<(String, Value)>> {
    let mut scenarios: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
    for (_, properties) in labelled(rows, "OptimizationScenario") {
        scenarios
            .entry(
                properties["scenario"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
            .or_default()
            .push((
                properties["value"].as_str().unwrap_or_default().to_owned(),
                properties["feasible"].clone(),
            ));
    }
    scenarios
}

/// Validates a plan the way gpu-cluster-lab's `validate_assignments` does.
fn validate(lab: &Lab, plan: &Plan) {
    for workload in &lab.workloads {
        for replica in 0..workload.replicas {
            let placed: Vec<_> = plan
                .iter()
                .filter(|(id, index, _)| id == workload.id && *index == replica)
                .collect();
            assert_eq!(placed.len(), 1, "{}/{replica} placed once", workload.id);
        }
        let mut hosts: Vec<_> = plan
            .iter()
            .filter(|(id, ..)| id == workload.id)
            .map(|(_, _, gpu)| gpu.split('/').next().unwrap_or_default())
            .collect();
        hosts.sort();
        let count = hosts.len();
        hosts.dedup();
        assert_eq!(hosts.len(), count, "{} spread across hosts", workload.id);
    }
    for gpu in &lab.gpus {
        let (memory, compute) = plan
            .iter()
            .filter(|(_, _, id)| *id == gpu.id())
            .map(|(id, ..)| {
                let workload = lab
                    .workloads
                    .iter()
                    .find(|workload| workload.id == id)
                    .expect("known workload");
                (workload.memory_mib, workload.compute_units)
            })
            .fold((0, 0), |(m, c), (dm, dc)| (m + dm, c + dc));
        assert!(
            memory <= 81_920 && compute <= 85 - gpu.background,
            "{} fits",
            gpu.id()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn gpu_lab_increased_background_demand_moves_one_replica() -> Result<()> {
    let mut lab = baseline();
    let previous = lab.assignments.clone();
    lab.gpus[0].background = 35;
    let mut optimization = started(gpu_model()).await?;
    let mut current = BTreeMap::new();
    sync(&mut optimization, &mut current, lab.records()).await?;
    let rows = snapshot(&optimization).await?;
    assert_eq!(status(&rows)["status"], "optimal", "{:?}", status(&rows));
    let first = plan(&rows);
    validate(&lab, &first);
    assert_eq!(movement(&previous, &first), (8, 1, 0));
    assert_eq!(status(&rows)["objectives"]["moves"], 1.0);
    // The assistant replica no longer fits the loaded GPU, so it is the one that moves.
    assert!(!first.contains(&("assistant".into(), 0, "inference-a/0".into())));

    // Restored capacity with the new plan as the current one: no moves.
    lab.gpus[0].background = 10;
    lab.assignments = first.clone();
    sync(&mut optimization, &mut current, lab.records()).await?;
    let rows = snapshot(&optimization).await?;
    let after = plan(&rows);
    assert_eq!(movement(&first, &after), (8, 0, 0));
    assert_eq!(after, first);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn gpu_lab_fragmentation_moves_one_replica_for_a_new_workload() -> Result<()> {
    let mut lab = Lab {
        gpus: hosts(&["inference-a", "inference-b", "inference-c"]),
        workloads: vec![
            profile("chat-alpha", "chat-v1", 2),
            profile("chat-bravo", "chat-v1", 2),
            profile("chat-charlie", "chat-v1", 2),
            profile("document-assistant", "assistant-v1", 1),
        ],
        assignments: Vec::new(),
    };
    lab.assign(&[
        ("chat-alpha", 0, "inference-a/0"),
        ("chat-bravo", 0, "inference-a/1"),
        ("chat-alpha", 1, "inference-b/0"),
        ("chat-charlie", 0, "inference-b/1"),
        ("chat-bravo", 1, "inference-c/0"),
        ("chat-charlie", 1, "inference-c/1"),
    ]);
    let mut optimization = started(gpu_model()).await?;
    sync(&mut optimization, &mut BTreeMap::new(), lab.records()).await?;
    let rows = snapshot(&optimization).await?;
    let result = plan(&rows);
    validate(&lab, &result);
    assert_eq!(movement(&lab.assignments, &result), (7, 1, 1));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn gpu_lab_resilience_distinguishes_baseline_and_added_load() -> Result<()> {
    let mut lab = baseline();
    let mut optimization = started(gpu_model()).await?;
    let mut current = BTreeMap::new();
    sync(&mut optimization, &mut current, lab.records()).await?;
    let rows = snapshot(&optimization).await?;
    let result = scenarios(&rows);
    assert_eq!(
        result["worker"],
        ["inference-a", "inference-b", "inference-c"].map(|host| (host.to_owned(), json!(true)))
    );
    assert_eq!(result["region"], [("westeurope".to_owned(), json!(false))]);
    let baseline_plan = plan(&rows);
    assert_eq!(movement(&lab.assignments, &baseline_plan), (8, 0, 0));
    let signature = status(&rows)["signature"].clone();
    assert!(labelled(&rows, "OptimizationScenario")
        .iter()
        .all(|(_, properties)| properties["signature"] == signature));

    // Added load: the plan is still feasible, but no worker loss is survivable.
    lab.workloads.push(profile("tenant-chat", "chat-v1", 2));
    sync(&mut optimization, &mut current, lab.records()).await?;
    let rows = snapshot(&optimization).await?;
    assert_eq!(status(&rows)["status"], "optimal");
    validate(&lab, &plan(&rows));
    let result = scenarios(&rows);
    assert_eq!(result["worker"].len(), 3);
    assert!(result["worker"]
        .iter()
        .all(|(_, feasible)| *feasible == json!(false)));

    // A fourth worker restores survivability of every single-worker loss.
    lab.gpus.extend(hosts(&["inference-d"]));
    sync(&mut optimization, &mut current, lab.records()).await?;
    let rows = snapshot(&optimization).await?;
    let result = scenarios(&rows);
    assert_eq!(result["worker"].len(), 4);
    assert!(result["worker"]
        .iter()
        .all(|(_, feasible)| *feasible == json!(true)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn scenarios_beyond_the_limit_are_reported_unsolved() -> Result<()> {
    let mut configuration = gpu_model();
    configuration["maxScenarios"] = json!(2);
    let mut optimization = started(configuration).await?;
    sync(
        &mut optimization,
        &mut BTreeMap::new(),
        baseline().records(),
    )
    .await?;
    let rows = snapshot(&optimization).await?;
    let statuses: Vec<_> = labelled(&rows, "OptimizationScenario")
        .into_iter()
        .map(|(id, properties)| {
            (
                id.to_owned(),
                properties["status"].clone(),
                properties["feasible"].clone(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        [
            (
                "scenario/region/westeurope".to_owned(),
                json!("limit"),
                Value::Null
            ),
            (
                "scenario/worker/inference-a".to_owned(),
                json!("optimal"),
                json!(true)
            ),
            (
                "scenario/worker/inference-b".to_owned(),
                json!("optimal"),
                json!(true)
            ),
            (
                "scenario/worker/inference-c".to_owned(),
                json!("limit"),
                Value::Null
            ),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn output_identities_escape_key_separators() -> Result<()> {
    let mut optimization = started(knapsack(10.0)).await?;
    send(&mut optimization, &[insert(item("x/y%", 1.0, 1.0))]).await?;
    settle(&mut optimization).await?;
    let rows = snapshot(&optimization).await?;
    assert_eq!(rows["take/x%2Fy%25"].1["name"], "x/y%");
    Ok(())
}

mod placement;
