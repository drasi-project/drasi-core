// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Initialization from upstream output snapshots.

use super::*;
use drasi_computation_contracts::{
    ElementImage, InputBinding, OutputCursor, OutputEndpoint, Record,
};
use drasi_computation_plugin_sdk::SnapshotSource;

const UPSTREAM: &str = "5d7e0f4b-2a64-4bb0-9a3c-6f1e2d3c4b5a";

/// An upstream output whose snapshot the test controls.
struct Upstream {
    snapshot: std::sync::Mutex<(OutputCursor, Vec<Element>)>,
}

#[async_trait]
impl SnapshotSource for Upstream {
    async fn read(&self, output: &OutputEndpoint) -> Result<(OutputCursor, Vec<Record>)> {
        assert_eq!(output.component.as_str(), UPSTREAM);
        let (cursor, elements) = self.snapshot.lock().expect("snapshot").clone();
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
        output: OutputEndpoint::new(ComponentId::try_new(UPSTREAM)?, PortId::try_new("out")?),
        stream: StreamId::try_new("source/out")?,
    })
}

fn set(
    upstream: &Upstream,
    producer: &GraphProducerIdentity,
    sequence: u64,
    elements: Vec<Element>,
) -> Result<()> {
    *upstream.snapshot.lock().expect("snapshot") =
        (OutputCursor::try_new(producer.clone(), sequence)?, elements);
    Ok(())
}

async fn start_with(upstream: Arc<Upstream>) -> Result<Optimization> {
    let mut optimization = Optimization::new(
        OptimizationFactory
            .metadata()
            .descriptor(ComponentId::try_new(ID)?)?,
        OptimizationConfig::parse(knapsack(10.0))?,
        None,
        &[binding()?],
        Some(upstream),
    )?;
    optimization.start(&mut contexts::start()).await?;
    Ok(optimization)
}

/// A live envelope from `producer` at `sequence`.
async fn live(
    optimization: &mut Optimization,
    producer: &GraphProducerIdentity,
    sequence: u64,
    changes: &[SourceChange],
) -> Result<()> {
    let mut envelope = GraphChangeCodec::encode_changes(
        changes,
        StreamId::try_new("source/out")?,
        sequence,
        None,
    )?;
    GraphProducerProgress::annotate(&mut envelope, producer, sequence)?;
    let outputs = optimization
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope,
        })
        .await?;
    assert!(outputs.is_empty());
    Ok(())
}

fn elements() -> Vec<Element> {
    vec![
        item("a", 5.0, 10.0),
        item("b", 4.0, 40.0),
        item("c", 6.0, 30.0),
        item("d", 3.0, 50.0),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn solves_from_upstream_snapshots_and_skips_covered_input() -> Result<()> {
    let producer = new_producer()?;
    let upstream = Arc::new(Upstream {
        snapshot: std::sync::Mutex::new((OutputCursor::try_new(producer.clone(), 0)?, vec![])),
    });
    set(&upstream, &producer, 5, elements())?;
    let mut optimization = start_with(upstream.clone()).await?;

    // The snapshot alone is solved and published, before any live input.
    let batches = settle(&mut optimization).await?;
    assert_eq!(batches.len(), 1);
    let rows = snapshot(&optimization).await?;
    assert_eq!(status(&rows)["status"], "optimal");
    assert_eq!(taken(&rows), ["b", "d"]);

    // An envelope the snapshot already covers changes nothing.
    live(
        &mut optimization,
        &producer,
        4,
        &[insert(item("a", 1.0, 1000.0))],
    )
    .await?;
    assert!(settle(&mut optimization).await?.is_empty());

    // A later one is applied and re-solved.
    live(
        &mut optimization,
        &producer,
        6,
        &[insert(item("a", 1.0, 1000.0))],
    )
    .await?;
    settle(&mut optimization).await?;
    assert_eq!(taken(&snapshot(&optimization).await?), ["a", "b", "d"]);

    // Stop and start keep the position: nothing is read again.
    optimization.stop(&contexts::stop(StopMode::Drain)).await?;
    set(&upstream, &producer, 9, vec![])?;
    optimization.start(&mut contexts::start()).await?;
    settle(&mut optimization).await?;
    assert_eq!(taken(&snapshot(&optimization).await?), ["a", "b", "d"]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_an_upstream_again_when_it_restarts() -> Result<()> {
    let producer = new_producer()?;
    let upstream = Arc::new(Upstream {
        snapshot: std::sync::Mutex::new((OutputCursor::try_new(producer.clone(), 0)?, vec![])),
    });
    set(&upstream, &producer, 3, elements())?;
    let mut optimization = start_with(upstream.clone()).await?;
    settle(&mut optimization).await?;
    assert_eq!(taken(&snapshot(&optimization).await?), ["b", "d"]);

    // A new producer epoch whose snapshot no longer has `d`.
    let restarted = new_producer()?;
    set(
        &upstream,
        &restarted,
        1,
        elements()
            .into_iter()
            .filter(|e| e.get_reference().element_id.as_ref() != "d")
            .collect(),
    )?;
    live(
        &mut optimization,
        &restarted,
        2,
        &[insert(item("e", 1.0, 1.0))],
    )
    .await?;
    settle(&mut optimization).await?;
    let rows = snapshot(&optimization).await?;
    assert_eq!(taken(&rows), ["b", "c"]);
    assert_eq!(status(&rows)["members"], 4);
    Ok(())
}
