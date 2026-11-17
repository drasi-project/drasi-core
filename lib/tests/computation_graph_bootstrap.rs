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

//! Bootstrap providers, sources and queries exchanging bootstrap sessions
//! over graph edges.

#![cfg(test)]

#[path = "support/mod.rs"]
mod fixture_support;
use fixture_support::FixtureGraphBuilderExt as _;

use std::{
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::computation::v1::*;
use tokio::sync::mpsc;

fn id(value: &str) -> ComponentId {
    fixture_support::component_id(match value {
        "source" => 1,
        "provider" => 2,
        "left" => 3,
        "right" => 4,
        "left-sink" => 5,
        "right-sink" => 6,
        "local" => 7,
        "upstream" => 8,
        "upstream-sink" => 9,
        "durable" => 12,
        "second-source" => 13,
        "second-provider" => 14,
        other => panic!("missing explicit fixture component ID for {other}"),
    })
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
fn pipe() -> Box<dyn PipeProvider> {
    Box::new(BoundedPipeConfig { capacity: 4 })
}

fn person(key: &str, name: &str, time: u64) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("people", key),
                labels: Arc::from([Arc::from("Person")]),
                effective_from: time,
            },
            properties: ElementPropertyMap::from(serde_json::json!({ "name": name })),
        },
    }
}

fn graph_change_port(name: &str, direction: PortDirection, bootstrap: bool) -> PortDescriptor {
    let schema = GraphChangeCodec::schema().descriptor().clone();
    if bootstrap {
        PortDescriptor::bootstrap(port(name), direction, schema, PipeRequirements::default())
    } else {
        PortDescriptor::new(port(name), direction, schema, PipeRequirements::default())
    }
}

/// An in-process source that relays bootstrap sessions from its bootstrap
/// input and hands over at the live sequence it has emitted so far.
struct RelaySource {
    descriptor: ComponentDescriptor,
    out: StreamId,
    relay: Arc<RecordingRelay>,
    live: mpsc::UnboundedReceiver<SourceChange>,
    emitted: Arc<AtomicU64>,
    hold: bool,
}

struct RecordingRelay {
    relay: Arc<BootstrapRelay>,
    requests: Mutex<Vec<BootstrapRequest>>,
}

#[async_trait]
impl ControlHandler for RecordingRelay {
    async fn on_message(
        &self,
        message: PeerMessage,
        control: ComponentControl,
    ) -> anyhow::Result<()> {
        if let ControlNotification::BootstrapRequest(request) = &message.notification {
            self.requests.lock().unwrap().push(request.clone());
        }
        self.relay.on_message(message, control).await
    }
}

struct SourceHandle {
    live: mpsc::UnboundedSender<SourceChange>,
    relay: Arc<RecordingRelay>,
    handovers: Arc<Mutex<Vec<(ComponentId, BootstrapSessionId, u64)>>>,
}

impl SourceHandle {
    fn requests(&self) -> Vec<BootstrapRequest> {
        self.relay.requests.lock().unwrap().clone()
    }
    fn handovers(&self) -> Vec<(ComponentId, BootstrapSessionId, u64)> {
        self.handovers.lock().unwrap().clone()
    }
}

fn relay_source() -> (RelaySource, SourceHandle) {
    named_relay_source("source")
}

fn named_relay_source(name: &str) -> (RelaySource, SourceHandle) {
    let out = format!("{name}/out");
    let (live, receiver) = mpsc::unbounded_channel();
    let emitted = Arc::new(AtomicU64::new(0));
    let handovers = Arc::new(Mutex::new(Vec::new()));
    let relay = BootstrapRelay::new({
        let emitted = emitted.clone();
        let handovers = handovers.clone();
        Arc::new(move |request, _upstream| {
            let sequence = emitted.load(Ordering::SeqCst);
            handovers
                .lock()
                .unwrap()
                .push((request.consumer.clone(), request.session, sequence));
            Ok(vec![BootstrapWatermark {
                stream: stream(&out),
                source_id: None,
                sequence,
                position: None,
                producer: None,
            }])
        })
    });
    let relay = Arc::new(RecordingRelay {
        relay,
        requests: Mutex::new(Vec::new()),
    });
    let source = RelaySource {
        descriptor: ComponentDescriptor::try_new(
            id(name),
            vec![
                graph_change_port("out", PortDirection::Output, false),
                graph_change_port("bootstrap", PortDirection::Input, true),
                graph_change_port("snapshot", PortDirection::Output, true),
            ],
        )
        .unwrap(),
        out: stream(&format!("{name}/out")),
        relay: relay.clone(),
        live: receiver,
        emitted,
        hold: false,
    };
    (
        source,
        SourceHandle {
            live,
            relay,
            handovers,
        },
    )
}

