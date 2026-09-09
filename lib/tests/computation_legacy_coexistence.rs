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

use std::{
    collections::HashMap,
    future::pending,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    computation::v1::{
        BoundedPipeConfig, ChangeSet, ChangeSetId, ComponentDescriptor, ComponentId,
        ComputationComponent, ComputationGraph, EdgeDefinition, Endpoint, Envelope, EnvelopeId,
        EnvelopeSink, EnvelopeSource, GraphError, GraphState, InputEnvelope, OutputEnvelope,
        PipeRequirements, PortDescriptor, PortDirection, PortId, SchemaDescriptor, SchemaId,
        SchemaVersion, SinkCompletion, StreamId, SystemMetadata,
    },
    ComponentStatus, DrasiLib, Query, Reaction, ReactionBase, ReactionBaseParams,
    ReactionRuntimeContext, Source, SourceBase, SourceBaseParams, SourceRuntimeContext,
    SourceSubscriptionSettings, SubscriptionResponse,
};
use tokio::sync::mpsc;

const LEGACY_SOURCE: &str = "legacy-source";
const LEGACY_QUERY: &str = "legacy-query";
const LEGACY_REACTION: &str = "legacy-reaction";

#[derive(Clone)]
struct InjectableSource {
    base: Arc<SourceBase>,
}

#[async_trait]
impl Source for InjectableSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "coexistence-injectable"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn supports_replay(&self) -> bool {
        false
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, self.type_name())
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct CapturingReaction {
    base: ReactionBase,
    results: mpsc::Sender<QueryResult>,
    loop_stops: Arc<AtomicUsize>,
}

#[async_trait]
impl Reaction for CapturingReaction {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "coexistence-capturing"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.get_queries().to_vec()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        let shutdown = self.base.create_shutdown_channel().await;
        let base = self.base.clone_shared();
        let results = self.results.clone();
        let loop_stops = self.loop_stops.clone();
        let task = tokio::spawn(async move {
            base.run_standard_loop(shutdown, HashMap::new(), move |result| {
                let results = results.clone();
                async move {
                    results
                        .send((*result).clone())
                        .await
                        .map_err(|_| anyhow::anyhow!("legacy result receiver closed"))
                }
            })
            .await
            .expect("legacy reaction consumer loop");
            loop_stops.fetch_add(1, Ordering::SeqCst);
        });
        self.base.set_processing_task(task).await;
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }
}

struct NativeSource {
    descriptor: ComponentDescriptor,
    event: Option<OutputEnvelope>,
    stops: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputationComponent for NativeSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for NativeSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        if let Some(event) = self.event.take() {
            return Ok(Some(event));
        }
        // Idle is not exhaustion: the caller-polled graph stays active until cancelled.
        pending().await
    }
}

struct NativeSink {
    descriptor: ComponentDescriptor,
    handled: mpsc::Sender<InputEnvelope>,
    stops: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputationComponent for NativeSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for NativeSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.handled
            .send(input)
            .await
            .map_err(|_| anyhow::anyhow!("native result receiver closed"))
    }
}

async fn wait_for_legacy_running(drasi: &DrasiLib, id: &str) {
    let mut events = drasi.subscribe_all_component_events();
    if drasi
        .get_graph()
        .await
        .nodes
        .iter()
        .any(|node| node.id == id && node.status == ComponentStatus::Running)
    {
        return;
    }
    loop {
        let event = events.recv().await.expect("legacy status channel closed");
        if event.component_id == id && event.status == ComponentStatus::Running {
            return;
        }
    }
}

async fn assert_legacy_running(drasi: &DrasiLib) {
    assert_eq!(
        drasi
            .get_source_status(LEGACY_SOURCE)
            .await
            .expect("read legacy source status"),
        ComponentStatus::Running
    );
    assert_eq!(
        drasi
            .get_query_status(LEGACY_QUERY)
            .await
            .expect("read legacy query status"),
        ComponentStatus::Running
    );
    assert_eq!(
        drasi
            .get_reaction_status(LEGACY_REACTION)
            .await
            .expect("read legacy reaction status"),
        ComponentStatus::Running
    );
}

