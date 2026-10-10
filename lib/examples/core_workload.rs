// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! In-process query/fan-out measurement with an independent per-query result oracle.
//! Arguments: events window queries payload-bytes ordinary|native
//! current-thread|multi-thread projection|aggregate|join|persistent-aggregate.

#![allow(clippy::print_stdout)]

use std::{
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::{ComputationIndexProvider, InMemoryComputationProvider},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    computation::v1::*,
    config::{QueryConfig, QueryJoinConfig, QueryJoinKeyConfig},
    DrasiLib, Query,
};
use drasi_reaction_application::ApplicationReaction;
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const KEYS: usize = 64;
const WARMUP: usize = 2_048;

struct Options {
    events: usize,
    window: usize,
    queries: usize,
    payload_bytes: usize,
    execution: String,
    runtime: String,
    workload: String,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        ensure!(
            args.len() == 7,
            "expected events window queries payload-bytes execution runtime workload"
        );
        let options = Self {
            events: args[0].parse()?,
            window: args[1].parse()?,
            queries: args[2].parse()?,
            payload_bytes: args[3].parse()?,
            execution: args[4].clone(),
            runtime: args[5].clone(),
            workload: args[6].clone(),
        };
        ensure!(
            (1..=10_000_000).contains(&options.events),
            "events must be 1..10000000"
        );
        ensure!((1..=64).contains(&options.window), "window must be 1..64");
        ensure!((1..=64).contains(&options.queries), "queries must be 1..64");
        ensure!(
            options.payload_bytes <= 1_048_576,
            "payload must not exceed 1048576 bytes"
        );
        ensure!(
            matches!(options.execution.as_str(), "ordinary" | "native"),
            "unknown execution"
        );
        ensure!(
            matches!(options.runtime.as_str(), "current-thread" | "multi-thread"),
            "unknown runtime"
        );
        ensure!(
            matches!(
                options.workload.as_str(),
                "projection" | "aggregate" | "join" | "persistent-aggregate"
            ),
            "unknown workload"
        );
        ensure!(
            options.workload != "persistent-aggregate" || options.execution == "native",
            "persistent-aggregate requires native execution and computation-rocksdb-tests"
        );
        Ok(options)
    }

    fn query(&self) -> &'static str {
        if self.is_aggregate() {
            "MATCH (n:Bench) RETURN sum(n.value) AS value"
        } else if self.workload == "join" {
            "MATCH (n:Bench)-[:FACTOR]->(r:Reference) RETURN n.value * r.factor AS value, n.payload AS payload"
        } else {
            "MATCH (n:Bench) RETURN n.value AS value, n.payload AS payload"
        }
    }

    fn is_aggregate(&self) -> bool {
        matches!(self.workload.as_str(), "aggregate" | "persistent-aggregate")
    }

    fn query_config(&self, id: &str) -> QueryConfig {
        let mut query = Query::cypher(id)
            .query(self.query())
            .from_source("input")
            .enable_bootstrap(false);
        if self.workload == "join" {
            query = query.with_joins(vec![QueryJoinConfig {
                id: "FACTOR".into(),
                keys: ["Bench", "Reference"]
                    .into_iter()
                    .map(|label| QueryJoinKeyConfig {
                        label: label.into(),
                        property: "key".into(),
                    })
                    .collect(),
            }]);
        }
        query.build()
    }
}

enum Input {
    Ordinary(ApplicationSourceHandle),
    Native(mpsc::Sender<SourceChange>),
}

impl Input {
    async fn send(&self, change: SourceChange) -> Result<()> {
        match self {
            Self::Ordinary(input) => input.send(change).await,
            Self::Native(input) => input.send(change).await.context("native input closed"),
        }
    }
}

fn change(sequence: usize, payload: &str, join: bool) -> SourceChange {
    let mut properties = json!({ "value": sequence + 1, "payload": payload });
    if join {
        properties["key"] = json!(sequence % KEYS);
    }
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("input", &format!("key-{}", sequence % KEYS)),
            labels: Arc::from([Arc::from("Bench")]),
            effective_from: sequence as u64 + 1,
        },
        properties: ElementPropertyMap::from(properties),
    };
    if sequence < KEYS {
        SourceChange::Insert { element }
    } else {
        SourceChange::Update { element }
    }
}