#[async_trait]
impl ComputationComponent for RelaySource {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn bind_bootstrap(&mut self, ports: BootstrapPorts) {
        self.relay.relay.bind(ports);
        if self.hold {
            // A first start with no saved live position.
            self.relay.relay.hold_live();
        }
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        Some(self.relay.clone())
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for RelaySource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        self.relay.relay.live_released().await;
        let Some(change) = self.live.recv().await else {
            return Ok(None);
        };
        let sequence = self.emitted.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Some(OutputEnvelope {
            port: port("out"),
            envelope: GraphChangeCodec::encode_change(change, self.out.clone(), sequence, None)?,
        }))
    }
}

/// Serves a fixed set of records, or fails with a reason.
struct Seed {
    stream: String,
    records: Vec<SourceChange>,
    failure: Option<&'static str>,
}

#[async_trait]
impl ComputationBootstrapProvider for Seed {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        if let Some(reason) = self.failure {
            anyhow::bail!("{reason}");
        }
        let changes = self
            .records
            .iter()
            .enumerate()
            .map(|(index, change)| {
                GraphChangeCodec::encode_change(
                    change.clone(),
                    stream(&self.stream),
                    index as u64 + 1,
                    None,
                )
                .map_err(anyhow::Error::from)
            })
            .collect::<Vec<_>>();
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter(changes)),
            watermarks: Vec::new(),
        })
    }
}

struct ProviderHandle {
    sessions: Arc<Mutex<Vec<BootstrapRequest>>>,
}

fn provider(
    name: &str,
    records: Vec<SourceChange>,
    failure: Option<&'static str>,
) -> (BootstrapProviderComponent, ProviderHandle) {
    let sessions = Arc::new(Mutex::new(Vec::new()));
    let stream_name = if name == "provider" {
        "seed".to_string()
    } else {
        format!("{name}/seed")
    };
    let factory: BootstrapProviderFactory = {
        let sessions = sessions.clone();
        Arc::new(move |request: &BootstrapRequest| {
            sessions.lock().unwrap().push(request.clone());
            Ok(Arc::new(Seed {
                stream: stream_name.clone(),
                records: records.clone(),
                failure,
            }) as Arc<dyn ComputationBootstrapProvider>)
        })
    };
    (
        BootstrapProviderComponent::new(id(name), factory).unwrap(),
        ProviderHandle { sessions },
    )
}

struct Sink {
    descriptor: ComponentDescriptor,
}

#[async_trait]
impl ComputationComponent for Sink {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

fn sink(name: &str) -> Sink {
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
        .unwrap(),
    }
}

async fn query(name: &str, text: &str) -> (ContinuousQueryTransformer, QueryResults) {
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "bootstrap-graph".into(),
            id: id(name),
            query: text.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream(&format!("{name}/out")),
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await
    .unwrap();
    let results = query.results();
    (query, results)
}

fn rows(results: &QueryResults) -> Vec<serde_json::Value> {
    let snapshot = results.snapshot().unwrap();
    let mut rows: Vec<_> =
        QueryChangeCodec::legacy_snapshot_rows(&snapshot.projector, snapshot.rows)
            .unwrap()
            .map(|(_, row)| row)
            .collect();
    rows.sort_by_key(|row| row.to_string());
    rows
}

fn names(values: &[&str]) -> Vec<serde_json::Value> {
    let mut rows: Vec<_> = values
        .iter()
        .map(|name| serde_json::json!({ "name": name }))
        .collect();
    rows.sort_by_key(|row| row.to_string());
    rows
}

const PEOPLE: &str = "MATCH (n:Person) RETURN n.name AS name";

