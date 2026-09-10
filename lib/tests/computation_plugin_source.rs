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

#![cfg(feature = "computation")]
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{computation::v1::*, DrasiLib};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::{
    collections::{BTreeMap, HashSet},
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::mpsc;

struct Bootstrap {
    rows: Arc<Mutex<Vec<SourceChange>>>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl drasi_lib::bootstrap::BootstrapProvider for Bootstrap {
    async fn bootstrap(
        &self,
        request: drasi_lib::bootstrap::BootstrapRequest,
        context: &drasi_lib::bootstrap::BootstrapContext,
        sender: drasi_lib::channels::BootstrapEventSender,
        _: Option<&drasi_lib::SourceSubscriptionSettings>,
    ) -> anyhow::Result<drasi_lib::bootstrap::BootstrapResult> {
        assert!(request.node_labels.contains(&"Item".to_owned()));
        self.calls.fetch_add(1, Ordering::SeqCst);
        let rows = self.rows.lock().expect("rows").clone();
        let count = rows.len();
        for (sequence, change) in rows.into_iter().enumerate() {
            sender
                .send(drasi_lib::channels::BootstrapEvent {
                    source_id: context.source_id.clone(),
                    change,
                    timestamp: chrono::Utc::now(),
                    sequence: sequence as u64,
                })
                .await?;
        }
        Ok(drasi_lib::bootstrap::BootstrapResult {
            event_count: count,
            source_position: None,
        })
    }
}
fn row(id: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("app", id),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::from(serde_json::json!({"name":id})),
        },
    }
}
struct Sink {
    descriptor: ComponentDescriptor,
    output: mpsc::Sender<ChangeEnvelope>,
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
        self.output.send(input.envelope).await?;
        Ok(())
    }
}
fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("id")
}
fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("port")
}
fn endpoint(node: &str, name: &str) -> Endpoint {
    Endpoint::new(id(node), port(name))
}
fn resource(value: &str) -> ResourceId {
    ResourceId::try_new(value).expect("resource")
}
#[derive(Default)]
struct ApplicationFactory {
    input: Mutex<Option<drasi_source_application::ApplicationSourceHandle>>,
    durable: bool,
    subscription_control: bool,
    subscriptions: Arc<Mutex<Vec<drasi_lib::SourceSubscriptionSettings>>>,
    removed: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl SourcePluginConstructor for ApplicationFactory {
    async fn create(&self) -> anyhow::Result<Box<dyn drasi_lib::Source>> {
        let (source, input) = ApplicationSource::new(
            "app",
            ApplicationSourceConfig {
                properties: Default::default(),
                durability: self.durable.then(|| drasi_lib::DurabilityConfig {
                    enabled: true,
                    ..Default::default()
                }),
            },
        )?;
        *self.input.lock().expect("input") = Some(input);
        Ok(Box::new(TrackedApplication {
            source,
            subscriptions: self.subscriptions.clone(),
            removed: self.removed.clone(),
            subscription_control: self.subscription_control,
        }))
    }
}

struct TrackedApplication {
    source: ApplicationSource,
    subscriptions: Arc<Mutex<Vec<drasi_lib::SourceSubscriptionSettings>>>,
    removed: Arc<Mutex<Vec<String>>>,
    subscription_control: bool,
}

struct ControlBeforeData {
    control: Option<Arc<drasi_lib::channels::SourceEventWrapper>>,
    receiver: Box<dyn drasi_lib::channels::ChangeReceiver<drasi_lib::channels::SourceEventWrapper>>,
}
#[async_trait]
impl drasi_lib::channels::ChangeReceiver<drasi_lib::channels::SourceEventWrapper>
    for ControlBeforeData
{
    async fn recv(&mut self) -> anyhow::Result<Arc<drasi_lib::channels::SourceEventWrapper>> {
        if let Some(control) = self.control.take() {
            return Ok(control);
        }
        self.receiver.recv().await
    }
}
#[async_trait]
impl drasi_lib::Source for TrackedApplication {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn id(&self) -> &str {
        self.source.id()
    }
    fn type_name(&self) -> &str {
        self.source.type_name()
    }
    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        self.source.properties()
    }
    fn supports_replay(&self) -> bool {
        self.source.supports_replay()
    }
    async fn initialize(&self, context: drasi_lib::SourceRuntimeContext) {
        self.source.initialize(context).await;
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.source.start().await
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.source.stop().await
    }
    async fn status(&self) -> drasi_lib::ComponentStatus {
        self.source.status().await
    }
    async fn set_bootstrap_provider(
        &self,
        bootstrap: Box<dyn drasi_lib::bootstrap::BootstrapProvider>,
    ) {
        self.source.set_bootstrap_provider(bootstrap).await;
    }
    async fn subscribe(
        &self,
        settings: drasi_lib::SourceSubscriptionSettings,
    ) -> anyhow::Result<drasi_lib::channels::SubscriptionResponse> {
        self.subscriptions
            .lock()
            .expect("subscriptions")
            .push(settings.clone());
        let control = drasi_lib::channels::SourceEventWrapper {
            source_id: self.source.id().to_owned(),
            event: drasi_lib::channels::SourceEvent::Control(
                drasi_lib::channels::SourceControl::Subscription {
                    query_id: settings.query_id.clone(),
                    query_node_id: "subscription".into(),
                    node_labels: vec![],
                    rel_labels: vec![],
                    operation: drasi_lib::channels::ControlOperation::Insert,
                },
            ),
            timestamp: chrono::Utc::now(),
            profiling: None,
            sequence: None,
            source_position: None,
        };
        let mut response = self.source.subscribe(settings).await?;
        if self.subscription_control {
            response.receiver = Box::new(ControlBeforeData {
                control: Some(Arc::new(control)),
                receiver: response.receiver,
            });
        }
        Ok(response)
    }
    async fn remove_position_handle(&self, id: &str) {
        self.removed.lock().expect("removed").push(id.to_owned());
        self.source.remove_position_handle(id).await;
    }
    async fn on_subscriptions_complete(&self) {
        self.source.on_subscriptions_complete().await;
    }
}

