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

//! Ordinary queries over a native source with two graph outputs. The first
//! declared output ("audit") carries `Audit` nodes and the second ("data")
//! carries `Item` nodes, each with its own snapshot, live emissions and
//! producer identity. A query that bound the first port by accident would read
//! the wrong dataset, so these tests distinguish the factory-designated primary
//! and explicitly selected outputs from a first-port guess.

#![cfg(test)]

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{computation::v1::*, DrasiLib, Query};
use serde_json::json;
use tokio::sync::mpsc;

const SCOPE: &str = "query-input-ports";

fn component(id: &str) -> ComponentId {
    ComponentId::try_new(id).expect("component id")
}

fn port(id: &str) -> PortId {
    PortId::try_new(id).expect("port id")
}

fn endpoint(component_id: &str, port_id: &str) -> Endpoint {
    Endpoint::new(component(component_id), port(port_id))
}

fn stream(component_id: &str, port_id: &str) -> StreamId {
    StreamId::try_new(format!("{component_id}/{port_id}")).expect("stream id")
}

/// `Audit` nodes are on the first declared port, `Item` nodes on the second.
fn label(port_id: &str) -> &'static str {
    match port_id {
        "audit" => "Audit",
        "data" => "Item",
        other => panic!("unexpected fixture port {other}"),
    }
}

struct LiveEvent {
    port: &'static str,
    key: &'static str,
    value: i64,
    /// `SystemMetadata.timestamp` is optional; `None` is a valid emission.
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

struct PortState {
    producer: GraphProducerIdentity,
    rows: BTreeMap<String, i64>,
    /// Logical position of the output's content, including seeded data.
    sequence: u64,
    /// Transport sequence of this output's stream: its first emission is 1,
    /// whatever the logical position at that time.
    emitted: u64,
}

/// The fixture's own per-output state. Providers derive graph records from it
/// on demand; the source emits live events from the same state.
struct Fixture {
    component: String,
    ports: Mutex<BTreeMap<&'static str, PortState>>,
    events: tokio::sync::Mutex<mpsc::Receiver<LiveEvent>>,
    sender: mpsc::Sender<LiveEvent>,
    creates: AtomicUsize,
}

type Failure = OutputReadError;
type ReadResult<T> = std::result::Result<T, Failure>;

impl Fixture {
    fn new(component_id: &str) -> Arc<Self> {
        let (sender, events) = mpsc::channel(8);
        let ports = ["audit", "data"]
            .into_iter()
            .map(|name| {
                let producer = GraphProducerIdentity::volatile(
                    SCOPE.into(),
                    SCOPE.into(),
                    component(component_id),
                    stream(component_id, name),
                )
                .expect("producer identity");
                (
                    name,
                    PortState {
                        producer,
                        rows: BTreeMap::new(),
                        sequence: 0,
                        emitted: 0,
                    },
                )
            })
            .collect();
        Arc::new(Self {
            component: component_id.into(),
            ports: Mutex::new(ports),
            events: tokio::sync::Mutex::new(events),
            sender,
            creates: AtomicUsize::new(0),
        })
    }

    /// Data that exists before any query starts; it is only visible through the
    /// output's snapshot.
    fn seed(&self, port_id: &'static str, key: &str, value: i64) {
        let mut ports = self.ports.lock().expect("fixture state");
        let state = ports.get_mut(port_id).expect("fixture port");
        state.sequence += 1;
        state.rows.insert(key.into(), value);
    }

    async fn send(&self, port_id: &'static str, key: &'static str, value: i64) {
        self.send_at(port_id, key, value, Some(chrono::Utc::now()))
            .await;
    }

    /// An emission whose envelope carries no source timestamp.
    async fn send_untimed(&self, port_id: &'static str, key: &'static str, value: i64) {
        self.send_at(port_id, key, value, None).await;
    }

    async fn send_at(
        &self,
        port_id: &'static str,
        key: &'static str,
        value: i64,
        timestamp: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        self.sender
            .send(LiveEvent {
                port: port_id,
                key,
                value,
                timestamp,
            })
            .await
            .expect("fixture source is running");
    }