/// Adds a query consuming the source's data and an output sink.
fn with_query(
    builder: ComputationGraphBuilder,
    name: &str,
    query: ContinuousQueryTransformer,
) -> ComputationGraphBuilder {
    let sink_name = format!("{name}-sink");
    builder
        .fixture_transformer(Box::new(query))
        .fixture_sink(Box::new(sink(&sink_name)))
        .bind_stream(endpoint(name, "out"), stream(&format!("{name}/out")))
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(endpoint(name, "out"), endpoint(&sink_name, "in")),
            pipe(),
        )
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn wait_started(control: &GraphControl, names: &[&str]) {
    for name in names {
        tokio::time::timeout(
            Duration::from_secs(10),
            control
                .component_handle_by_id(&id(name))
                .unwrap()
                .wait_started(),
        )
        .await
        .unwrap_or_else(|_| panic!("{name} did not start: {:#?}", control.observed()))
        .unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn two_consumers_bootstrap_through_one_source_relay_with_their_own_sessions() {
    let (source, handle) = relay_source();
    let (seed, seeded) = provider("provider", vec![person("a", "Alice", 1)], None);
    let (left, left_results) = query("left", PEOPLE).await;
    let (right, right_results) = query("right", PEOPLE).await;
    let mut builder = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(seed))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        );
    for (name, query) in [("left", left), ("right", right)] {
        builder = with_query(builder, name, query).connect(
            EdgeDefinition::new(endpoint("source", "snapshot"), endpoint(name, "bootstrap")),
            pipe(),
        );
    }
    let mut graph = builder.build().unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        wait_started(&control, &["left", "right"]).await;
        assert_eq!(rows(&left_results), names(&["Alice"]));
        assert_eq!(rows(&right_results), names(&["Alice"]));

        let requests = handle.requests();
        assert_eq!(requests.len(), 2);
        assert_ne!(requests[0].session, requests[1].session);
        let mut consumers: Vec<_> = requests.iter().map(|r| r.consumer.clone()).collect();
        consumers.sort();
        assert_eq!(consumers, vec![id("left"), id("right")]);
        // Each downstream session maps to its own upstream provider session.
        let upstream = seeded.sessions.lock().unwrap().clone();
        assert_eq!(upstream.len(), 2);
        assert!(upstream.iter().all(|r| r.consumer == id("source")));
        assert_ne!(upstream[0].session, upstream[1].session);
        // The provider sees the consumer each relayed session serves.
        let mut origins: Vec<_> = upstream.iter().map(|r| r.origin().clone()).collect();
        origins.sort();
        assert_eq!(origins, vec![id("left"), id("right")]);
        let handovers = handle.handovers();
        assert_eq!(handovers.len(), 2);
        for request in &requests {
            assert!(handovers
                .iter()
                .any(|(consumer, session, _)| consumer == &request.consumer
                    && session == &request.session));
        }

        handle.live.send(person("b", "Bob", 2)).unwrap();
        eventually("live change", || {
            rows(&left_results) == names(&["Alice", "Bob"])
                && rows(&right_results) == names(&["Alice", "Bob"])
        })
        .await;
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

/// Two queries bootstrap from the same two relaying sources, each of which
/// serves one session at a time. A query must not hold one source's session
/// while waiting for the other's, or the queries deadlock. The hang depends on
/// the order in which requests reach the sources, so several rounds run on a
/// multi-threaded runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queries_sharing_two_relaying_sources_bootstrap_without_deadlock() {
    for _ in 0..10 {
        bootstrap_from_two_shared_relaying_sources().await;
    }
}

