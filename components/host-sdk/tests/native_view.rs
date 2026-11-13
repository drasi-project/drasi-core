// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Build `drasi-computation-transformer-view --features dynamic-plugin` first.
//! This test loads the real library; it has no Rust dependency on the view crate.

#[path = "fixtures/native_sink_inputs.rs"]
mod native_inputs;

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ComponentId, ElementImage, GraphChangeCodec, GraphProducerProgress, InputEnvelope,
    OutputCursor, OutputProgress, OutputReadError, OutputSnapshot, PortId, SnapshotProvider,
    StreamId,
};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_host_sdk::computation::{self, NativeComponentProxy, NativePlugin};
use drasi_lib::computation::v1::{ComputationComponent, Transformer};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc, time::Duration};

const PACKAGE: &str = "drasi-computation-transformer-view";
const FACTORY: &str = "drasi.standard/view";
const VIEW_ID: &str = "f178dc35-4d59-52b1-bf05-c7168dd497a1";

fn load() -> Result<Arc<NativePlugin>> {
    let directory = match std::env::var_os("DRASI_NATIVE_PLUGIN_DIR") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .context("test executable directory")?
            .parent()
            .context("native library directory")?
            .join("plugins"),
    };
    let library = directory.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        PACKAGE.replace('-', "_"),
        std::env::consts::DLL_SUFFIX,
    ));
    anyhow::ensure!(
        library.is_file(),
        "missing {}; first run make build-test-plugins (or set DRASI_NATIVE_PLUGIN_DIR)",
        library.display()
    );
    computation::load(&library).with_context(|| format!("load {}", library.display()))
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

fn relation(target: &str, time: u64) -> Element {
    Element::Relation {
        metadata: node("edge", "ViewEdge", time, json!({}))
            .get_metadata()
            .clone(),
        in_node: ElementReference::new("source", "parent"),
        out_node: ElementReference::new("source", target),
        properties: ElementPropertyMap::from(json!({"weight": 3})),
    }
}

fn input(change: SourceChange, sequence: u64) -> Result<InputEnvelope> {
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_change(
            change,
            StreamId::try_new("source/out")?,
            sequence,
            None,
        )?,
    })
}

fn snapshot_provider(view: &NativeComponentProxy) -> Result<Arc<dyn SnapshotProvider>> {
    let readers = view.output_readers();
    readers.validate(view.descriptor())?;
    assert_eq!(readers.ports().len(), 1);
    let output = readers
        .ports()
        .get(&PortId::try_new("out")?)
        .context("Host did not expose the advertised view output")?;
    assert!(output.replay().is_none());
    assert!(output.snapshot_follow().is_none());
    output
        .snapshot()
        .cloned()
        .context("Host did not expose the advertised view snapshot provider")
}

fn assert_snapshot(snapshot: &OutputSnapshot, expected: &[Element], sequence: u64) -> Result<()> {
    assert_eq!(snapshot.schema(), GraphChangeCodec::schema().descriptor());
    assert_eq!(snapshot.dataset(), VIEW_ID);
    assert_eq!(snapshot.cursor().sequence(), sequence);
    assert_eq!(
        snapshot.cursor().producer().component_id().as_str(),
        VIEW_ID
    );
    assert_eq!(snapshot.cursor().producer().stream().as_str(), "view/out");
    assert!(!snapshot.cursor().producer().persistent());
    let expected = expected
        .iter()
        .map(|element| {
            let record = GraphChangeCodec::element_record(&ElementImage::try_from(element)?)?;
            Ok((record.identity().clone(), record))
        })
        .collect::<Result<im::HashMap<_, _>>>()?;
    assert_eq!(snapshot.records(), &expected);
    Ok(())
}

async fn process(
    view: &mut NativeComponentProxy,
    change: SourceChange,
    sequence: u64,
) -> Result<(Vec<SourceChange>, OutputCursor)> {
    let mut output = view.transform(input(change, sequence)?).await?;
    assert_eq!(output.len(), 1);
    let output = output.pop().context("missing view output")?;
    assert_eq!(output.port.as_str(), "out");
    assert_eq!(output.envelope.system().stream().as_str(), "view/out");
    assert_eq!(output.envelope.system().sequence(), sequence);
    let progress = GraphProducerProgress::from_envelope(&output.envelope)?
        .context("view output lost its producer progress across the native boundary")?;
    assert!(!progress.identity().persistent());
    assert_eq!(progress.sequence(), sequence);
    Ok((
        GraphChangeCodec::decode_changes(&output.envelope)?,
        OutputCursor::try_new(progress.identity().clone(), progress.sequence())?,
    ))
}