    #[allow(clippy::too_many_arguments)]
    fn insert(
        &self,
        port_id: &str,
        key: &str,
        value: i64,
        effective_from: u64,
        sequence: u64,
        timestamp: Option<chrono::DateTime<chrono::Utc>>,
        producer: &GraphProducerIdentity,
    ) -> ChangeEnvelope {
        GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(&self.component, key),
                        labels: Arc::from(vec![Arc::<str>::from(label(port_id))]),
                        effective_from,
                    },
                    properties: ElementPropertyMap::from(json!({ "value": value })),
                },
            },
            producer.stream().clone(),
            sequence,
            timestamp,
        )
        .expect("graph-change envelope")
    }

    fn emit(&self, event: LiveEvent) -> anyhow::Result<OutputEnvelope> {
        let mut ports = self.ports.lock().expect("fixture state");
        let state = ports.get_mut(event.port).expect("fixture port");
        state.sequence += 1;
        state.emitted += 1;
        state.rows.insert(event.key.into(), event.value);
        let mut envelope = self.insert(
            event.port,
            event.key,
            event.value,
            state.sequence,
            state.emitted,
            event.timestamp,
            &state.producer,
        );
        GraphProducerProgress::annotate(&mut envelope, &state.producer, state.sequence)?;
        Ok(OutputEnvelope {
            port: port(event.port),
            envelope,
        })
    }

    fn snapshot(&self, port_id: &'static str) -> ReadResult<OutputSnapshot> {
        let ports = self.ports.lock().expect("fixture state");
        let state = &ports[port_id];
        let records: Vec<_> = state
            .rows
            .iter()
            .map(|(key, value)| {
                let envelope = self.insert(port_id, key, *value, 1, 1, None, &state.producer);
                let ChangeOperation::Added { after, .. } = &envelope.changes().operations()[0]
                else {
                    panic!("fixture inserts are additions");
                };
                (after.identity().clone(), after.clone())
            })
            .collect();
        OutputSnapshot::try_new(
            OutputCursor::try_new(state.producer.clone(), state.sequence)?,
            format!("{}/{port_id}", self.component),
            GraphChangeCodec::schema().descriptor().clone(),
            records.into_iter().collect(),
        )
    }
}

struct PortSnapshot {
    fixture: Arc<Fixture>,
    port: &'static str,
}

#[async_trait]
impl SnapshotProvider for PortSnapshot {
    async fn snapshot(&self) -> ReadResult<OutputSnapshot> {
        self.fixture.snapshot(self.port)
    }
}

struct FixtureSource {
    descriptor: ComponentDescriptor,
    fixture: Arc<Fixture>,
}

#[async_trait]
impl ComputationComponent for FixtureSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn output_readers(&self) -> OutputReaders {
        ["audit", "data"]
            .into_iter()
            .fold(OutputReaders::new(), |readers, name| {
                readers.with_snapshot(
                    port(name),
                    Arc::new(PortSnapshot {
                        fixture: self.fixture.clone(),
                        port: name,
                    }),
                )
            })
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for FixtureSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let mut events = self.fixture.events.lock().await;
        match events.recv().await {
            Some(event) => self.fixture.emit(event).map(Some),
            None => Ok(None),
        }
    }
}

type Received = Arc<Mutex<Vec<ChangeEnvelope>>>;

struct FixtureSink {
    descriptor: ComponentDescriptor,
    received: Received,
}

#[async_trait]
impl ComputationComponent for FixtureSink {
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
impl EnvelopeSink for FixtureSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.received.lock().expect("sink").push(input.envelope);
        Ok(())
    }
}

fn graph_port(id: &str, direction: PortDirection) -> PortDescriptor {
    PortDescriptor::new(
        port(id),
        direction,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    )
}

/// A native source factory with two graph outputs, `audit` before `data`.
struct SourceFactory {
    descriptor: FactoryDescriptor,
    primaries: PortPrimaries,
    fixture: Arc<Fixture>,
}

impl SourceFactory {
    fn new(fixture: Arc<Fixture>, primaries: PortPrimaries) -> Arc<Self> {
        Arc::new(Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test/two-output-source", "1")
                    .expect("implementation"),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema::default(),
                dependencies: BTreeMap::new(),
            },
            primaries,
            fixture,
        })
    }

    fn specification(&self) -> ComponentSpecification {
        ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(
                component(&self.fixture.component),
                vec![
                    graph_port("audit", PortDirection::Output),
                    graph_port("data", PortDirection::Output),
                ],
            )
            .expect("descriptor"),
            role: ComponentRole::Source,
            completion: None,
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        }
    }
}