async fn bootstrap_from_two_shared_relaying_sources() {
    let people = |prefix: &str| {
        (0..32)
            .map(|index| {
                let name = format!("{prefix}{index}");
                person(&name, &name, 1)
            })
            .collect::<Vec<_>>()
    };
    let (first_source, _first) = named_relay_source("source");
    let (second_source, _second) = named_relay_source("second-source");
    let (first_seed, _) = provider("provider", people("p"), None);
    let (second_seed, _) = provider("second-provider", people("s"), None);
    let (left, left_results) = query("left", PEOPLE).await;
    let (right, right_results) = query("right", PEOPLE).await;
    let mut builder = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(first_source))
        .fixture_source(Box::new(second_source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(first_seed))
        .bootstrap_provider(fixture_support::component_name(), Box::new(second_seed))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .bind_stream(
            endpoint("second-source", "out"),
            stream("second-source/out"),
        );
    for (provider, source) in [("provider", "source"), ("second-provider", "second-source")] {
        builder = builder.connect(
            EdgeDefinition::new(
                endpoint(provider, "snapshot"),
                endpoint(source, "bootstrap"),
            ),
            pipe(),
        );
    }
    // The queries list their inputs in opposite orders.
    for (name, query, sources) in [
        ("left", left, ["source", "second-source"]),
        ("right", right, ["second-source", "source"]),
    ] {
        builder = with_query(builder, name, query).connect(
            EdgeDefinition::new(endpoint("second-source", "out"), endpoint(name, "in")),
            pipe(),
        );
        for source in sources {
            builder = builder.connect(
                EdgeDefinition::new(endpoint(source, "snapshot"), endpoint(name, "bootstrap")),
                pipe(),
            );
        }
    }
    let mut graph = builder.build().unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        wait_started(&control, &["left", "right"]).await;
        assert_eq!(rows(&left_results).len(), 64);
        assert_eq!(rows(&right_results).len(), 64);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_local_provider_is_exclusive_and_inputs_are_never_asked() {
    let (source, handle) = relay_source();
    let (seed, seeded) = provider("local", vec![person("c", "Carol", 1)], None);
    let (left, left_results) = query("left", PEOPLE).await;
    let builder = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(seed))
        .bind_stream(endpoint("source", "out"), stream("source/out"));
    let mut graph = with_query(builder, "left", left)
        .connect(
            EdgeDefinition::new(endpoint("local", "snapshot"), endpoint("left", "bootstrap")),
            pipe(),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        wait_started(&control, &["left"]).await;
        assert_eq!(rows(&left_results), names(&["Carol"]));
        assert!(handle.requests().is_empty());
        let sessions = seeded.sessions.lock().unwrap().clone();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].consumer, id("left"));
        handle.live.send(person("d", "Dan", 2)).unwrap();
        eventually("live change", || {
            rows(&left_results) == names(&["Carol", "Dan"])
        })
        .await;
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn source_relay_and_upstream_query_snapshot_merge() {
    let (source, handle) = relay_source();
    let (seed, _seeded) = provider("provider", vec![person("a", "Alice", 1)], None);
    let (upstream, upstream_results) = query("upstream", PEOPLE).await;
    // The downstream query sees both the source's people and the upstream
    // query's result nodes.
    let (left, left_results) = query("left", "MATCH (n) RETURN count(n) AS total").await;
    let builder = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(seed))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(
                endpoint("source", "snapshot"),
                endpoint("upstream", "bootstrap"),
            ),
            pipe(),
        );
    let builder = with_query(builder, "upstream", upstream);
    let mut graph = with_query(builder, "left", left)
        .connect(
            EdgeDefinition::new(endpoint("upstream", "out"), endpoint("left", "in")),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(
                endpoint("source", "snapshot"),
                endpoint("left", "bootstrap"),
            ),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(
                endpoint("upstream", "snapshot"),
                endpoint("left", "bootstrap"),
            ),
            pipe(),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        wait_started(&control, &["upstream", "left"]).await;
        assert_eq!(rows(&upstream_results), names(&["Alice"]));
        assert_eq!(rows(&left_results), [serde_json::json!({ "total": 2 })]);
        let consumers: Vec<_> = handle.requests().into_iter().map(|r| r.consumer).collect();
        assert!(consumers.contains(&id("upstream")) && consumers.contains(&id("left")));
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_restarted_consumer_bootstraps_again_before_live_changes() {
    let (source, handle) = relay_source();
    let (seed, seeded) = provider("provider", vec![person("a", "Alice", 1)], None);
    let (left, left_results) = query("left", PEOPLE).await;
    let builder = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(seed))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        );
    let mut graph = with_query(builder, "left", left)
        .connect(
            EdgeDefinition::new(
                endpoint("source", "snapshot"),
                endpoint("left", "bootstrap"),
            ),
            pipe(),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        wait_started(&control, &["left"]).await;
        handle.live.send(person("b", "Bob", 2)).unwrap();
        eventually("live change", || {
            rows(&left_results) == names(&["Alice", "Bob"])
        })
        .await;
        let left = control.component_handle_by_id(&id("left")).unwrap();
        left.stop().await.unwrap();
        left.start().await.unwrap();
        wait_started(&control, &["left"]).await;
        // The volatile query reset and bootstrapped from a fresh session.
        assert_eq!(seeded.sessions.lock().unwrap().len(), 2);
        assert_eq!(handle.requests().len(), 2);
        assert_eq!(rows(&left_results), names(&["Alice"]));
        handle.live.send(person("c", "Carol", 3)).unwrap();
        eventually("live change after restart", || {
            rows(&left_results) == names(&["Alice", "Carol"])
        })
        .await;
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_provider_fails_the_consumer_bootstrap_with_its_reason() {
    let (source, _handle) = relay_source();
    let (seed, _seeded) = provider("provider", Vec::new(), Some("snapshot store unavailable"));
    let (left, _left_results) = query("left", PEOPLE).await;
    let builder = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(seed))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        );
    let mut graph = with_query(builder, "left", left)
        .connect(
            EdgeDefinition::new(
                endpoint("source", "snapshot"),
                endpoint("left", "bootstrap"),
            ),
            pipe(),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            control
                .component_handle_by_id(&id("left"))
                .unwrap()
                .wait_started(),
        )
        .await
        .expect("bootstrap failure must not hang")
        .expect_err("bootstrap failure must fail the consumer");
        let rendered = format!("{error:#} {error:?}");
        assert!(
            rendered.contains("snapshot store unavailable"),
            "missing provider reason: {rendered}"
        );
        control.cancel();
    });
    assert!(result.is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn a_late_consumer_gets_its_own_snapshot_while_live_changes_continue() {
    let (source, handle) = relay_source();
    let (seed, seeded) = provider("provider", vec![person("a", "Alice", 1)], None);
    let (left, left_results) = query("left", PEOPLE).await;
    let (right, right_results) = query("right", PEOPLE).await;
    let builder = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(seed))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        );
    let mut graph = with_query(builder, "left", left)
        .connect(
            EdgeDefinition::new(
                endpoint("source", "snapshot"),
                endpoint("left", "bootstrap"),
            ),
            pipe(),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        wait_started(&control, &["left"]).await;
        for (key, name, time) in [("b", "Bob", 2), ("c", "Carol", 3)] {
            handle.live.send(person(key, name, time)).unwrap();
        }
        eventually("live changes", || {
            rows(&left_results) == names(&["Alice", "Bob", "Carol"])
        })
        .await;

        let right_handle = control
            .add_component(
                ComponentAddition::new(
                    ComponentName::try_new("right").unwrap(),
                    ConstructedComponent::transformer(Box::new(right)),
                )
                .bind_stream(port("out"), stream("right/out"))
                .auto_start(false),
            )
            .await
            .unwrap();
        control
            .add_component(
                ComponentAddition::new(
                    ComponentName::try_new("right-sink").unwrap(),
                    ConstructedComponent::sink(Box::new(sink("right-sink"))),
                )
                .auto_start(false),
            )
            .await
            .unwrap();
        for (from, to) in [
            (endpoint("source", "out"), endpoint("right", "in")),
            (
                endpoint("source", "snapshot"),
                endpoint("right", "bootstrap"),
            ),
            (endpoint("right", "out"), endpoint("right-sink", "in")),
        ] {
            let report = control
                .connect(
                    EdgeDefinition::new(from, to),
                    pipe(),
                    RelationshipPolicy::default(),
                )
                .await
                .unwrap();
            assert!(report.committed, "{report:?}");
        }
        control
            .component_handle_by_id(&id("right-sink"))
            .unwrap()
            .start()
            .await
            .unwrap();
        right_handle.start().await.unwrap();
        wait_started(&control, &["right"]).await;

        // The late consumer's snapshot covers the live changes emitted before
        // it joined; its handover boundary is its own.
        let handovers = handle.handovers();
        assert_eq!(handovers.len(), 2);
        assert_eq!(handovers[0].0, id("left"));
        assert_eq!(handovers[0].2, 0);
        assert_eq!(handovers[1].0, id("right"));
        assert_eq!(handovers[1].2, 2);
        assert_eq!(seeded.sessions.lock().unwrap().len(), 2);
        assert_eq!(rows(&right_results), names(&["Alice"]));

        handle.live.send(person("d", "Dan", 4)).unwrap();
        eventually("live change for both consumers", || {
            rows(&left_results) == names(&["Alice", "Bob", "Carol", "Dan"])
                && rows(&right_results) == names(&["Alice", "Dan"])
        })
        .await;
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

fn rejected(builder: ComputationGraphBuilder, expected: &str) {
    match builder.build() {
        Ok(_) => panic!("accepted invalid bootstrap wiring: {expected}"),
        Err(error) => {
            let rendered = format!("{error:#} {error:?}").to_lowercase();
            assert!(rendered.contains(expected), "{expected}: {rendered}");
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn graph_validation_rejects_invalid_bootstrap_wiring() {
    async fn base() -> ComputationGraphBuilder {
        let (source, _) = relay_source();
        let (left, _) = query("left", PEOPLE).await;
        with_query(
            ComputationGraph::builder("bootstrap-graph")
                .fixture_source(Box::new(source))
                .bind_stream(endpoint("source", "out"), stream("source/out")),
            "left",
            left,
        )
    }

    // A bootstrap output never feeds a data input, and vice versa.
    rejected(
        base().await.connect(
            EdgeDefinition::new(endpoint("source", "snapshot"), endpoint("left", "in")),
            pipe(),
        ),
        "bootstrap",
    );
    let (data, _) = query("upstream", PEOPLE).await;
    rejected(
        with_query(base().await, "upstream", data).connect(
            EdgeDefinition::new(endpoint("upstream", "out"), endpoint("left", "bootstrap")),
            pipe(),
        ),
        "bootstrap",
    );

    // A local provider and the inputs' bootstrap outputs are exclusive.
    let (seed, _) = provider("local", Vec::new(), None);
    rejected(
        base()
            .await
            .bootstrap_provider(fixture_support::component_name(), Box::new(seed))
            .connect(
                EdgeDefinition::new(endpoint("local", "snapshot"), endpoint("left", "bootstrap")),
                pipe(),
            )
            .connect(
                EdgeDefinition::new(
                    endpoint("source", "snapshot"),
                    endpoint("left", "bootstrap"),
                ),
                pipe(),
            ),
        "not both",
    );

    // A consumer bootstraps only from producers whose data it consumes.
    let (upstream, _) = query("upstream", PEOPLE).await;
    rejected(
        with_query(base().await, "upstream", upstream).connect(
            EdgeDefinition::new(
                endpoint("upstream", "snapshot"),
                endpoint("left", "bootstrap"),
            ),
            pipe(),
        ),
        "when it consumes its data",
    );

    // A source's bootstrap input accepts only a local provider.
    let (other, _) = query("upstream", PEOPLE).await;
    rejected(
        with_query(base().await, "upstream", other).connect(
            EdgeDefinition::new(
                endpoint("upstream", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        ),
        "bootstrap",
    );

    // Providers declare only bootstrap outputs.
    let descriptor = ComponentDescriptor::try_new(
        id("provider"),
        vec![graph_change_port("out", PortDirection::Output, false)],
    )
    .unwrap();
    assert!(BootstrapProviderComponent::with_descriptor(
        descriptor,
        Arc::new(|_: &BootstrapRequest| anyhow::bail!("unused")),
    )
    .is_err());

    // Bootstrap ports are optional; data ports still must connect.
    assert!(base().await.build().is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_nodes_and_edges_are_inspectable_like_other_components() {
    let (source, _) = relay_source();
    let (seed, _) = provider("provider", Vec::new(), None);
    let graph = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(seed))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .fixture_sink(Box::new(Sink {
            descriptor: ComponentDescriptor::try_new(
                id("left-sink"),
                vec![graph_change_port("in", PortDirection::Input, false)],
            )
            .unwrap(),
        }))
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("left-sink", "in")),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        )
        .build()
        .unwrap();
    let snapshot = graph.snapshot();
    let node = snapshot
        .nodes
        .iter()
        .find(|node| node.descriptor.id() == &id("provider"))
        .expect("bootstrap provider node");
    assert_eq!(node.role, ComponentRole::BootstrapProvider);
    assert!(snapshot.edges.iter().any(|edge| edge.definition
        == EdgeDefinition::new(
            endpoint("provider", "snapshot"),
            endpoint("source", "bootstrap")
        )));
    assert!(snapshot.is_bootstrap_relationship(&EdgeDefinition::new(
        endpoint("provider", "snapshot"),
        endpoint("source", "bootstrap"),
    )));
}

/// A sink that runs its bootstrap session by hand and records every raw
/// envelope its bootstrap edge delivers.
struct Probe {
    descriptor: ComponentDescriptor,
    control: Option<ComponentControl>,
    ports: Arc<Mutex<Option<BootstrapPorts>>>,
    seen: Arc<Mutex<Vec<BootstrapSessionId>>>,
    own: Arc<Mutex<Option<BootstrapSessionId>>>,
}

fn probe(name: &str) -> Probe {
    Probe {
        descriptor: ComponentDescriptor::try_new(
            id(name),
            vec![
                graph_change_port("in", PortDirection::Input, false),
                graph_change_port("bootstrap", PortDirection::Input, true),
            ],
        )
        .unwrap(),
        control: None,
        ports: Arc::default(),
        seen: Arc::default(),
        own: Arc::default(),
    }
}

#[async_trait]
impl ComputationComponent for Probe {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn bind_control(&mut self, control: ComponentControl) {
        self.control = Some(control);
    }
    fn bind_bootstrap(&mut self, ports: BootstrapPorts) {
        *self.ports.lock().unwrap() = Some(ports);
    }
    async fn start(&mut self, _context: &StartContext) -> anyhow::Result<()> {
        let ports = self.ports.lock().unwrap().clone().unwrap();
        let edge = ports.inputs().pop().unwrap();
        let reader = BootstrapSessionReader::new(ports.clone(), edge.clone());
        *self.own.lock().unwrap() = Some(reader.session());
        reader
            .request(self.control.as_ref().unwrap(), false, None, None)
            .await?;
        loop {
            let envelope = ports.receive(&edge).await?.unwrap();
            let marker = BootstrapMarker::of(&envelope)?.unwrap();
            self.seen.lock().unwrap().push(marker.session);
            if matches!(marker.event, BootstrapSessionEvent::Complete { .. }) {
                return Ok(());
            }
        }
    }
    drasi_lib::passive_component_lifecycle!(@hook stop, StopContext);
    drasi_lib::passive_component_lifecycle!(@hook initialize, InitializeContext);
    drasi_lib::passive_component_lifecycle!(@hook pause, PauseContext);
    drasi_lib::passive_component_lifecycle!(@hook resume, ResumeContext);
    drasi_lib::passive_component_lifecycle!(@hook dispose, DisposeContext);
}

#[async_trait]
impl EnvelopeSink for Probe {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_replies_are_delivered_only_on_the_requesting_edge() {
    let (source, _handle) = relay_source();
    let (seed, _seeded) = provider("provider", vec![person("a", "Alice", 1)], None);
    let mut builder = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(seed))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        );
    let mut probes = Vec::new();
    for name in ["left", "right"] {
        let probe = probe(name);
        probes.push((probe.ports.clone(), probe.seen.clone(), probe.own.clone()));
        builder = builder
            .fixture_sink(Box::new(probe))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                pipe(),
            )
            .connect(
                EdgeDefinition::new(endpoint("source", "snapshot"), endpoint(name, "bootstrap")),
                pipe(),
            );
    }
    let mut graph = builder.build().unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        wait_started(&control, &["left", "right"]).await;
        for (ports, seen, own) in &probes {
            let own = own.lock().unwrap().unwrap();
            // Begin, one record and completion: nothing from the other session.
            assert_eq!(*seen.lock().unwrap(), vec![own; 3]);
            let ports = ports.lock().unwrap().clone().unwrap();
            let edge = ports.inputs().pop().unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), ports.receive(&edge))
                    .await
                    .is_err(),
                "another consumer's session reached this edge"
            );
        }
        let sessions: Vec<_> = probes
            .iter()
            .map(|(_, _, own)| own.lock().unwrap().unwrap())
            .collect();
        assert_ne!(sessions[0], sessions[1]);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_first_start_holds_live_changes_until_the_first_boundary() {
    let (mut source, handle) = relay_source();
    source.hold = true;
    let (seed, _seeded) = provider("provider", vec![person("a", "Alice", 1)], None);
    let (left, left_results) = query("left", PEOPLE).await;
    // Sent before any consumer bootstrapped: held until the first boundary.
    handle.live.send(person("b", "Bob", 2)).unwrap();
    let builder = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(seed))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        );
    let mut graph = with_query(builder, "left", left)
        .connect(
            EdgeDefinition::new(
                endpoint("source", "snapshot"),
                endpoint("left", "bootstrap"),
            ),
            pipe(),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        wait_started(&control, &["left"]).await;
        eventually("held live change", || {
            rows(&left_results) == names(&["Alice", "Bob"])
        })
        .await;
        // Nothing was emitted before the first session's handover boundary.
        let handovers = handle.handovers();
        assert_eq!(handovers.len(), 1);
        assert_eq!(handovers[0].2, 0);
        assert!(!handle.relay.relay.is_live_held());
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

/// Persistent consumer state recording every write.
#[derive(Default)]
struct DurableState {
    writes: Mutex<Vec<bytes::Bytes>>,
}

#[async_trait]
impl BootstrapState for DurableState {
    fn durability(&self) -> drasi_core::interface::StorageDurability {
        drasi_core::interface::StorageDurability::LOCAL_PROCESS_RESTART
    }
    async fn read(&self) -> anyhow::Result<Option<bytes::Bytes>> {
        Ok(self.writes.lock().unwrap().last().cloned())
    }
    async fn write(&self, state: bytes::Bytes) -> anyhow::Result<()> {
        self.writes.lock().unwrap().push(state);
        Ok(())
    }
}

/// A provider that records its handover in durable state before its first
/// record, as a provider creating an external snapshot resource does.
struct Handover {
    seen: Arc<Mutex<Vec<Option<bytes::Bytes>>>>,
}

#[async_trait]
impl ComputationBootstrapProvider for Handover {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("requires state")
    }
    async fn prepare_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<BootstrapPreparation> {
        let saved = state.read().await?;
        self.seen.lock().unwrap().push(saved);
        Ok(BootstrapPreparation::Ready)
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<ComputationBootstrapSnapshot> {
        state.write(bytes::Bytes::from_static(b"slot-1")).await?;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter(vec![
                GraphChangeCodec::encode_change(person("a", "Alice", 1), stream("seed"), 1, None)
                    .map_err(anyhow::Error::from),
            ])),
            watermarks: Vec::new(),
        })
    }
}

