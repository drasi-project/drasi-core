// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![allow(clippy::print_stdout)]

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::computation::v1::*;
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

fn descriptor(id: &str, output: bool) -> Result<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        ComponentId::try_new(id)?,
        vec![PortDescriptor::new(
            PortId::try_new(if output { "out" } else { "in" })?,
            if output {
                PortDirection::Output
            } else {
                PortDirection::Input
            },
            if output {
                GraphChangeCodec::schema()
            } else {
                QueryChangeCodec::schema()
            }
            .descriptor()
            .clone(),
            PipeRequirements::default(),
        )],
    )?)
}

fn endpoint(id: &str, port: &str) -> Result<Endpoint> {
    Ok(Endpoint::new(
        ComponentId::try_new(id)?,
        PortId::try_new(port)?,
    ))
}

fn event(source: &str, key: &str, sequence: u64, timer: bool) -> Result<ChangeEnvelope> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, key),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: 1000,
        },
        properties: ElementPropertyMap::from(
            serde_json::json!({"producer":source, "value":sequence}),
        ),
    };
    Ok(GraphChangeCodec::encode_change(
        if timer || sequence == 1 {
            SourceChange::Insert { element }
        } else {
            SourceChange::Update { element }
        },
        StreamId::try_new(format!("{source}/out"))?,
        sequence,
        None,
    )?)
}

struct ReadySource {
    descriptor: ComponentDescriptor,
    produced: Arc<AtomicU64>,
}
#[async_trait]
impl ComputationComponent for ReadySource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for ReadySource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let sequence = self.produced.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: event(self.descriptor.id().as_str(), "live", sequence, false)?,
        }))
    }
}

struct Counter {
    descriptor: ComponentDescriptor,
    count: Arc<AtomicU64>,
    producers: Option<Arc<Mutex<BTreeMap<String, u64>>>>,
}
#[async_trait]
impl ComputationComponent for Counter {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for Counter {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        if let Some(producers) = &self.producers {
            let output = QueryChangeCodec::to_legacy_result(&input.envelope)?;
            let mut producers = producers.lock().expect("producer observations");
            for result in output.results {
                let row = match result {
                    drasi_lib::channels::ResultDiff::Add { data, .. }
                    | drasi_lib::channels::ResultDiff::Update { after: data, .. } => data,
                    other => anyhow::bail!("unexpected live result: {other:?}"),
                };
                let producer = row["producer"]
                    .as_str()
                    .context("missing result producer")?;
                *producers.entry(producer.to_owned()).or_default() += 1;
            }
        }
        self.count.fetch_add(1, Ordering::Release);
        Ok(())
    }
}

async fn query(id: &str, timer: bool) -> Result<ContinuousQueryTransformer> {
    ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "responsiveness".into(),
            id: ComponentId::try_new(id)?,
            query: if timer {
                "MATCH (n:Item) WHERE drasi.trueLater(true, 2000) RETURN n.value AS value"
            } else {
                "MATCH (n:Item) RETURN n.producer AS producer, n.value AS value"
            }
            .into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new(format!("{id}/out"))?,
            outbox_capacity: NonZeroUsize::new(64).expect("nonzero capacity"),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await
}

