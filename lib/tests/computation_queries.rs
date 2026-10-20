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

#![cfg(test)]

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use drasi_core::{
    computation::{ComputationIndexProvider, InMemoryComputationProvider},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{channels::ResultDiff, computation::v1::*};

struct Source {
    descriptor: ComponentDescriptor,
    events: VecDeque<OutputEnvelope>,
    progress: Option<Arc<ConsumerProgressSet>>,
    positions: Option<Arc<InputPositionPolicy>>,
    keep_alive: bool,
    lifecycle: Arc<SourceLifecycle>,
    stop_gate: Option<Arc<StopGate>>,
}

#[derive(Default)]
struct SourceLifecycle {
    starts: std::sync::atomic::AtomicUsize,
    stops: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct StopGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        self.progress
            .as_ref()
            .map_or_else(ComponentRecovery::default, |progress| {
                match &self.positions {
                    Some(policy) => ComponentRecovery::default()
                        .with_consumer_positions(progress.clone(), policy.clone()),
                    None => ComponentRecovery::default().with_consumer_progress(progress.clone()),
                }
            })
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.lifecycle
            .starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(gate) = &self.stop_gate {
            gate.entered.notify_one();
            gate.release.notified().await;
            self.stop_gate = None;
        }
        self.lifecycle
            .stops
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        if self.keep_alive && self.events.is_empty() {
            std::future::pending::<()>().await;
        }
        Ok(self.events.pop_front())
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    events: Arc<Mutex<Vec<ChangeEnvelope>>>,
}
#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.events.lock().expect("results").push(input.envelope);
        Ok(())
    }
}

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("id")
}
fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("port")
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(value).expect("stream")
}
fn endpoint(node: &str, name: &str) -> Endpoint {
    Endpoint::new(id(node), port(name))
}

fn person(name: &str, update: bool, time: u64) -> SourceChange {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("people", "one"),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(serde_json::json!({ "name": name })),
    };
    if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    }
}

fn definition(query: &str) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "query-graph".into(),
        id: id("query"),
        query: query.into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: stream("query/out"),
        outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn disabled_durable_delivery_keeps_its_completion_future_small() {
    let mut query = ContinuousQueryTransformer::new(
        definition("MATCH (n:Person) RETURN n.name AS name"),
        Arc::new(InMemoryComputationProvider),
    )
    .await
    .unwrap();
    let completion = query.delivery_completed(&[]);
    let bytes = std::mem::size_of_val(completion.as_ref().get_ref());
    assert!(
        bytes <= 16 * std::mem::size_of::<usize>(),
        "disabled durable delivery retained storage-operation state: {bytes} bytes"
    );
    completion.await.unwrap();
}