/// A sink that bootstraps a persistent consumer through its bootstrap edge.
struct DurableProbe {
    descriptor: ComponentDescriptor,
    control: Option<ComponentControl>,
    ports: Option<BootstrapPorts>,
    state: Arc<DurableState>,
    first_write_before_records: Arc<Mutex<Option<bool>>>,
}

#[async_trait]
impl ComputationComponent for DurableProbe {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn bind_control(&mut self, control: ComponentControl) {
        self.control = Some(control);
    }
    fn bind_bootstrap(&mut self, ports: BootstrapPorts) {
        self.ports = Some(ports);
    }
    async fn start(&mut self, _context: &StartContext) -> anyhow::Result<()> {
        use futures::StreamExt as _;
        for _ in 0..2 {
            let provider = PipeBootstrapProvider::new(
                "bootstrap-graph",
                self.ports.clone().unwrap(),
                self.control.clone().unwrap(),
                None,
            )?;
            assert_eq!(
                provider.prepare_with_state(self.state.as_ref()).await?,
                BootstrapPreparation::Ready
            );
            let mut snapshot = provider.snapshot_with_state(self.state.as_ref()).await?;
            let written = !self.state.writes.lock().unwrap().is_empty();
            let mut records = 0;
            while let Some(record) = snapshot.changes.next().await {
                record?;
                records += 1;
            }
            assert_eq!(records, 1);
            drop(snapshot);
            provider.complete_snapshot().await?;
            if let Some(state) = provider.completion_state()? {
                self.state.write(state).await?;
            }
            self.first_write_before_records
                .lock()
                .unwrap()
                .get_or_insert(written);
        }
        Ok(())
    }
    drasi_lib::passive_component_lifecycle!(@hook stop, StopContext);
    drasi_lib::passive_component_lifecycle!(@hook initialize, InitializeContext);
    drasi_lib::passive_component_lifecycle!(@hook pause, PauseContext);
    drasi_lib::passive_component_lifecycle!(@hook resume, ResumeContext);
    drasi_lib::passive_component_lifecycle!(@hook dispose, DisposeContext);
}