async fn responsiveness(runtime: &str) -> Result<()> {
    let mut measurements = Vec::new();
    for _ in 0..8 {
        let scheduling = Arc::new(QuerySchedulingResource::default());
        let mut timers = query("timers", true)
            .await?
            .with_scheduling(scheduling.clone())?;
        timers.start().await?;
        for sequence in 1..=4096 {
            ensure!(
                timers
                    .transform(InputEnvelope {
                        port: PortId::try_new("in")?,
                        envelope: event("timer-input", &sequence.to_string(), sequence, true)?,
                    })
                    .await?
                    .is_empty(),
                "timer fired during setup"
            );
        }
        let live_count = Arc::new(AtomicU64::new(0));
        let timer_count = Arc::new(AtomicU64::new(0));
        let processed = Arc::new(Mutex::new(BTreeMap::<String, u64>::new()));
        let mut builder = ComputationGraph::builder("responsiveness")
            .query(Box::new(query("live", false).await?))
            .query(Box::new(timers))
            .source(Box::new(QueryScheduledSource::new(
                ComponentId::try_new("clock")?,
                StreamId::try_new("clock/out")?,
                scheduling,
            )?))
            .bind_stream(endpoint("clock", "out")?, StreamId::try_new("clock/out")?)
            .connect(
                EdgeDefinition::new(endpoint("clock", "out")?, endpoint("timers", "in")?),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
        let mut producers = Vec::new();
        for index in 0..8 {
            let id = format!("source-{index}");
            let count = Arc::new(AtomicU64::new(0));
            builder = builder
                .source(Box::new(ReadySource {
                    descriptor: descriptor(&id, true)?,
                    produced: count.clone(),
                }))
                .bind_stream(
                    endpoint(&id, "out")?,
                    StreamId::try_new(format!("{id}/out"))?,
                )
                .connect(
                    EdgeDefinition::new(endpoint(&id, "out")?, endpoint("live", "in")?),
                    Box::new(BoundedPipeConfig { capacity: 64 }),
                );
            producers.push(count);
        }
        for (id, count) in [("live", live_count.clone()), ("timers", timer_count.clone())] {
            let sink = format!("{id}-sink");
            builder = builder
                .sink(Box::new(Counter {
                    descriptor: descriptor(&sink, false)?,
                    count,
                    producers: (id == "live").then(|| processed.clone()),
                }))
                .bind_stream(
                    endpoint(id, "out")?,
                    StreamId::try_new(format!("{id}/out"))?,
                )
                .connect(
                    EdgeDefinition::new(endpoint(id, "out")?, endpoint(&sink, "in")?),
                    Box::new(BoundedPipeConfig { capacity: 64 }),
                );
        }
        // The timer query is intentionally driven by its own wakeup path, not a second evaluator.
        let mut graph = builder.build()?;
        let run = graph.start()?;
        let control = run.control();
        let (running, result) = tokio::join!(run, async {
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                while live_count.load(Ordering::Acquire) < 256
                    || timer_count.load(Ordering::Acquire) < 16
                {
                    tokio::task::yield_now().await;
                }
                ensure!(
                    producers
                        .iter()
                        .all(|count| count.load(Ordering::Acquire) > 0),
                    "a ready producer was starved"
                );
                {
                    let processed = processed.lock().expect("producer observations");
                    ensure!(
                        (0..8).all(|index| processed
                            .get(&format!("source-{index}"))
                            .copied()
                            .unwrap_or(0)
                            > 0),
                        "query results starved a producer: {processed:?}"
                    );
                }
                let live = live_count.load(Ordering::Acquire);
                let timers = timer_count.load(Ordering::Acquire);
                ensure!(timers < 4096, "live work waited for timer exhaustion");
                let before = Instant::now();
                let stopped = control
                    .stop_components(GraphRevision(1), GraphSelection::All)
                    .await?;
                let elapsed = before.elapsed();
                ensure!(
                    stopped.summary == OperationSummary::Completed,
                    "stop failed"
                );
                ensure!(
                    elapsed < Duration::from_secs(2),
                    "stop exceeded responsiveness guard: {elapsed:?}"
                );
                Ok::<_, anyhow::Error>((live, timers, elapsed.as_nanos()))
            })
            .await;
            control.cancel();
            result
        });
        graph.dispose().await?;
        ensure!(
            matches!(running, Ok(()) | Err(GraphError::Cancelled)),
            "graph failed: {running:?}"
        );
        let (live, timers, stop_ns) = result??;
        measurements.push(
            serde_json::json!({"live_outputs":live, "timer_outputs":timers, "stop_ns":stop_ns}),
        );
    }
    println!(
        "{}",
        serde_json::json!({
            "runtime":runtime, "producers":8, "scheduled_per_trial":4096,
            "stop_guard_ns":2_000_000_000_u64, "trials":measurements,
        })
    );
    Ok(())
}

#[tokio::test]
async fn ready_queries_and_timer_bursts_remain_responsive_current_thread() -> Result<()> {
    responsiveness("current-thread").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_queries_and_timer_bursts_remain_responsive_multi_thread() -> Result<()> {
    responsiveness("multi-thread").await
}