async fn inject_and_assert_result(
    injector: &SourceBase,
    results: &mut mpsc::Receiver<QueryResult>,
    name: &str,
    effective_from: u64,
) -> u64 {
    injector
        .dispatch_source_change(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(LEGACY_SOURCE, name),
                    labels: Arc::from(vec![Arc::from("Item")]),
                    effective_from,
                },
                properties: ElementPropertyMap::from(serde_json::json!({ "name": name })),
            },
        })
        .await
        .expect("inject legacy source event");

    let result = results.recv().await.expect("legacy result channel closed");
    assert_eq!(result.query_id, LEGACY_QUERY);
    match result.results.as_slice() {
        [ResultDiff::Add { data, .. }] => {
            assert_eq!(data, &serde_json::json!({ "name": name }));
        }
        other => panic!("expected one Add for {name}, got {other:?}"),
    }
    result.sequence
}

async fn assert_coexistence(instance_id: &str) {
    let injector = Arc::new(
        SourceBase::new(SourceBaseParams::new(LEGACY_SOURCE))
            .expect("construct injectable legacy source"),
    );
    let (results_tx, mut results_rx) = mpsc::channel(4);
    let legacy_loop_stops = Arc::new(AtomicUsize::new(0));
    let source = InjectableSource {
        base: injector.clone(),
    };
    let drasi = DrasiLib::builder()
        .with_id(instance_id)
        .with_source(source.clone())
        .with_query(
            Query::cypher(LEGACY_QUERY)
                .query("MATCH (i:Item) RETURN i.name AS name")
                .from_source(LEGACY_SOURCE)
                .enable_bootstrap(false)
                .auto_start(true)
                .build(),
        )
        .with_reaction(CapturingReaction {
            base: ReactionBase::new(ReactionBaseParams::new(
                LEGACY_REACTION,
                vec![LEGACY_QUERY.to_string()],
            )),
            results: results_tx,
            loop_stops: legacy_loop_stops.clone(),
        })
        .build()
        .await
        .expect("build legacy pipeline");

    drasi.start().await.expect("start legacy pipeline");
    for id in [LEGACY_SOURCE, LEGACY_QUERY, LEGACY_REACTION] {
        wait_for_legacy_running(&drasi, id).await;
    }
    assert_legacy_running(&drasi).await;
    let before = inject_and_assert_result(&injector, &mut results_rx, "before", 1000).await;

    let schema = SchemaDescriptor::try_new(
        SchemaId::try_new("coexistence-empty").expect("valid empty schema ID"),
        SchemaVersion::try_new(1).expect("valid schema version"),
        "application/x-empty",
        Bytes::from_static(b"No record operations; empty change sets only"),
    )
    .expect("construct empty-event schema descriptor");
    // Empty change sets are valid B1 events; no permissive record validator is needed.
    let changes = ChangeSet::try_new(
        ChangeSetId::try_new("coexistence", Bytes::from_static(b"changes-1"))
            .expect("valid change set ID"),
        schema.clone(),
        vec![],
    )
    .expect("construct valid empty change set");
    let envelope_id = EnvelopeId::try_new("coexistence", Bytes::from_static(b"event-1"))
        .expect("valid native envelope ID");
    let stream = StreamId::try_new("native-stream").expect("valid native stream ID");
    let output = Endpoint::new(
        ComponentId::try_new("native-source").expect("valid native source ID"),
        PortId::try_new("out").expect("valid native output port ID"),
    );
    let input = Endpoint::new(
        ComponentId::try_new("native-sink").expect("valid native sink ID"),
        PortId::try_new("in").expect("valid native input port ID"),
    );
    let descriptor = |endpoint: &Endpoint, direction| {
        ComponentDescriptor::try_new(
            endpoint.component.clone(),
            vec![PortDescriptor::new(
                endpoint.port.clone(),
                direction,
                schema.clone(),
                PipeRequirements::default(),
            )],
        )
        .expect("construct single-port native component descriptor")
    };
    let stops = Arc::new(AtomicUsize::new(0));
    let (handled_tx, mut handled_rx) = mpsc::channel(1);
    let mut graph = ComputationGraph::builder("native-coexistence")
        .source(Box::new(NativeSource {
            descriptor: descriptor(&output, PortDirection::Output),
            event: Some(OutputEnvelope {
                port: output.port.clone(),
                envelope: Envelope::new(
                    envelope_id.clone(),
                    changes,
                    SystemMetadata::new(stream.clone(), 1),
                ),
            }),
            stops: stops.clone(),
        }))
        .sink(Box::new(NativeSink {
            descriptor: descriptor(&input, PortDirection::Input),
            handled: handled_tx,
            stops: stops.clone(),
        }))
        .bind_stream(output.clone(), stream.clone())
        .connect(
            EdgeDefinition::new(output, input.clone()),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("build native graph");

    let run = graph.start().expect("start native graph");
    let control = run.control();
    let mut states = control.subscribe();
    // Both futures are caller-polled, including on the single-threaded runtime.
    let (outcome, during) = tokio::join!(run, async {
        states
            .wait_for(|state| *state == GraphState::Running)
            .await
            .expect("native graph state channel closed");
        let handled = handled_rx.recv().await.expect("native sink did not handle");
        assert_eq!(handled.port, input.port);
        assert_eq!(handled.envelope.id(), &envelope_id);
        assert_eq!(handled.envelope.system().stream(), &stream);
        assert_eq!(handled.envelope.system().sequence(), 1);
        assert_eq!(control.state(), GraphState::Running);
        assert_legacy_running(&drasi).await;

        let during = inject_and_assert_result(&injector, &mut results_rx, "during", 2000).await;
        assert!(during > before);
        assert_legacy_running(&drasi).await;
        assert_eq!(control.state(), GraphState::Running);
        control.cancel();
        during
    });

    assert!(matches!(outcome, Err(GraphError::Cancelled)), "{outcome:?}");
    assert_eq!(graph.state(), GraphState::Cancelled);
    assert_eq!(stops.load(Ordering::SeqCst), 2, "both native hooks stopped");
    assert_legacy_running(&drasi).await;

    // Only inject after cancellation and cleanup have completed, not from a queued event.
    let after = inject_and_assert_result(&injector, &mut results_rx, "after", 3000).await;
    assert!(after > during);
    assert_legacy_running(&drasi).await;
    assert_eq!(legacy_loop_stops.load(Ordering::SeqCst), 0);

    {
        use drasi_lib::computation::v1::{
            ComponentFactory, ComponentRole, ComponentSpecification, ConfigurationValue,
            GraphChangeCodec, LegacySourceFactory, LegacySourceResource, ResourceHandle,
            ResourceId, ResourceOwnership, ResourceRole, ResourceSpecification,
        };
        use std::collections::BTreeMap;

        let factory = Arc::new(LegacySourceFactory::default());
        let wrapped_id = ComponentId::try_new("wrapped-source").expect("id");
        let wrapped_stream = StreamId::try_new("wrapped-source/out").expect("stream");
        let source_resource = ResourceId::try_new("borrowed-source").expect("resource");
        let graph_schema = GraphChangeCodec::schema();
        let output = Endpoint::new(wrapped_id.clone(), PortId::try_new("out").expect("port"));
        let input = Endpoint::new(
            ComponentId::try_new("wrapped-sink").expect("id"),
            PortId::try_new("in").expect("port"),
        );
        let source_descriptor = ComponentDescriptor::try_new(
            wrapped_id,
            vec![PortDescriptor::new(
                output.port.clone(),
                PortDirection::Output,
                graph_schema.descriptor().clone(),
                PipeRequirements::default(),
            )],
        )
        .expect("source descriptor");
        let (wrapped_tx, mut wrapped_rx) = mpsc::channel(1);
        let wrapped_stops = Arc::new(AtomicUsize::new(0));
        let mut wrapped_graph = ComputationGraph::builder("wrapped-coexistence")
            .component(
                ComponentSpecification {
                    descriptor: source_descriptor,
                    role: ComponentRole::Source,
                    completion: None,
                    implementation: factory.descriptor().implementation.clone(),
                    configuration_version: 1,
                    configuration: BTreeMap::from([(
                        Arc::from("stream"),
                        ConfigurationValue::Literal(serde_json::json!(wrapped_stream.as_str())),
                    )]),
                    dependencies: BTreeMap::from([(
                        Arc::from("source"),
                        vec![source_resource.clone()],
                    )]),
                },
                factory,
            )
            .declare_resource(ResourceSpecification {
                id: source_resource.clone(),
                role: ResourceRole::LegacySource,
                ownership: ResourceOwnership::Borrowed,
                binding: Arc::from(LEGACY_SOURCE),
            })
            .expect("resource declaration")
            .provide_resource(
                source_resource,
                ResourceHandle::new(
                    ResourceRole::LegacySource,
                    Arc::new(LegacySourceResource::borrowed(Arc::new(source))),
                ),
            )
            .expect("borrow actual running source")
            .sink(Box::new(NativeSink {
                descriptor: ComponentDescriptor::try_new(
                    input.component.clone(),
                    vec![PortDescriptor::new(
                        input.port.clone(),
                        PortDirection::Input,
                        graph_schema.descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )
                .expect("sink descriptor"),
                handled: wrapped_tx,
                stops: wrapped_stops.clone(),
            }))
            .bind_stream(output.clone(), wrapped_stream.clone())
            .connect(
                EdgeDefinition::new(output, input),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()
            .expect("wrapped graph");
        let run = wrapped_graph.start().expect("wrapped scope");
        let control = run.control();
        let (outcome, ()) = tokio::join!(run, async {
            control.startup_report().await.expect("wrapped startup");
            assert_legacy_running(&drasi).await;
            inject_and_assert_result(&injector, &mut results_rx, "wrapped", 4000).await;
            let wrapped = wrapped_rx.recv().await.expect("wrapped source change");
            assert_eq!(wrapped.envelope.system().stream(), &wrapped_stream);
            assert_eq!(
                wrapped.envelope.system().sequence(),
                1,
                "adapter has its own producer sequence"
            );
            let metadata = GraphChangeCodec::source_metadata(&wrapped.envelope)
                .expect("raw metadata")
                .expect("source metadata retained");
            assert_eq!(metadata.source_id, LEGACY_SOURCE);
            assert!(metadata.sequence.expect("raw source sequence") > 1);
            assert_eq!(
                GraphChangeCodec::decode_changes(&wrapped.envelope)
                    .expect("typed graph change")
                    .len(),
                1
            );
            control.cancel();
        });
        assert!(matches!(outcome, Err(GraphError::Cancelled)));
        wrapped_graph
            .dispose()
            .await
            .expect("borrowed source not deprovisioned");
        assert_eq!(wrapped_stops.load(Ordering::SeqCst), 1);
        assert_legacy_running(&drasi).await;
        inject_and_assert_result(&injector, &mut results_rx, "after-wrapper", 5000).await;
        assert_legacy_running(&drasi).await;
        assert_eq!(legacy_loop_stops.load(Ordering::SeqCst), 0);
    }
    drasi.stop().await.expect("stop legacy pipeline");
    assert_eq!(legacy_loop_stops.load(Ordering::SeqCst), 1);
}

async fn run_coexistence(instance_id: &str) {
    // A deadlock guard only: ordering is established by state and result notifications.
    tokio::time::timeout(Duration::from_secs(30), assert_coexistence(instance_id))
        .await
        .expect("legacy/native coexistence deadlocked");
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_pipeline_survives_graph_cancellation_on_current_thread() {
    run_coexistence("coexistence-current-thread").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_pipeline_survives_graph_cancellation_on_multi_thread() {
    run_coexistence("coexistence-multi-thread").await;
}