#[async_trait]
impl ComponentFactory for SourceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn primary_ports(&self) -> PortPrimaries {
        self.primaries.clone()
    }

    fn validate(&self, specification: &ComponentSpecification) -> anyhow::Result<()> {
        anyhow::ensure!(
            specification.descriptor == self.specification().descriptor,
            "unexpected two-output source interface"
        );
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        self.fixture.creates.fetch_add(1, Ordering::SeqCst);
        Ok(ConstructedComponent::source(Box::new(FixtureSource {
            descriptor: context.specification.descriptor.clone(),
            fixture: self.fixture.clone(),
        })))
    }
}

struct Installed {
    core: Arc<DrasiLib>,
    fixture: Arc<Fixture>,
    audit_received: Received,
    data_received: Received,
}

fn primary(port_id: &str) -> PortPrimaries {
    PortPrimaries::new(None, Some(port(port_id)))
}

/// Install `native` with an `audit` consumer and a `data` consumer: every
/// declared output of the source stays connected, as the architecture requires.
async fn install(primaries: PortPrimaries) -> Installed {
    let fixture = Fixture::new("native");
    fixture.seed("audit", "a1", 10);
    fixture.seed("data", "i1", 1);
    fixture.seed("data", "i2", 2);
    let audit_received = Received::default();
    let data_received = Received::default();
    let factory = SourceFactory::new(fixture.clone(), primaries);
    let mut batch = ComponentBatch::builder().component(factory.specification(), factory);
    for (name, received) in [("audit", &audit_received), ("data", &data_received)] {
        let sink = format!("{name}-sink");
        batch = batch
            .sink(Box::new(FixtureSink {
                descriptor: ComponentDescriptor::try_new(
                    component(&sink),
                    vec![graph_port("in", PortDirection::Input)],
                )
                .expect("sink descriptor"),
                received: received.clone(),
            }))
            .bind_stream(endpoint("native", name), stream("native", name))
            .connect(
                EdgeDefinition::new(endpoint("native", name), endpoint(&sink, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            );
    }
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("query-input-ports")
            .build()
            .await
            .expect("instance"),
    );
    let report = core
        .add_components(batch.build().expect("batch").auto_start(false))
        .await
        .expect("add components");
    assert!(report.committed, "{report:?}");
    core.computation_component("native")
        .expect("native source")
        .wait_created()
        .await
        .expect("source construction");
    Installed {
        core,
        fixture,
        audit_received,
        data_received,
    }
}

impl Installed {
    async fn start_components(&self) {
        for id in ["audit-sink", "data-sink", "native"] {
            self.core
                .computation_component(id)
                .expect("component")
                .start()
                .await
                .unwrap_or_else(|error| panic!("start {id}: {error:#}"));
        }
    }

    async fn start_query(&self, id: &str) {
        self.core
            .start_query(id)
            .await
            .unwrap_or_else(|error| panic!("start query {id}: {error:#}"));
        self.core
            .computation_component(id)
            .expect("query component")
            .wait_started()
            .await
            .unwrap_or_else(|error| panic!("query {id} did not start: {error:#}"));
    }

    /// The data endpoints this query's input edges actually bind. The query's
    /// own scheduled-work helper (`scheduled/...`) is infrastructure, not input.
    fn upstream(&self, query: &str) -> Vec<(String, String)> {
        let snapshot = self.core.computation_control().unwrap().desired_snapshot();
        let mut upstream: Vec<_> = snapshot
            .edges
            .iter()
            .filter(|edge| edge.definition.to.component.as_str() == query)
            .filter(|edge| {
                !edge
                    .definition
                    .from
                    .component
                    .as_str()
                    .starts_with("scheduled/")
            })
            .map(|edge| {
                (
                    edge.definition.from.component.to_string(),
                    edge.definition.from.port.to_string(),
                )
            })
            .collect();
        upstream.sort();
        upstream
    }

    /// The recorded failure, if any, of each named component.
    fn failures(&self, ids: &[&str]) -> Vec<(String, Option<String>)> {
        let observed = self.core.computation_control().unwrap().observed();
        ids.iter()
            .map(|id| {
                let failure = observed
                    .components
                    .get(&component(id))
                    .and_then(|node| node.failure.as_ref())
                    .map(|failure| format!("{failure:?}"));
                (id.to_string(), failure)
            })
            .collect()
    }

    async fn values(&self, query: &str) -> Vec<i64> {
        let mut values: Vec<_> = self
            .core
            .get_query_results(query)
            .await
            .unwrap_or_else(|error| panic!("results of {query}: {error:#}"))
            .iter()
            .map(|row| row["value"].as_i64().expect("integer result"))
            .collect();
        values.sort_unstable();
        values
    }

    async fn wait_for(&self, query: &str, expected: &[i64]) {
        let outcome = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self.values(query).await == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            outcome.is_ok(),
            "query {query} never reached {expected:?}; it has {:?}; status {:?}; observed {:?}",
            self.values(query).await,
            self.core.get_query_status(query).await,
            self.core
                .computation_control()
                .unwrap()
                .observed()
                .relationships
                .iter()
                .filter(|(edge, _)| edge.to.component.as_str() == query)
                .collect::<Vec<_>>()
        );
    }

    /// Return the failure of declaring or starting `config`. Admission may keep
    /// a failed declaration, so the failure may surface on start instead of add.
    async fn failure(&self, config: drasi_lib::config::QueryConfig) -> String {
        let id = config.id.clone();
        if let Err(error) = self.core.add_query(config).await {
            return format!("{error:#}");
        }
        let started = self.core.start_query(&id).await;
        let failure = match started {
            Err(error) => format!("{error:#}"),
            Ok(()) => {
                let waited = self
                    .core
                    .computation_component(&id)
                    .expect("query component")
                    .wait_started()
                    .await;
                match waited {
                    Err(error) => format!("{error:#}"),
                    Ok(_) => panic!("query {id} started with an unresolvable input"),
                }
            }
        };
        assert!(
            self.core
                .get_query_results(&id)
                .await
                .map_or(true, |rows| rows.is_empty()),
            "query {id} must not process data"
        );
        failure
    }
}