async fn loaded_view_case() -> Result<()> {
    let plugin = load()?;
    assert_eq!(plugin.metadata().plugin.id.as_ref(), PACKAGE);
    assert_eq!(plugin.factories().len(), 1);
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == FACTORY)
        .context("view factory missing from the actual native library")?;
    let capabilities = &factory.metadata().capabilities;
    assert_eq!(capabilities.output_snapshots, vec![PortId::try_new("out")?]);
    assert_eq!(capabilities.output_progress, Some(OutputProgress::Origin));
    assert!(capabilities.delete_owned_resources);
    assert!(!capabilities.transactional);
    assert!(!capabilities.durable_output);
    assert!(capabilities.output_replays.is_empty());
    assert!(capabilities.output_follows.is_empty());

    let configuration = json!({
        "stream": "view/out",
        "middleware": [{
            "name": "project",
            "kind": "map",
            "config": {"Parent": {"insert": [
                {
                    "id": "$.key", "label": "ViewNode",
                    "properties": {"value": "$.value", "extra": "$.extra"}
                },
                {
                    "id": "$.edge", "label": "ViewEdge",
                    "elementType": {"Relation": {"inNodeId": "$.from", "outNodeId": "$.key"}},
                    "properties": {"weight": "$.weight"}
                }
            ]}}
        }],
        "pipeline": ["project"]
    });
    let mut view = native_inputs::create(
        factory,
        ComponentId::try_new(VIEW_ID)?,
        configuration.clone(),
    )?;
    assert_eq!(view.configuration()?, configuration);
    let snapshots = snapshot_provider(&view)?;
    assert!(matches!(
        snapshots.snapshot().await,
        Err(OutputReadError::NotReady(_))
    ));
    view.initialize().await?;
    view.start().await?;
    assert_snapshot(&snapshots.snapshot().await?, &[], 0)?;

    let full_input = node(
        "input",
        "Parent",
        1,
        json!({
            "key": "child-a", "value": 7, "extra": "retained",
            "edge": "edge", "from": "parent", "weight": 3
        }),
    );
    let first_rows = [
        node(
            "child-a",
            "ViewNode",
            1,
            json!({"value": 7, "extra": "retained"}),
        ),
        relation("child-a", 1),
    ];
    let (added, first_cursor) = process(
        &mut view,
        SourceChange::Insert {
            element: full_input,
        },
        1,
    )
    .await?;
    assert_eq!(
        added,
        first_rows
            .iter()
            .cloned()
            .map(|element| SourceChange::Insert { element })
            .collect::<Vec<_>>()
    );
    let first = snapshots.snapshot().await?;
    assert_snapshot(&first, &first_rows, 1)?;
    assert_eq!(first.cursor(), &first_cursor);

    let next_rows = [
        node("child-b", "ViewNode", 2, json!({"value": null})),
        relation("child-b", 2),
    ];
    let (patched, second_cursor) = process(
        &mut view,
        SourceChange::Update {
            element: node(
                "input",
                "Parent",
                2,
                json!({"key": "child-b", "value": null}),
            ),
            removed_properties: ["extra".into()].into(),
        },
        2,
    )
    .await?;
    assert_eq!(patched.len(), 3);
    assert_eq!(
        patched[0],
        SourceChange::Delete {
            metadata: node("child-a", "ViewNode", 2, json!({}))
                .get_metadata()
                .clone(),
        }
    );
    assert!(patched.contains(&SourceChange::Insert {
        element: next_rows[0].clone()
    }));
    assert!(patched.contains(&SourceChange::Replace {
        element: next_rows[1].clone()
    }));
    let second = snapshots.snapshot().await?;
    assert_snapshot(&second, &next_rows, 2)?;
    assert_eq!(second.cursor(), &second_cursor);
    assert_eq!(first_cursor.epoch(), second_cursor.epoch());
    assert_snapshot(&first, &first_rows, 1)?;

    let deletion = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "input"),
            labels: Arc::from([]),
            effective_from: 3,
        },
    };
    view.stop().await?;
    assert!(matches!(
        snapshots.snapshot().await,
        Err(OutputReadError::NotReady(_))
    ));
    assert!(view.transform(input(deletion.clone(), 3)?).await.is_err());
    view.start().await?;
    let resumed = snapshots.snapshot().await?;
    assert_snapshot(&resumed, &next_rows, 2)?;
    assert_eq!(resumed.cursor(), second.cursor());

    let (deleted, third_cursor) = process(&mut view, deletion, 3).await?;
    assert_eq!(deleted.len(), 2);
    for mut element in next_rows {
        element.update_effective_time(3);
        assert!(deleted.contains(&SourceChange::Delete {
            metadata: element.get_metadata().clone(),
        }));
    }
    let empty = snapshots.snapshot().await?;
    assert_snapshot(&empty, &[], 3)?;
    assert_eq!(empty.cursor(), &third_cursor);

    process(
        &mut view,
        SourceChange::Insert {
            element: node(
                "input",
                "Parent",
                4,
                json!({"key": "child-c", "edge": "edge", "from": "parent", "weight": 3}),
            ),
        },
        4,
    )
    .await?;
    let retained = snapshots.snapshot().await?;
    assert_eq!(retained.records().len(), 2);
    assert!(view.delete_owned_resources().await.is_err());
    assert_eq!(snapshots.snapshot().await?.cursor(), retained.cursor());
    view.stop().await?;
    assert!(matches!(
        snapshots.snapshot().await,
        Err(OutputReadError::NotReady(_))
    ));
    view.delete_owned_resources().await?;
    view.delete_owned_resources().await?;
    assert!(matches!(
        snapshots.snapshot().await,
        Err(OutputReadError::Retired)
    ));
    assert!(view.start().await.is_err());
    assert!(view
        .transform(input(
            SourceChange::Insert {
                element: node("input", "Parent", 5, json!({"key": "cannot-return"})),
            },
            5
        )?)
        .await
        .is_err());
    view.dispose().await?;
    drop(view);
    assert!(matches!(
        snapshots.snapshot().await,
        Err(OutputReadError::Retired)
    ));

    let mut fresh = native_inputs::create(factory, ComponentId::try_new(VIEW_ID)?, configuration)?;
    let fresh_snapshots = snapshot_provider(&fresh)?;
    fresh.initialize().await?;
    fresh.start().await?;
    let cold = fresh_snapshots.snapshot().await?;
    assert_snapshot(&cold, &[], 0)?;
    assert_ne!(cold.cursor().epoch(), first_cursor.epoch());
    fresh.stop().await?;
    fresh.delete_owned_resources().await?;
    fresh.dispose().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_view_transforms_and_snapshots_obey_host_lifecycle_fences() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), loaded_view_case())
        .await
        .context("native view Host-boundary proof timed out")?
}