#[async_trait]
impl EnvelopeSink for DurableProbe {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn provider_state_is_retained_by_a_persistent_consumer_and_replayed() {
    let (source, _handle) = relay_source();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let durable = BootstrapProviderComponent::new(id("provider"), {
        let seen = seen.clone();
        Arc::new(move |_: &BootstrapRequest| {
            Ok(Arc::new(Handover { seen: seen.clone() }) as Arc<dyn ComputationBootstrapProvider>)
        })
    })
    .unwrap();
    let state = Arc::new(DurableState::default());
    let before = Arc::new(Mutex::new(None));
    let consumer = DurableProbe {
        descriptor: ComponentDescriptor::try_new(
            id("durable"),
            vec![
                graph_change_port("in", PortDirection::Input, false),
                graph_change_port("bootstrap", PortDirection::Input, true),
            ],
        )
        .unwrap(),
        control: None,
        ports: None,
        state: state.clone(),
        first_write_before_records: before.clone(),
    };
    let mut graph = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(durable))
        .fixture_sink(Box::new(consumer))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("durable", "in")),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(
                endpoint("source", "snapshot"),
                endpoint("durable", "bootstrap"),
            ),
            pipe(),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        wait_started(&control, &["durable"]).await;
        // The handover reached durable state before any record was consumed.
        assert_eq!(*before.lock().unwrap(), Some(true));
        // The first session started fresh; the second got the retained handover.
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], None);
        assert_eq!(seen[1], Some(bytes::Bytes::from_static(b"slot-1")));
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