fn source(changes: Vec<SourceChange>) -> Source {
    Source {
        progress: None,
        positions: None,
        keep_alive: false,
        lifecycle: Arc::new(SourceLifecycle::default()),
        stop_gate: None,
        descriptor: ComponentDescriptor::try_new(
            id("source"),
            vec![PortDescriptor::new(
                port("out"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )
        .expect("source"),
        events: changes
            .into_iter()
            .enumerate()
            .map(|(index, change)| OutputEnvelope {
                port: port("out"),
                envelope: GraphChangeCodec::encode_change(
                    change,
                    stream("source/out"),
                    index as u64 + 1,
                    None,
                )
                .expect("graph change"),
            })
            .collect(),
    }
}

fn sink(name: &str, events: Arc<Mutex<Vec<ChangeEnvelope>>>) -> Sink {
    Sink {
        descriptor: ComponentDescriptor::try_new(
            id(name),
            vec![PortDescriptor::new(
                port("in"),
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )
        .expect("sink"),
        events,
    }
}

async fn run_query(
    definition: ContinuousQueryDefinition,
    provider: Arc<dyn ComputationIndexProvider>,
    changes: Vec<SourceChange>,
) -> (QueryResults, Vec<ChangeEnvelope>) {
    let query = ContinuousQueryTransformer::new(definition, provider)
        .await
        .expect("construct graph query");
    let results = query.results();
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut graph = ComputationGraph::builder("query-graph")
        .source(Box::new(source(changes)))
        .query(Box::new(query))
        .sink(Box::new(sink("sink", events.clone())))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .bind_stream(endpoint("query", "out"), stream("query/out"))
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    tokio::time::timeout(Duration::from_secs(15), graph.start().expect("scope"))
        .await
        .expect("query graph deadlocked")
        .expect("query processing");
    let collected = events.lock().expect("events").clone();
    drop(graph);
    (results, collected)
}

#[tokio::test(flavor = "current_thread")]
async fn real_cypher_query_runs_inside_computation_graph_with_typed_add_update_delete() {
    let removed = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("people", "one"),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: 3000,
        },
    };
    let (results, output) = run_query(
        definition("MATCH (n:Person) RETURN n.name AS name"),
        Arc::new(InMemoryComputationProvider),
        vec![person("Alice", false, 1000), person("Alicia", true, 2000), removed],
    )
    .await;
    assert_eq!(output.len(), 3);
    let first = QueryChangeCodec::to_legacy_result(&output[0]).expect("projection");
    assert!(matches!(&first.results[0], ResultDiff::Add { data, .. } if data["name"] == "Alice"));
    assert!(matches!(
        QueryChangeCodec::to_legacy_result(&output[1])
            .expect("projection")
            .results[0],
        ResultDiff::Update { .. }
    ));
    assert!(matches!(
        QueryChangeCodec::to_legacy_result(&output[2])
            .expect("projection")
            .results[0],
        ResultDiff::Delete { .. }
    ));
    assert_eq!(results.snapshot().expect("snapshot").as_of_sequence, 3);
    assert!(results.snapshot().expect("snapshot").rows.is_empty());
    assert_eq!(results.replay(0).expect("retained output").len(), 3);
}

async fn source_fanout_to_real_queries_and_multiple_sinks(fault: Option<(&str, &str)>) {
    let removed = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("people", "one"),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: 3000,
        },
    };
    let mut input = source(vec![
        person("Alice", false, 1000),
        person("Alicia", true, 2000),
        removed,
    ]);
    let mut builder = ComputationGraph::builder("query-graph")
        .bind_stream(endpoint("source", "out"), stream("source/out"));
    let mut owners = Vec::new();
    let mut observations = Vec::new();
    for (name, alias) in [("left", "name"), ("right", "label")] {
        let mut definition = definition(&format!("MATCH (n:Person) RETURN n.name AS {alias}"));
        definition.id = id(name);
        definition.output_stream = stream(&format!("{name}/out"));
        let progress = Arc::new(QuerySourceProgress::new("query-graph", id(name)).unwrap());
        let query =
            ContinuousQueryTransformer::new(definition, Arc::new(InMemoryComputationProvider))
                .await
                .unwrap()
                .with_source_progress(progress.clone())
                .unwrap();
        let results = query.results();
        owners.push(progress);
        builder = builder
            .query(Box::new(query))
            .bind_stream(endpoint(name, "out"), stream(&format!("{name}/out")))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
        let mut subscribers = Vec::new();
        for suffix in ["one", "two"] {
            let sink_name = format!("{name}-{suffix}");
            let events = Arc::new(Mutex::new(Vec::new()));
            subscribers.push(events.clone());
            builder = builder.sink(Box::new(sink(&sink_name, events))).connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&sink_name, "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
        }
        observations.push((name, alias, results, subscribers));
    }
    let mut graph_id = "query-graph";
    let mut producer = id("source");
    match fault.map(|(kind, _)| kind) {
        Some("impostor") => {
            owners[0] = Arc::new(QuerySourceProgress::new("query-graph", id("left")).unwrap());
        }
        Some("omitted") => {
            owners.pop();
        }
        Some("foreign") => {
            graph_id = "foreign";
            owners = ["left", "right"]
                .into_iter()
                .map(|name| Arc::new(QuerySourceProgress::new("foreign", id(name)).unwrap()))
                .collect();
        }
        Some("producer") => producer = id("another-source"),
        None => {}
        Some(kind) => panic!("unknown binding fault {kind}"),
    }
    let progress = Arc::new(ConsumerProgressSet::new(graph_id, producer, owners).unwrap());
    input.progress = Some(progress.clone());
    let graph = builder.source(Box::new(input)).build();
    if let Some((kind, expected)) = fault {
        let error = match graph {
            Ok(_) => panic!("accepted invalid consumer progress: {kind}"),
            Err(error) => error,
        };
        assert!(error.to_string().contains(expected), "{kind}: {error}");
        return;
    }
    let mut graph = graph.unwrap();
    tokio::time::timeout(Duration::from_secs(15), graph.start().unwrap())
        .await
        .expect("fanout must drain every query and sink")
        .unwrap();
    for (name, alias, results, subscribers) in observations {
        assert_eq!(results.snapshot().unwrap().as_of_sequence, 3);
        assert!(results.snapshot().unwrap().rows.is_empty());
        let first = subscribers[0].lock().unwrap();
        let second = subscribers[1].lock().unwrap();
        assert_eq!(first.len(), 3, "{name}");
        assert_eq!(second.len(), 3, "{name}");
        for (left, right) in first.iter().zip(second.iter()) {
            assert!(Arc::ptr_eq(left.event(), right.event()));
            assert_eq!(QueryChangeCodec::metadata(left).unwrap().query_id, name);
        }
        let changes: Vec<_> = first
            .iter()
            .flat_map(|event| QueryChangeCodec::to_legacy_result(event).unwrap().results)
            .collect();
        assert_eq!(changes.len(), 3);
        assert!(matches!(&changes[0], ResultDiff::Add { data, .. }
            if data[alias] == "Alice" && data.as_object().unwrap().len() == 1));
        assert!(
            matches!(&changes[1], ResultDiff::Update { before, after, .. }
            if before[alias] == "Alice" && after[alias] == "Alicia")
        );
        assert!(matches!(&changes[2], ResultDiff::Delete { data, .. }
            if data[alias] == "Alicia"));
    }
    let observed = progress.snapshot().unwrap();
    assert_eq!(observed.consumers.len(), 2);
    for consumer in observed.consumers.values() {
        assert!(!consumer.persistent);
        assert_eq!(consumer.checkpoints.len(), 1);
        assert_eq!(consumer.checkpoints.values().next().unwrap().sequence, 3);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn real_query_fanout_current_thread() {
    source_fanout_to_real_queries_and_multiple_sinks(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_query_fanout_multi_thread() {
    source_fanout_to_real_queries_and_multiple_sinks(None).await;
}

#[tokio::test(flavor = "current_thread")]
async fn consumer_progress_requires_every_actual_owner_in_the_producer_graph() {
    for fault in [
        ("impostor", "actual committed-input owner"),
        ("omitted", "does not cover required branch right"),
        ("foreign", "another graph"),
        ("producer", "another producer"),
    ] {
        source_fanout_to_real_queries_and_multiple_sinks(Some(fault)).await;
    }
}

async fn consumer_changes_await_the_feedback_producer_stop() {
    use std::sync::atomic::Ordering;

    let mut input = source(Vec::new());
    input.keep_alive = true;
    let lifecycle = input.lifecycle.clone();
    let gate = Arc::new(StopGate::default());
    input.stop_gate = Some(gate.clone());
    let mut builder = ComputationGraph::builder("query-graph")
        .bind_stream(endpoint("source", "out"), stream("source/out"));
    let mut owners = Vec::new();
    for name in ["left", "right"] {
        let owner = Arc::new(QuerySourceProgress::new("query-graph", id(name)).unwrap());
        let mut definition = definition("MATCH (n:Person) RETURN n.name AS name");
        definition.id = id(name);
        definition.output_stream = stream(&format!("{name}/out"));
        let query =
            ContinuousQueryTransformer::new(definition, Arc::new(InMemoryComputationProvider))
                .await
                .unwrap()
                .with_source_progress(owner.clone())
                .unwrap();
        owners.push(owner);
        builder = builder
            .query(Box::new(query))
            .bind_stream(endpoint(name, "out"), stream(&format!("{name}/out")))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
        for suffix in ["one", "two"] {
            let sink_id = format!("{name}-{suffix}");
            builder = builder
                .sink(Box::new(sink(&sink_id, Arc::new(Mutex::new(Vec::new())))))
                .connect(
                    EdgeDefinition::new(endpoint(name, "out"), endpoint(&sink_id, "in")),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                );
        }
    }
    input.progress = Some(Arc::new(
        ConsumerProgressSet::new("query-graph", id("source"), owners).unwrap(),
    ));
    let mut graph = builder.source(Box::new(input)).build().unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        assert_eq!(
            control.startup_report().await.unwrap().summary,
            OperationSummary::Completed
        );
        let revision = control.desired_snapshot().revision;
        let observer = control
            .preview(
                revision,
                vec![DesiredMutation::Unbind {
                    edge: EdgeDefinition::new(endpoint("left", "out"), endpoint("left-two", "in")),
                    policy: RemovalPolicy::Orphan,
                }],
            )
            .await
            .unwrap();
        assert!(
            !observer.restarted().contains(&id("source")),
            "changing sinks after an input owner must not restart its upstream source"
        );
        let error = control
            .set_subscriptions(vec![(id("source"), id("left"))])
            .await
            .expect_err("uncoordinated membership change");
        assert!(error.to_string().contains("reconciled lifecycle"));
        assert_eq!(control.desired_snapshot().revision, revision);
        let preview = control
            .preview(
                revision,
                vec![DesiredMutation::Restart(GraphSelection::Exact(vec![id("left")]))],
            )
            .await
            .unwrap();
        assert!(preview.restarted().contains(&id("source")));
        assert!(preview.restarted().contains(&id("left")));
        assert!(!preview.restarted().contains(&id("right")));
        let change = control.reconcile(preview, TopologyBindings::default());
        tokio::pin!(change);
        tokio::select! {
            result = &mut change => panic!("reconciliation bypassed source stop: {result:?}"),
            _ = gate.entered.notified() => {}
        }
        assert_eq!(control.desired_snapshot().revision, revision);
        assert_eq!(lifecycle.starts.load(Ordering::SeqCst), 1);
        assert_eq!(lifecycle.stops.load(Ordering::SeqCst), 0);
        gate.release.notify_one();
        let report = change.await.unwrap();
        assert!(report.committed);
        assert_eq!(report.summary, OperationSummary::Completed);
        assert_eq!(lifecycle.starts.load(Ordering::SeqCst), 2);
        assert_eq!(lifecycle.stops.load(Ordering::SeqCst), 1);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test(flavor = "current_thread")]
async fn consumer_progress_lifecycle_current_thread() {
    tokio::time::timeout(
        Duration::from_secs(15),
        consumer_changes_await_the_feedback_producer_stop(),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consumer_progress_lifecycle_multi_thread() {
    tokio::time::timeout(
        Duration::from_secs(15),
        consumer_changes_await_the_feedback_producer_stop(),
    )
    .await
    .unwrap();
}

struct OrdinalPosition;

impl InputPositionComparator for OrdinalPosition {
    fn is_after(&self, event: &[u8], handled: &[u8]) -> anyhow::Result<bool> {
        Ok(u64::from_be_bytes(event.try_into()?) > u64::from_be_bytes(handled.try_into()?))
    }
}

struct SeedAtPosition {
    name: &'static str,
    logical: u64,
    position: u64,
    cursor: bool,
}

#[async_trait]
impl ComputationBootstrapProvider for SeedAtPosition {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter([Ok(
                GraphChangeCodec::encode_change(
                    person(self.name, false, self.position * 100),
                    stream("source/out"),
                    1,
                    None,
                )?,
            )])),
            watermarks: vec![BootstrapWatermark {
                stream: stream("source/out"),
                source_id: Some("source".into()),
                sequence: self.logical,
                position: self
                    .cursor
                    .then(|| bytes::Bytes::copy_from_slice(&self.position.to_be_bytes())),
            }],
        })
    }
}

struct GraphRelay {
    descriptor: ComponentDescriptor,
    sequence: u64,
}

#[async_trait]
impl ComputationComponent for GraphRelay {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        ComponentRecovery::stateless()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Transformer for GraphRelay {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.sequence += 1;
        let changes = GraphChangeCodec::decode_changes(&input.envelope)?;
        Ok(vec![OutputEnvelope {
            port: port("out"),
            envelope: GraphChangeCodec::derive_changes(
                &input.envelope,
                &changes,
                stream("relay/out"),
                self.sequence,
            )?,
        }])
    }
}

async fn independent_bootstrap_positions_filter_replay(relay: bool, positions: bool) {
    use drasi_computation_contracts::exchange::{SourceEvent, SourceEventWrapper};

    let stale_delete = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("people", "one"),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: 5000,
        },
    };
    let mut input = source(Vec::new());
    input.events = [
        (10, stale_delete),
        (20, person("Alicia", true, 2000)),
        (30, person("Bob", true, 3000)),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (position, change))| {
        let sequence = if positions {
            u64::try_from(index).unwrap() + 101
        } else {
            position
        };
        let event = SourceEventWrapper::new(
            "source".into(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
            sequence,
        )
        .with_source_position(bytes::Bytes::copy_from_slice(&u64::to_be_bytes(position)));
        OutputEnvelope {
            port: port("out"),
            envelope: GraphChangeCodec::encode_source_event(
                Arc::new(event),
                &id("source"),
                stream("source/out"),
                u64::try_from(index).unwrap() + 501,
                None,
            )
            .unwrap(),
        }
    })
    .collect();
    let mut builder = ComputationGraph::builder("query-graph")
        .bind_stream(endpoint("source", "out"), stream("source/out"));
    if relay {
        builder = builder
            .transformer(Box::new(GraphRelay {
                descriptor: ComponentDescriptor::try_new(
                    id("relay"),
                    vec![
                        PortDescriptor::new(
                            port("in"),
                            PortDirection::Input,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        ),
                        PortDescriptor::new(
                            port("out"),
                            PortDirection::Output,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        ),
                    ],
                )
                .unwrap(),
                sequence: 0,
            }))
            .bind_stream(endpoint("relay", "out"), stream("relay/out"))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("relay", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
    }
    let upstream = if relay { "relay" } else { "source" };
    let mut owners = Vec::new();
    let mut observations = Vec::new();
    for (name, initial, position, logical) in [
        ("ahead", "Alicia", 20, if positions { 100 } else { 20 }),
        ("joining", "Alice", 10, if positions { 0 } else { 10 }),
    ] {
        let owner = Arc::new(QuerySourceProgress::new("query-graph", id(name)).unwrap());
        let mut definition = definition("MATCH (n:Person) RETURN n.name AS name");
        definition.id = id(name);
        definition.output_stream = stream(&format!("{name}/out"));
        let query =
            ContinuousQueryTransformer::new(definition, Arc::new(InMemoryComputationProvider))
                .await
                .unwrap()
                .with_source_progress(owner.clone())
                .unwrap()
                .with_bootstrap(Arc::new(SeedAtPosition {
                    name: initial,
                    logical,
                    position,
                    cursor: positions,
                }));
        let results = query.results();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_id = format!("{name}-sink");
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(sink(&sink_id, events.clone())))
            .bind_stream(endpoint(name, "out"), stream(&format!("{name}/out")))
            .connect(
                EdgeDefinition::new(endpoint(upstream, "out"), endpoint(name, "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&sink_id, "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
        owners.push(owner);
        observations.push((name, results, events));
    }
    let progress = Arc::new(ConsumerProgressSet::new("query-graph", id("source"), owners).unwrap());
    input.progress = Some(progress.clone());
    input.positions = positions.then(|| {
        Arc::new(
            InputPositionPolicy::new(
                SourceProgressKey::Source("source".into()),
                Arc::new(OrdinalPosition),
            )
            .unwrap(),
        )
    });
    let mut graph = builder.source(Box::new(input)).build().unwrap();
    tokio::time::timeout(Duration::from_secs(15), graph.start().unwrap())
        .await
        .expect("position-filtered fanout must drain")
        .unwrap();
    for (name, results, events) in observations {
        let rows: Vec<_> =
            QueryChangeCodec::legacy_snapshot_rows(name, results.snapshot().unwrap().rows)
                .unwrap()
                .map(|(_, row)| row)
                .collect();
        assert_eq!(rows, [serde_json::json!({"name":"Bob"})], "{name}");
        let changes: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .flat_map(|event| QueryChangeCodec::to_legacy_result(event).unwrap().results)
            .collect();
        let expected = if name == "ahead" { 1 } else { 2 };
        assert_eq!(changes.len(), expected, "{name}: {changes:?}");
        assert!(changes
            .iter()
            .all(|change| matches!(change, ResultDiff::Update { .. })));
    }
    for state in progress.snapshot().unwrap().consumers.values() {
        let checkpoint = &state.checkpoints[&SourceProgressKey::Source("source".into())];
        assert_eq!(checkpoint.sequence, if positions { 103 } else { 30 });
        assert_eq!(
            checkpoint.source_position.as_deref(),
            Some(&30_u64.to_be_bytes()[..])
        );
        assert_eq!(state.transport_sequences[&stream("source/out")], 503);
        if relay {
            assert_eq!(state.transport_sequences[&stream("relay/out")], 3);
        }
        assert!(!state.persistent);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn independent_bootstrap_position_fanout_current_thread() {
    independent_bootstrap_positions_filter_replay(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_bootstrap_position_fanout_multi_thread() {
    independent_bootstrap_positions_filter_replay(false, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn stateless_path_position_fanout_current_thread() {
    independent_bootstrap_positions_filter_replay(true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stateless_path_position_fanout_multi_thread() {
    independent_bootstrap_positions_filter_replay(true, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn stable_sequence_fanout_through_stateless_path_current_thread() {
    independent_bootstrap_positions_filter_replay(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stable_sequence_fanout_through_stateless_path_multi_thread() {
    independent_bootstrap_positions_filter_replay(true, false).await;
}

async fn scheduled_result_times_and_provenance() {
    let earliest = chrono::Utc::now();
    let (results, output) = run_query(
        definition("MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name"),
        Arc::new(InMemoryComputationProvider),
        vec![person("Alice", false, 1000)],
    )
    .await;
    assert_eq!(output.len(), 1);
    let metadata = QueryChangeCodec::metadata(&output[0]).expect("metadata");
    assert!(
        metadata.timestamp >= earliest,
        "result timestamp must represent production time"
    );
    assert!(metadata.timestamp <= chrono::Utc::now());
    assert_eq!(
        QueryChangeCodec::metadata(&output[0])
            .expect("metadata")
            .source_id
            .as_deref(),
        Some("people")
    );
    assert_eq!(
        output[0]
            .system()
            .timestamp()
            .expect("future time")
            .timestamp_millis(),
        2000
    );
    assert!(output[0].lineage().is_some());
    assert_eq!(results.snapshot().expect("snapshot").as_of_sequence, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn future_work_runs_after_source_eof_and_preserves_original_source_provenance() {
    scheduled_result_times_and_provenance().await;
}

#[tokio::test(flavor = "current_thread")]
async fn scheduled_results_keep_production_and_logical_event_times_separate() {
    scheduled_result_times_and_provenance().await;
}

#[cfg(feature = "computation-rocksdb-tests")]
#[tokio::test]
async fn graph_query_atomically_persists_and_recovers_typed_output_without_legacy_managers() {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    let temp = tempfile::tempdir().expect("temp");
    let provider: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
        temp.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
        ),
    ));
    let definition = definition("MATCH (n:Person) RETURN n.name AS name");
    let (_, output) = run_query(
        definition.clone(),
        provider.clone(),
        vec![person("Alice", false, 1000)],
    )
    .await;
    assert_eq!(output.len(), 1);
    let mut query = ContinuousQueryTransformer::new(definition, provider)
        .await
        .expect("reopen query");
    query.start().await.expect("reconcile committed bundle");
    assert_eq!(
        query.results().snapshot().expect("snapshot").as_of_sequence,
        1
    );
    assert_eq!(query.results().snapshot().expect("snapshot").rows.len(), 1);
    assert_eq!(query.results().replay(0).expect("retained output").len(), 1);
    let duplicate = GraphChangeCodec::encode_change(
        person("Alice", false, 1000),
        stream("source/out"),
        1,
        None,
    )
    .expect("input");
    assert!(query
        .transform(InputEnvelope {
            port: port("in"),
            envelope: duplicate
        })
        .await
        .expect("checkpoint dedup")
        .is_empty());
    query.stop().await.expect("stop healthy query");
}

#[tokio::test]
async fn gql_and_multi_change_envelopes_use_the_same_graph_owned_query_path() {
    let mut gql = definition("MATCH (n:Person) RETURN n.name AS name");
    gql.language = ComputationQueryLanguage::Gql;
    let (_, output) = run_query(
        gql,
        Arc::new(InMemoryComputationProvider),
        vec![person("Alice", false, 1000)],
    )
    .await;
    assert_eq!(output.len(), 1);
    assert!(
        matches!(&QueryChangeCodec::to_legacy_result(&output[0]).expect("row").results[0],
        ResultDiff::Add { data, .. } if data["name"] == "Alice")
    );
}

#[cfg(feature = "computation-rocksdb-tests")]
mod atomic_failure {
    use super::*;
    use drasi_core::{
        computation::{ComputationIndexes, ComputationResource, TransactionDomain},
        interface::{IndexError, IndexSet, OutboxWriter},
    };
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use std::result::Result;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct FailingOutbox {
        inner: Arc<dyn OutboxWriter>,
        fail: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl OutboxWriter for FailingOutbox {
        async fn append(&self, query: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err(IndexError::CorruptedData);
            }
            self.inner.append(query, sequence, data).await
        }
        async fn read_from(
            &self,
            query: &str,
            after: u64,
        ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
            self.inner.read_from(query, after).await
        }
        async fn read_latest_sequence(&self, query: &str) -> Result<Option<u64>, IndexError> {
            self.inner.read_latest_sequence(query).await
        }
        async fn clear(&self, query: &str) -> Result<(), IndexError> {
            self.inner.clear(query).await
        }
        async fn trim_to_capacity(
            &self,
            query: &str,
            capacity: usize,
        ) -> Result<usize, IndexError> {
            self.inner.trim_to_capacity(query, capacity).await
        }
    }

    struct Provider {
        inner: Arc<dyn ComputationIndexProvider>,
        fail: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ComputationIndexProvider for Provider {
        async fn create_indexes(
            &self,
            graph: &str,
            query: &str,
        ) -> Result<ComputationIndexes, IndexError> {
            let original = self.inner.create_indexes(graph, query).await?;
            let set = original.indexes();
            let domain = TransactionDomain::new(set.session_control.clone());
            let resources = ComputationIndexes::try_new(
                IndexSet {
                    element_index: set.element_index.clone(),
                    archive_index: set.archive_index.clone(),
                    result_index: set.result_index.clone(),
                    future_queue: set.future_queue.clone(),
                    session_control: set.session_control.clone(),
                },
                Some(domain.clone()),
                Some(ComputationResource::participating(
                    original.checkpoint_store().expect("checkpoint").clone(),
                    &domain,
                )),
                Some(ComputationResource::participating(
                    Arc::new(FailingOutbox {
                        inner: original.outbox_writer().expect("outbox").clone(),
                        fail: self.fail.clone(),
                        calls: self.calls.clone(),
                    }),
                    &domain,
                )),
                Some(ComputationResource::participating(
                    original.live_results_writer().expect("live").clone(),
                    &domain,
                )),
            )
            .map_err(IndexError::other)?;
            Ok(resources.with_cleanup(original.cleanup().expect("cleanup").clone()))
        }
        fn is_volatile(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn query_output_failure_rolls_back_and_fences_before_any_later_input_can_mutate_indexes()
    {
        let temp = tempfile::tempdir().expect("temp");
        let base: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
            temp.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
            ),
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(Provider {
            inner: base.clone(),
            fail: Arc::new(AtomicBool::new(true)),
            calls: calls.clone(),
        });
        let definition = definition("MATCH (n:Person) RETURN n.name AS name");
        let mut query = ContinuousQueryTransformer::new(definition.clone(), provider)
            .await
            .expect("construct");
        let results = query.results();
        query.start().await.expect("start");
        let input = || InputEnvelope {
            port: port("in"),
            envelope: GraphChangeCodec::encode_change(
                person("Alice", false, 1000),
                stream("source/out"),
                1,
                None,
            )
            .expect("input"),
        };
        assert!(query.transform(input()).await.is_err());
        assert!(
            query.transform(input()).await.is_err(),
            "failure ingress fence rejects later input"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(results.snapshot().expect("snapshot").as_of_sequence, 0);
        assert!(results.snapshot().expect("snapshot").rows.is_empty());
        query.stop().await.expect("await failed query cleanup");
        drop(query);
        let mut recovered = ContinuousQueryTransformer::new(definition, base)
            .await
            .expect("reopen");
        recovered.start().await.expect("recover rollback");
        let output = recovered
            .transform(input())
            .await
            .expect("input checkpoint did not advance on failed output");
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].envelope.system().sequence(), 1);
        recovered.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn non_atomic_publication_failure_keeps_a_visible_durable_pending_fence() {
        let temp = tempfile::tempdir().expect("temp");
        let base: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
            temp.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
            ),
        ));
        let provider = Arc::new(Provider {
            inner: base.clone(),
            fail: Arc::new(AtomicBool::new(true)),
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let options = QueryOptions {
            recovery: QueryRecoveryPolicy::Strict,
            publication: QueryPublicationMode::NonAtomic,
        };
        let definition = definition("MATCH (n:Person) RETURN n.name AS name");
        {
            let mut query =
                ContinuousQueryTransformer::new_with_options(definition.clone(), provider, options)
                    .await
                    .expect("construct");
            query.start().await.expect("start");
            assert!(query
                .transform(InputEnvelope {
                    port: port("in"),
                    envelope: GraphChangeCodec::encode_change(
                        person("Alice", false, 1000),
                        stream("source/out"),
                        1,
                        None
                    )
                    .expect("input"),
                })
                .await
                .is_err());
            query.stop().await.expect("stop failed publication");
        }
        let mut reopened = ContinuousQueryTransformer::new_with_options(definition, base, options)
            .await
            .expect("reopen");
        let error = reopened
            .start()
            .await
            .expect_err("incomplete publication cannot silently resume");
        assert!(matches!(
            error.downcast_ref::<QueryRecoveryError>(),
            Some(QueryRecoveryError::PendingPublication)
        ));
        reopened.stop().await.expect("cleanup");
    }
}