async fn exercise(
    options: &Options,
    input: Input,
    mut output: mpsc::Receiver<QueryResult>,
) -> Result<Value> {
    let payload = "x".repeat(options.payload_bytes);
    if options.workload == "join" {
        let seed = async {
            for key in 0..KEYS {
                input
                    .send(SourceChange::Insert {
                        element: Element::Node {
                            metadata: ElementMetadata {
                                reference: ElementReference::new(
                                    "input",
                                    &format!("reference-{key}"),
                                ),
                                labels: Arc::from([Arc::from("Reference")]),
                                effective_from: 1,
                            },
                            properties: ElementPropertyMap::from(
                                json!({ "key": key, "factor": 2 }),
                            ),
                        },
                    })
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::pin!(seed);
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                tokio::select! {
                    result = &mut seed => break result,
                    result = output.recv() => {
                        ensure!(result.context("output closed during join setup")?.results.is_empty(), "unmatched reference produced a result");
                    }
                }
            }
        }).await.context("join initialization stalled")??;
    }
    let mut values = [0_u64; KEYS];
    let mut sum = 0_u64;
    let mut sequence = 0;
    let mut latencies = Vec::with_capacity(options.events);
    let mut seconds = 0.0;
    for (count, measured) in [(WARMUP, false), (options.events, true)] {
        let started = Instant::now();
        let mut processed = 0;
        while processed < count {
            let batch = options.window.min(count - processed);
            let mut starts = Vec::with_capacity(batch);
            let mut expected = Vec::with_capacity(batch);
            let mut remaining = vec![options.queries; batch];
            let mut positions = vec![0; options.queries];
            for _ in 0..batch {
                let key = sequence % KEYS;
                sum -= values[key];
                values[key] = sequence as u64 + 1;
                sum += values[key];
                expected.push(if options.is_aggregate() {
                    sum
                } else if options.workload == "join" {
                    values[key] * 2
                } else {
                    values[key]
                });
                starts.push(Instant::now());
                tokio::time::timeout(
                    Duration::from_secs(30),
                    input.send(change(sequence, &payload, options.workload == "join")),
                )
                .await
                .context("input admission stalled")??;
                sequence += 1;
            }
            let mut returned = 0;
            while returned < batch * options.queries {
                let result = tokio::time::timeout(Duration::from_secs(30), output.recv())
                    .await
                    .context("output stalled")?
                    .context("output closed")?;
                let query: usize = result
                    .query_id
                    .strip_prefix("query-")
                    .context("unexpected query ID")?
                    .parse()?;
                ensure!(query < options.queries, "unexpected query index");
                if options.workload == "join" && result.results.is_empty() {
                    continue;
                }
                returned += 1;
                let position = positions[query];
                ensure!(position < batch, "duplicate output for {}", result.query_id);
                let row = match result.results.as_slice() {
                    [ResultDiff::Add { data, .. }] => data,
                    [ResultDiff::Update { after, .. } | ResultDiff::Aggregation { after, .. }] => {
                        after
                    }
                    results => anyhow::bail!("expected exactly one changed row, got {results:?}"),
                };
                ensure!(
                    row["value"].as_f64() == Some(expected[position] as f64),
                    "{} output mismatch at input {}: {row}",
                    result.query_id,
                    sequence - batch + position,
                );
                if !options.is_aggregate() {
                    ensure!(
                        row["payload"].as_str() == Some(payload.as_str()),
                        "payload mismatch"
                    );
                }
                positions[query] += 1;
                remaining[position] -= 1;
                if measured && remaining[position] == 0 {
                    latencies.push(starts[position].elapsed().as_nanos() as u64);
                }
            }
            ensure!(
                positions.iter().all(|position| *position == batch),
                "missing query output"
            );
            ensure!(
                matches!(output.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                "unexpected extra output"
            );
            processed += batch;
        }
        if measured {
            seconds = started.elapsed().as_secs_f64();
        }
    }
    ensure!(latencies.len() == options.events, "incomplete measurement");
    latencies.sort_unstable();
    Ok(json!({
        "events": options.events,
        "window": options.window,
        "queries": options.queries,
        "payload_bytes": options.payload_bytes,
        "execution": options.execution,
        "runtime": options.runtime,
        "workers": if options.runtime == "multi-thread" { 2 } else { 1 },
        "workload": options.workload,
        "keys": KEYS,
        "warmup_events": WARMUP,
        "verified_results": options.events * options.queries,
        "seconds": seconds,
        "events_per_second": options.events as f64 / seconds,
        "query_evaluations_per_second": (options.events * options.queries) as f64 / seconds,
        "latency_p50_ns": latencies[(options.events - 1) / 2],
        "latency_p99_ns": latencies[(options.events - 1) * 99 / 100],
    }))
}

async fn ordinary(options: &Options) -> Result<Value> {
    let (source, input) = ApplicationSource::new(
        "input",
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?;
    let ids: Vec<_> = (0..options.queries)
        .map(|index| format!("query-{index}"))
        .collect();
    let (reaction, output) = ApplicationReaction::new("output", ids.clone());
    let receiver = output.take_receiver().await.context("output receiver")?;
    let mut builder = DrasiLib::builder()
        .with_id("core-workload")
        .with_source(source);
    for id in ids {
        builder = builder.with_query(options.query_config(&id));
    }
    let core = builder.with_reaction(reaction).build().await?;
    core.start().await?;
    let measured = exercise(options, Input::Ordinary(input), receiver).await;
    let stop = Instant::now();
    core.shutdown().await?;
    let mut measured = measured?;
    measured["shutdown_ns"] = json!(stop.elapsed().as_nanos() as u64);
    Ok(measured)
}

struct NativeSource {
    descriptor: ComponentDescriptor,
    receiver: mpsc::Receiver<SourceChange>,
    sequence: u64,
}

struct NativeSink {
    descriptor: ComponentDescriptor,
    output: mpsc::Sender<QueryResult>,
}

macro_rules! component {
    ($type:ty) => {
        #[async_trait]
        impl ComputationComponent for $type {
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
    };
}
component!(NativeSource);
component!(NativeSink);

#[async_trait]
impl EnvelopeSource for NativeSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let Some(change) = self.receiver.recv().await else {
            return Ok(None);
        };
        self.sequence += 1;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: GraphChangeCodec::encode_change(
                change,
                StreamId::try_new("input")?,
                self.sequence,
                None,
            )?,
        }))
    }
}