fn query(id: &str, label: &str) -> Query {
    Query::cypher(id)
        .query(format!("MATCH (n:{label}) RETURN n.value AS value"))
        .auto_start(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn primary_and_explicit_outputs_bind_and_read_their_own_data() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let installed = install(primary("data")).await;
        let core = &installed.core;
        for config in [
            // No port: the designated primary (`data`), not the first declared output.
            query("implicit-items", "Item").from_input("native").build(),
            query("implicit-audits", "Audit")
                .from_input("native")
                .build(),
            query("audit-audits", "Audit")
                .from_input_port("native", "audit")
                .build(),
            query("audit-items", "Item")
                .from_input_port("native", "audit")
                .build(),
            query("data-items", "Item")
                .from_input_port("native", "data")
                .with_result_label("DataItem")
                .build(),
        ] {
            core.add_query(config).await.expect("add query");
        }
        installed.start_components().await;
        for id in ["implicit-items", "implicit-audits", "audit-audits", "audit-items", "data-items"]
        {
            installed.start_query(id).await;
        }

        // Each query reads exactly one output of the source.
        for (name, selected) in [
            ("implicit-items", "data"),
            ("implicit-audits", "data"),
            ("audit-audits", "audit"),
            ("audit-items", "audit"),
            ("data-items", "data"),
        ] {
            assert_eq!(
                installed.upstream(name),
                [("native".to_string(), selected.to_string())],
                "{name}"
            );
        }

        // Bootstrap comes from the selected output's own snapshot.
        installed.wait_for("implicit-items", &[1, 2]).await;
        installed.wait_for("data-items", &[1, 2]).await;
        installed.wait_for("audit-audits", &[10]).await;
        assert_eq!(installed.values("implicit-audits").await, Vec::<i64>::new());
        assert_eq!(installed.values("audit-items").await, Vec::<i64>::new());

        // Live data follows the same selected output.
        installed.fixture.send("audit", "a2", 20).await;
        installed.fixture.send("data", "i3", 3).await;
        installed.wait_for("implicit-items", &[1, 2, 3]).await;
        installed.wait_for("data-items", &[1, 2, 3]).await;
        installed.wait_for("audit-audits", &[10, 20]).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(installed.values("implicit-audits").await, Vec::<i64>::new());
        assert_eq!(installed.values("audit-items").await, Vec::<i64>::new());

        // The fixed output schemas and the other subscribers are unaffected.
        for (received, name) in
            [(&installed.audit_received, "audit"), (&installed.data_received, "data")]
        {
            tokio::time::timeout(Duration::from_secs(10), async {
                while received.lock().unwrap().is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("sink receives its own output");
            let received = received.lock().unwrap();
            assert_eq!(received.len(), 1, "{name}");
            assert_eq!(received[0].system().stream(), &stream("native", name));
            assert_eq!(
                received[0].changes().schema(),
                GraphChangeCodec::schema().descriptor()
            );
        }
        assert_eq!(installed.fixture.creates.load(Ordering::SeqCst), 1);
        core.shutdown().await.expect("shutdown");
    })
    .await
    .expect("query input ports stalled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_outputs_without_a_primary_require_an_explicit_port() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let installed = install(PortPrimaries::default()).await;
        let failure = installed
            .failure(query("ambiguous", "Item").from_input("native").build())
            .await;
        assert!(failure.contains("ambiguous Output port"), "{failure}");
        assert!(
            failure.contains(r#"[PortId("audit"), PortId("data")]"#),
            "{failure}"
        );

        // The same source works once the port is named, and the rejected query
        // never consumed or disturbed it.
        installed
            .core
            .add_query(
                query("named", "Item")
                    .from_input_port("native", "data")
                    .build(),
            )
            .await
            .expect("add named query");
        installed.start_components().await;
        installed.start_query("named").await;
        assert_eq!(
            installed.upstream("named"),
            [("native".to_string(), "data".to_string())]
        );
        installed.wait_for("named", &[1, 2]).await;
        assert!(installed.upstream("ambiguous").is_empty());
        installed.core.shutdown().await.expect("shutdown");
    })
    .await
    .expect("ambiguous input stalled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_and_wrong_direction_ports_fail_before_processing() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let installed = install(primary("data")).await;
        installed
            .core
            .add_query(query("upstream", "Item").from_input("native").build())
            .await
            .expect("add upstream query");
        installed.start_components().await;
        installed.start_query("upstream").await;
        installed.wait_for("upstream", &[1, 2]).await;

        // A source has no input port, and a query's `in` is not an output.
        for (config, needle) in [
            (
                query("unknown", "Item")
                    .from_input_port("native", "missing")
                    .build(),
                r#"no Output port PortId("missing") is declared (declared Output ports: [PortId("audit"), PortId("data")])"#,
            ),
            (
                query("source-input", "Item")
                    .from_input_port("native", "in")
                    .build(),
                r#"no Output port PortId("in") is declared (declared Output ports: [PortId("audit"), PortId("data")])"#,
            ),
            (
                query("wrong-direction", "Item")
                    .from_input_port("upstream", "in")
                    .build(),
                r#"no Output port PortId("in") is declared (declared Output ports: [PortId("out")])"#,
            ),
        ] {
            let id = config.id.clone();
            let failure = installed.failure(config).await;
            assert!(failure.contains(needle), "{id}: {failure}");
            assert!(installed.upstream(&id).is_empty(), "{id}");
        }

        // The healthy query and its source are unaffected by the failures.
        installed.fixture.send("data", "i3", 3).await;
        installed.wait_for("upstream", &[1, 2, 3]).await;
        installed.core.shutdown().await.expect("shutdown");
    })
    .await
    .expect("invalid ports stalled");
}