async fn scenario() {
    let drasi = DrasiLib::builder()
        .with_id("plugin-source-instance")
        .build()
        .await
        .expect("instance");
    let services = drasi
        .computation_plugin_services("plugins")
        .expect("services");
    let factory = Arc::new(ApplicationFactory::default());
    let host = SourcePluginHost::recreatable(factory.clone(), services.clone())
        .await
        .expect("real source factory");
    let input = factory
        .input
        .lock()
        .expect("input")
        .clone()
        .expect("created handle");
    let rows = Arc::new(Mutex::new(vec![row("bootstrap")]));
    let calls = Arc::new(AtomicUsize::new(0));
    host.set_bootstrap_provider(Box::new(Bootstrap {
        rows: rows.clone(),
        calls: calls.clone(),
    }))
    .await
    .expect("bootstrap plugin");
    let progress = Arc::new(QuerySourceProgress::new("plugins", id("query")).expect("progress"));
    let stream = StreamId::try_new("app/out").expect("stream");
    let subscription = LegacySourceSubscription::new(
        host.clone(),
        SourceSubscriptionOptions {
            enable_bootstrap: true,
            nodes: HashSet::from(["Item".into()]),
            bootstrap_timeout_secs: 5,
            ..Default::default()
        },
        stream.clone(),
        Some(progress.clone()),
    )
    .expect("subscription");
    let bootstrap = LegacySourceBootstrap::new(vec![subscription.clone()]);
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "plugins".into(),
            id: id("query"),
            query: "MATCH (n:Item) RETURN n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out").expect("stream"),
            outbox_capacity: NonZeroUsize::new(16).expect("capacity"),
        },
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )
    .await
    .expect("query")
    .with_source_progress(progress.clone())
    .expect("progress")
    .with_bootstrap(bootstrap.clone());
    let results = query.results();
    let (sender, mut output) = mpsc::channel(8);
    let source_factory = Arc::new(SourcePluginAdapterFactory::default());
    let source_descriptor = ComponentDescriptor::try_new(
        id("source"),
        vec![PortDescriptor::new(
            port("out"),
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("descriptor");
    let mut builder = services
        .declare(ComputationGraph::builder("plugins"))
        .expect("services")
        .component(
            ComponentSpecification {
                descriptor: source_descriptor,
                role: ComponentRole::Source,
                completion: None,
                implementation: source_factory.descriptor().implementation.clone(),
                configuration_version: 1,
                configuration: BTreeMap::from([(
                    Arc::from("stream"),
                    ConfigurationValue::Literal("app/out".into()),
                )]),
                dependencies: [
                    (Arc::from("source"), vec![resource("host")]),
                    (Arc::from("subscription"), vec![resource("subscription")]),
                    (Arc::from("progress"), vec![resource("progress")]),
                    (Arc::from("bootstrap"), vec![resource("source-bootstrap")]),
                ]
                .into_iter()
                .chain(services.dependencies())
                .collect(),
            },
            source_factory,
        )
        .query(Box::new(query))
        .sink(Box::new(Sink {
            descriptor: ComponentDescriptor::try_new(
                id("sink"),
                vec![PortDescriptor::new(
                    port("in"),
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("sink"),
            output: sender,
        }))
        .bind_stream(endpoint("source", "out"), stream)
        .bind_stream(
            endpoint("query", "out"),
            StreamId::try_new("query/out").expect("stream"),
        )
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
            Box::new(BoundedPipeConfig { capacity: 8 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
            Box::new(BoundedPipeConfig { capacity: 8 }),
        )
        .relationship_policy(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
            RelationshipPolicy {
                activation: ActivationCoupling::RequiresRunning,
                rebind_on_consumer_replace: true,
                ..Default::default()
            },
        );
    for (name, role, ownership, handle) in [
        (
            "source-bootstrap",
            ResourceRole::Bootstrap,
            ResourceOwnership::Borrowed,
            host.bootstrap_resource()
                .expect("bootstrap")
                .expect("provider"),
        ),
        (
            "host",
            ResourceRole::LegacySource,
            ResourceOwnership::Graph,
            host.resource(),
        ),
        (
            "subscription",
            ResourceRole::SourceSubscription,
            ResourceOwnership::Graph,
            ResourceHandle::new(ResourceRole::SourceSubscription, subscription.clone())
                .with_cleanup(subscription),
        ),
        (
            "progress",
            ResourceRole::Checkpoint,
            ResourceOwnership::Borrowed,
            ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(QuerySourceProgressResource(progress)),
            ),
        ),
        (
            "bootstrap",
            ResourceRole::Bootstrap,
            ResourceOwnership::Borrowed,
            ResourceHandle::new(
                ResourceRole::Bootstrap,
                Arc::new(QueryBootstrapResource(bootstrap)),
            ),
        ),
    ] {
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource(name),
                role,
                ownership,
                binding: Arc::from(name),
            })
            .expect("declare")
            .provide_resource(resource(name), handle)
            .expect("provide");
    }
    let graph = builder.build().expect("graph");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "creation does not execute bootstrap"
    );
    let managed = drasi
        .add_computation_graph(graph, ComputationOptions { auto_start: false })
        .await
        .expect("register");
    assert_eq!(
        managed.start().await.expect("start").summary,
        OperationSummary::Completed
    );
    assert_eq!(results.snapshot().expect("bootstrapped").rows.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    rows.lock().expect("rows").push(row("live"));
    input
        .send_node_insert(
            "live",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("name", "live")
                .build(),
        )
        .await
        .expect("real source live input");
    let live = output.recv().await.expect("native query output");
    assert_eq!(live.system().sequence(), 1);
    assert_eq!(results.snapshot().expect("live rows").rows.len(), 2);
    managed.stop().await.expect("soft stop");
    let restarted = managed.start().await.expect("restart");
    assert_eq!(
        restarted.summary,
        OperationSummary::Completed,
        "{restarted:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "volatile state is explicitly re-bootstrapped"
    );
    assert_eq!(results.snapshot().expect("rebuilt rows").rows.len(), 2);
    let input = factory
        .input
        .lock()
        .expect("input")
        .clone()
        .expect("reconstructed handle");
    input
        .send_node_insert(
            "after",
            vec!["Item"],
            PropertyMapBuilder::new()
                .with_string("name", "after")
                .build(),
        )
        .await
        .expect("input after restart");
    assert_eq!(
        output
            .recv()
            .await
            .expect("continued output")
            .system()
            .sequence(),
        2
    );
    drasi.shutdown().await.expect("all plugin work stopped");
}
#[tokio::test(flavor = "current_thread")]
async fn real_source_bootstrap_live_and_restart_current_thread() {
    tokio::time::timeout(Duration::from_secs(20), scenario())
        .await
        .expect("adapter deadlock");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_source_bootstrap_live_and_restart_multi_thread() {
    tokio::time::timeout(Duration::from_secs(20), scenario())
        .await
        .expect("adapter deadlock");
}

async fn receive_queries(
    output: &mut tokio::sync::broadcast::Receiver<ChangeEnvelope>,
    sequence: u64,
) {
    let mut queries = std::collections::BTreeSet::new();
    for _ in 0..2 {
        let envelope = output.recv().await.expect("both queries publish");
        assert_eq!(envelope.system().sequence(), sequence);
        queries.insert(
            QueryChangeCodec::metadata(&envelope)
                .expect("metadata")
                .query_id,
        );
    }
    assert_eq!(
        queries,
        std::collections::BTreeSet::from(["first".into(), "second".into()])
    );
}

async fn persistent_shared_source() {
    use drasi_lib::wal::WalProvider;
    let temp = tempfile::tempdir().expect("temp");
    let wal = Arc::new(drasi_wal_redb::RedbWalProvider::new(
        temp.path().join("wal"),
    ));
    let backend = Arc::new(drasi_index_rocksdb::RocksDbIndexProvider::new(
        temp.path().join("indexes"),
        false,
        false,
    ));
    let drasi = DrasiLib::builder()
        .with_id("durable-plugin-instance")
        .with_wal_provider(wal.clone())
        .with_default_index_provider("persistent", backend)
        .build()
        .await
        .expect("instance");
    let factory = Arc::new(ApplicationFactory {
        durable: true,
        ..Default::default()
    });
    for generation in 0..2 {
        let pipeline = drasi.computation_pipeline("durable").expect("pipeline");
        let services = pipeline.services();
        let catalog = pipeline.catalog();
        let mut output = catalog.subscribe();
        let host = SourcePluginHost::recreatable(factory.clone(), services.clone())
            .await
            .expect("source");
        let mut pipeline = pipeline
            .source(host, SourceSubscriptionOptions::default())
            .expect("source binding");
        for query in ["first", "second"] {
            pipeline = pipeline.query(
                drasi_lib::Query::cypher(query)
                    .query("MATCH (n:Item) RETURN n.name AS name")
                    .from_source("app")
                    .enable_bootstrap(false)
                    .build(),
            );
        }
        let managed = drasi
            .add_computation_graph(
                pipeline.build().expect("graph"),
                ComputationOptions { auto_start: false },
            )
            .await
            .expect("register");
        let start = managed.start().await.expect("start");
        assert_eq!(start.summary, OperationSummary::Completed, "{start:?}");
        if generation == 0 {
            let input = factory
                .input
                .lock()
                .expect("input")
                .clone()
                .expect("handle");
            input
                .send_node_insert(
                    "first",
                    vec!["Item"],
                    PropertyMapBuilder::new()
                        .with_string("name", "first")
                        .build(),
                )
                .await
                .expect("first input");
            receive_queries(&mut output, 1).await;
            managed.stop().await.expect("stop both subscriptions");
            assert_eq!(factory.removed.lock().expect("removed").len(), 2);
            assert_eq!(
                services
                    .wal
                    .as_ref()
                    .expect("WAL")
                    .append("app", &row("offline"))
                    .await
                    .expect("offline WAL event"),
                2
            );
            let report = managed.start().await.expect("resume");
            assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
            receive_queries(&mut output, 2).await;
            let settings = factory.subscriptions.lock().expect("settings").clone();
            assert_eq!(settings.len(), 4);
            for setting in &settings[2..] {
                assert_eq!(
                    setting.resume_from,
                    Some(1_u64.to_be_bytes().to_vec().into())
                );
                assert_eq!(setting.resume_sequence, Some(1));
                assert!(setting.request_position_handle);
            }
            let input = factory
                .input
                .lock()
                .expect("input")
                .clone()
                .expect("new handle");
            input
                .send_node_insert(
                    "after",
                    vec!["Item"],
                    PropertyMapBuilder::new()
                        .with_string("name", "after")
                        .build(),
                )
                .await
                .expect("after resume");
            receive_queries(&mut output, 3).await;
        } else {
            assert_eq!(
                catalog
                    .snapshot("first", Duration::from_secs(5))
                    .await
                    .expect("recovered")
                    .rows
                    .len(),
                3
            );
            let settings = factory.subscriptions.lock().expect("settings").clone();
            for setting in &settings[settings.len() - 2..] {
                assert_eq!(
                    setting.resume_from,
                    Some(3_u64.to_be_bytes().to_vec().into())
                );
                assert_eq!(setting.resume_sequence, Some(3));
            }
            let input = factory
                .input
                .lock()
                .expect("input")
                .clone()
                .expect("reconstructed handle");
            input
                .send_node_insert(
                    "last",
                    vec!["Item"],
                    PropertyMapBuilder::new()
                        .with_string("name", "last")
                        .build(),
                )
                .await
                .expect("last input");
            receive_queries(&mut output, 4).await;
        }
        assert!(matches!(
            wal.head_sequence("app").await,
            Err(drasi_lib::wal::WalError::SourceNotRegistered(_))
        ));
        drasi
            .remove_computation_graph("durable")
            .await
            .expect("dispose native graph");
    }
    drasi.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_wal_plugin_replays_shared_source_and_recovers_reconstructed_native_queries() {
    tokio::time::timeout(Duration::from_secs(30), persistent_shared_source())
        .await
        .expect("persistent handoff deadlock");
}

async fn volatile_restart(subscription_control: bool) {
    let drasi = DrasiLib::builder().build().await.expect("instance");
    let pipeline = drasi.computation_pipeline("volatile").expect("pipeline");
    let factory = Arc::new(ApplicationFactory {
        subscription_control,
        ..Default::default()
    });
    let source = SourcePluginHost::recreatable(factory.clone(), pipeline.services())
        .await
        .expect("source");
    let catalog = pipeline.catalog();
    let mut results = catalog.subscribe();
    let graph = pipeline
        .source(source, SourceSubscriptionOptions::default())
        .expect("bind source")
        .query(
            drasi_lib::Query::cypher("query")
                .query("MATCH (n:Item) RETURN n.name AS name")
                .from_source("app")
                .enable_bootstrap(false)
                .build(),
        )
        .build()
        .expect("graph");
    let handle = drasi
        .add_computation_graph(graph, ComputationOptions { auto_start: false })
        .await
        .expect("register");
    for epoch in 0..2 {
        assert_eq!(
            handle.start().await.expect("start").summary,
            OperationSummary::Completed
        );
        let input = factory
            .input
            .lock()
            .expect("input")
            .clone()
            .expect("handle");
        for sequence in epoch * 2 + 1..=epoch * 2 + 2 {
            input
                .send_node_insert(
                    format!("item-{sequence}"),
                    vec!["Item"],
                    PropertyMapBuilder::new()
                        .with_string("name", format!("name-{sequence}"))
                        .build(),
                )
                .await
                .expect("input");
            let event = results.recv().await.expect("new data is not deduplicated");
            assert_eq!(event.system().sequence(), sequence);
        }
        handle.stop().await.expect("stop");
    }
    let settings = factory.subscriptions.lock().expect("subscriptions").clone();
    assert_eq!(settings.len(), 2);
    assert_eq!(settings[1].resume_sequence, Some(2));
    assert!(
        settings[1].resume_from.is_none(),
        "raising a sequence floor is not positional replay"
    );
    drasi.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn recreated_volatile_source_does_not_reuse_raw_sequences_without_bootstrap_or_replay() {
    tokio::time::timeout(Duration::from_secs(5), volatile_restart(false))
        .await
        .expect("new events lost after restart");
}

#[tokio::test]
async fn subscription_control_events_are_ignored_before_encoding_subsequent_data() {
    tokio::time::timeout(Duration::from_secs(5), volatile_restart(true))
        .await
        .expect("control event killed data subscription");
}