#[async_trait]
impl EnvelopeSink for NativeSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Accepted
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.output
            .send(QueryChangeCodec::to_legacy_result(&input.envelope)?)
            .await
            .context("output receiver closed")
    }
}

fn descriptor(
    id: &str,
    port: &str,
    direction: PortDirection,
    schema: SchemaDescriptor,
) -> Result<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        ComponentId::try_new(id)?,
        vec![PortDescriptor::new(
            PortId::try_new(port)?,
            direction,
            schema,
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

async fn native(options: &Options) -> Result<Value> {
    let directory = if options.workload == "persistent-aggregate" {
        Some(tempfile::tempdir()?)
    } else {
        None
    };
    let provider: Arc<dyn ComputationIndexProvider> = if let Some(directory) = &directory {
        #[cfg(feature = "computation-rocksdb-tests")]
        {
            Arc::new(
                drasi_index_rocksdb::computation::RocksDbComputationProvider::new(
                    directory.path(),
                    drasi_index_rocksdb::RocksIndexOptions::new(
                        false,
                        false,
                        drasi_index_rocksdb::RocksDbMemoryBudget::from_total_budget_bytes(
                            32 << 20,
                        )?,
                    ),
                ),
            )
        }
        #[cfg(not(feature = "computation-rocksdb-tests"))]
        {
            let _ = directory;
            anyhow::bail!("persistent-aggregate requires computation-rocksdb-tests");
        }
    } else {
        Arc::new(InMemoryComputationProvider)
    };
    let (input, receiver) = mpsc::channel(options.window);
    let (output, results) = mpsc::channel(options.window * options.queries);
    let mut builder = ComputationGraph::builder("core-workload")
        .source(Box::new(NativeSource {
            descriptor: descriptor(
                "input",
                "out",
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
            )?,
            receiver,
            sequence: 0,
        }))
        .bind_stream(endpoint("input", "out")?, StreamId::try_new("input")?);
    for index in 0..options.queries {
        let id = format!("query-{index}");
        let sink = format!("sink-{index}");
        let stream = StreamId::try_new(format!("{id}/out"))?;
        let settings = if options.workload == "join" {
            QueryExecutionSettings::from_legacy_config(&options.query_config(&id))
        } else {
            QueryExecutionSettings::default()
        };
        let query = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: "core-workload".into(),
                id: ComponentId::try_new(id.as_str())?,
                query: options.query().into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: stream.clone(),
                outbox_capacity: NonZeroUsize::new(options.window).context("window")?,
            },
            provider.clone(),
            QueryOptions::default(),
            settings,
            None,
        )
        .await?;
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(NativeSink {
                descriptor: descriptor(
                    &sink,
                    "in",
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                )?,
                output: output.clone(),
            }))
            .bind_stream(endpoint(&id, "out")?, stream)
            .connect(
                EdgeDefinition::new(endpoint("input", "out")?, endpoint(&id, "in")?),
                Box::new(BoundedPipeConfig {
                    capacity: options.window,
                }),
            )
            .connect(
                EdgeDefinition::new(endpoint(&id, "out")?, endpoint(&sink, "in")?),
                Box::new(BoundedPipeConfig {
                    capacity: options.window,
                }),
            );
    }
    drop(output);
    let mut graph = builder.build()?;
    let run = graph.start()?;
    let control = run.control();
    let (running, measured) = tokio::join!(run, async {
        let measured = exercise(options, Input::Native(input), results).await;
        let stop = Instant::now();
        control.cancel();
        (measured, stop)
    });
    graph.dispose().await?;
    ensure!(
        matches!(running, Ok(()) | Err(GraphError::Cancelled)),
        "native graph failed: {running:?}"
    );
    let (measured, stop) = measured;
    let mut measured = measured?;
    measured["shutdown_ns"] = json!(stop.elapsed().as_nanos() as u64);
    Ok(measured)
}

fn main() -> Result<()> {
    let options = Options::parse(&std::env::args().skip(1).collect::<Vec<_>>())?;
    let mut runtime = if options.runtime == "multi-thread" {
        let mut runtime = tokio::runtime::Builder::new_multi_thread();
        runtime.worker_threads(2);
        runtime
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    let value = runtime.enable_all().build()?.block_on(async {
        if options.execution == "native" {
            native(&options).await
        } else {
            ordinary(&options).await
        }
    })?;
    println!("{value}");
    Ok(())
}