/// `SystemMetadata.timestamp` is optional, so a native source may emit live
/// data without a source time. An ordinary query must process it like timed
/// data. The checks are ordered so a rejected send, a failed source or a failed
/// query is reported as such rather than as a quiet query.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untimed_native_input_is_processed_after_the_snapshot() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let installed = install(primary("data")).await;
        installed
            .core
            .add_query(query("untimed", "Item").from_input("native").build())
            .await
            .expect("add query");
        installed.start_components().await;
        installed.start_query("untimed").await;
        installed.wait_for("untimed", &[1, 2]).await;

        installed.fixture.send_untimed("data", "i3", 3).await;

        // The source accepted the emission and its other subscriber received it.
        let delivered = tokio::time::timeout(Duration::from_secs(5), async {
            while installed.data_received.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            delivered.is_ok(),
            "the untimed envelope was never delivered; failures: {:?}",
            installed.failures(&["native", "data-sink", "untimed"])
        );
        {
            let received = installed.data_received.lock().unwrap();
            assert_eq!(received.len(), 1);
            assert_eq!(received[0].system().timestamp(), None);
        }
        let failures = installed.failures(&["native", "data-sink", "untimed"]);
        assert!(
            failures.iter().all(|(_, failure)| failure.is_none()),
            "a component failed on untimed input: {failures:?}"
        );

        let processed = tokio::time::timeout(Duration::from_secs(5), async {
            while installed.values("untimed").await != [1, 2, 3] {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            processed.is_ok(),
            "the query did not process untimed native input: it has {:?}, status {:?}, failures {:?}",
            installed.values("untimed").await,
            installed.core.get_query_status("untimed").await,
            installed.failures(&["native", "untimed"])
        );
        installed.core.shutdown().await.expect("shutdown");
    })
    .await
    .expect("untimed input stalled");
}