/// A provider node whose start waits for a release, so bootstrap requests can
/// arrive before it runs.
struct DelayedProvider {
    inner: BootstrapProviderComponent,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ComputationComponent for DelayedProvider {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    fn bind_bootstrap(&mut self, ports: BootstrapPorts) {
        self.inner.bind_bootstrap(ports);
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        self.inner.control_handler()
    }
    async fn initialize(&mut self, context: &InitializeContext) -> anyhow::Result<()> {
        self.inner.initialize(context).await
    }
    async fn start(&mut self, context: &StartContext) -> anyhow::Result<()> {
        self.release.notified().await;
        self.inner.start(context).await
    }
    async fn pause(&mut self, context: &PauseContext) -> anyhow::Result<()> {
        self.inner.pause(context).await
    }
    async fn resume(&mut self, context: &ResumeContext) -> anyhow::Result<()> {
        self.inner.resume(context).await
    }
    async fn stop(&mut self, context: &StopContext) -> anyhow::Result<()> {
        self.inner.stop(context).await
    }
    async fn dispose(&mut self, context: &DisposeContext) -> anyhow::Result<()> {
        self.inner.dispose(context).await
    }
}

impl ComputationBootstrapper for DelayedProvider {}

#[tokio::test(flavor = "current_thread")]
async fn a_request_before_the_provider_starts_waits_for_it() {
    let (source, handle) = relay_source();
    let (seed, seeded) = provider("provider", vec![person("a", "Alice", 1)], None);
    let release = Arc::new(tokio::sync::Notify::new());
    let delayed = DelayedProvider {
        inner: seed,
        release: release.clone(),
    };
    let (left, left_results) = query("left", PEOPLE).await;
    let builder = ComputationGraph::builder("bootstrap-graph")
        .fixture_source(Box::new(source))
        .bootstrap_provider(fixture_support::component_name(), Box::new(delayed))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        );
    let mut graph = with_query(builder, "left", left)
        .connect(
            EdgeDefinition::new(
                endpoint("source", "snapshot"),
                endpoint("left", "bootstrap"),
            ),
            pipe(),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        // The source relays the consumer's request before the provider runs.
        eventually("relayed request", || !handle.requests().is_empty()).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rows(&left_results).is_empty());
        release.notify_one();
        wait_started(&control, &["provider", "left"]).await;
        assert_eq!(rows(&left_results), names(&["Alice"]));
        assert_eq!(seeded.sessions.lock().unwrap().len(), 1);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}
