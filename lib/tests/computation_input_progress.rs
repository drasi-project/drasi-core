// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[allow(dead_code)]
mod computation_support;

use anyhow::Result;
use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};

const GRAPH: &str = "handled-inputs";

struct Tracked<T> {
    inner: T,
    progress: Option<Arc<QuerySourceProgress>>,
    consumers: Option<Arc<ConsumerProgressSet>>,
}

#[async_trait]
impl<T: ComputationComponent> ComputationComponent for Tracked<T> {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }

    fn recovery_contract(&self) -> ComponentRecovery {
        let mut contract = self.inner.recovery_contract();
        if let Some(progress) = &self.progress {
            contract = contract.with_volatile_input_progress(progress.clone());
        }
        if let Some(consumers) = &self.consumers {
            contract = contract.with_consumer_progress(consumers.clone());
        }
        contract
    }

    async fn start(&mut self) -> Result<()> {
        if let Some(consumers) = &self.consumers {
            consumers.read_only().wait_ready().await?;
        }
        self.inner.start().await
    }

    async fn stop(&mut self) -> Result<()> {
        self.inner.stop().await
    }
}

#[async_trait]
impl<T: EnvelopeSource> EnvelopeSource for Tracked<T> {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.inner.next().await
    }
}

#[async_trait]
impl<T: Transformer> Transformer for Tracked<T> {
    async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        self.inner.transform(input).await
    }

    fn has_pending_emissions(&self) -> bool {
        self.inner.has_pending_emissions()
    }

    async fn continue_transform(&mut self) -> Result<Vec<OutputEnvelope>> {
        self.inner.continue_transform().await
    }

    async fn delivery_completed(&mut self, output: &[OutputEnvelope]) -> Result<()> {
        self.inner.delivery_completed(output).await
    }
}

#[async_trait]
impl<T: EnvelopeSink> EnvelopeSink for Tracked<T> {
    fn completion(&self) -> SinkCompletion {
        self.inner.completion()
    }

    fn batching(&self) -> Result<Option<SinkBatching>> {
        self.inner.batching()
    }

    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.inner.handle(input).await
    }

    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        self.inner.handle_batch(inputs).await
    }
}

fn progress(id: &str) -> Arc<QuerySourceProgress> {
    Arc::new(QuerySourceProgress::new(GRAPH, component(id)).expect("progress"))
}

fn consumers(producer: &str, owners: Vec<Arc<QuerySourceProgress>>) -> Arc<ConsumerProgressSet> {
    Arc::new(ConsumerProgressSet::new(GRAPH, component(producer), owners).expect("consumer set"))
}

fn replayed_inputs() -> Result<Vec<OutputEnvelope>> {
    let identity = GraphProducerIdentity::volatile(
        "scope".into(),
        GRAPH.into(),
        component("source"),
        stream("source"),
    )?;
    let mut first = root("source", 1, &[10]);
    GraphProducerProgress::annotate(&mut first, &identity, 1)?;
    let duplicate = first.reemit(2)?;
    let mut second = root("source", 3, &[20]);
    GraphProducerProgress::annotate(&mut second, &identity, 2)?;
    Ok(vec![output(first), output(duplicate), output(second)])
}

type Batches = Arc<Mutex<Vec<Vec<InputEnvelope>>>>;

struct BatchSink {
    descriptor: ComponentDescriptor,
    batches: Batches,
}

#[async_trait]
impl ComputationComponent for BatchSink {
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
impl EnvelopeSink for BatchSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn batching(&self) -> Result<Option<SinkBatching>> {
        Ok(Some(SinkBatching {
            max_inputs: 3,
            max_operations: 3,
            max_bytes: 64 * 1024,
            max_delay_ms: 100,
        }))
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.batches.lock().expect("batches").push(vec![input]);
        Ok(())
    }
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        self.batches.lock().expect("batches").push(inputs);
        Ok(())
    }
}

async fn fanout(batched: bool) -> Result<()> {
    let transform_progress = progress("transform");
    let direct_progress = progress("direct");
    let left_progress = progress("left");
    let right_progress = progress("right");
    let source_consumers = consumers(
        "source",
        vec![transform_progress.clone(), direct_progress.clone()],
    );
    let transform_consumers = consumers(
        "transform",
        vec![left_progress.clone(), right_progress.clone()],
    );
    let left = Received::default();
    let right = Received::default();
    let direct = Batches::default();
    let invocations = Arc::new(AtomicUsize::new(0));
    let calls = invocations.clone();
    let identity = GraphProducerIdentity::volatile(
        "scope".into(),
        GRAPH.into(),
        component("transform"),
        stream("transform"),
    )?;
    let transformer = NativeTransform::new("transform", move |input| {
        let sequence = calls.fetch_add(1, Ordering::SeqCst) as u64 + 1;
        let values: Vec<_> = values(&input.envelope)
            .iter()
            .map(|value| value * 2)
            .collect();
        let mut envelope = derived(
            &input.envelope,
            "transform",
            sequence,
            changes("transform", sequence, &values),
        );
        GraphProducerProgress::annotate(&mut envelope, &identity, sequence)?;
        Ok(vec![output(envelope)])
    });
    let mut builder = ComputationGraph::builder(GRAPH)
        .source(Box::new(Tracked {
            inner: FiniteSource::new("source", replayed_inputs()?),
            progress: None,
            consumers: Some(source_consumers),
        }))
        .transformer(Box::new(Tracked {
            inner: transformer,
            progress: Some(transform_progress.clone()),
            consumers: Some(transform_consumers),
        }))
        .sink(Box::new(Tracked {
            inner: CollectSink::new("left", &["in"], left.clone()),
            progress: Some(left_progress.clone()),
            consumers: None,
        }))
        .sink(Box::new(Tracked {
            inner: CollectSink::new("right", &["in"], right.clone()),
            progress: Some(right_progress.clone()),
            consumers: None,
        }));
    let direct_received = Received::default();
    if batched {
        builder = builder.sink(Box::new(Tracked {
            inner: BatchSink {
                descriptor: descriptor("direct", &["in"], &[]),
                batches: direct.clone(),
            },
            progress: Some(direct_progress.clone()),
            consumers: None,
        }));
    } else {
        builder = builder.sink(Box::new(Tracked {
            inner: CollectSink::new("direct", &["in"], direct_received.clone()),
            progress: Some(direct_progress.clone()),
            consumers: None,
        }));
    }
    let mut graph = builder
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("transform", "out"), stream("transform"))
        .connect(
            edge("source", "transform"),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .connect(
            edge("source", "direct"),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .connect(
            edge("transform", "left"),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .connect(
            edge("transform", "right"),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .build()?;
    graph.start()?.await?;
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        2,
        "duplicates must not rerun transformation"
    );
    for received in [left, right] {
        let received = received.lock().expect("received");
        assert_eq!(
            received
                .iter()
                .flat_map(|input| values(&input.envelope))
                .collect::<Vec<_>>(),
            [20, 40]
        );
    }
    if batched {
        let batches = direct.lock().expect("batches");
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].len(),
            2,
            "duplicates within a batch are not delivered twice"
        );
        assert_eq!(
            batches[0]
                .iter()
                .flat_map(|input| values(&input.envelope))
                .collect::<Vec<_>>(),
            [10, 20]
        );
    } else {
        assert_eq!(direct_received.lock().expect("direct").len(), 2);
    }
    for (owner, producer, transport) in [
        (transform_progress, "source", 3),
        (direct_progress, "source", 3),
        (left_progress, "transform", 2),
        (right_progress, "transform", 2),
    ] {
        let snapshot = owner.snapshot();
        assert_eq!(
            snapshot.checkpoints[&SourceProgressKey::Stream(stream(producer))].sequence,
            2
        );
        assert_eq!(snapshot.transport_sequences[&stream(producer)], transport);
        assert!(
            !snapshot.persistent,
            "handled input must not become durable"
        );
        assert!(
            !snapshot.ready && !snapshot.admitting,
            "completed graph awaits owned stop"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn source_transformer_and_sink_groups_use_actual_volatile_boundaries_current_thread(
) -> Result<()> {
    for batched in [false, true] {
        tokio::time::timeout(Duration::from_secs(10), fanout(batched)).await??;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_transformer_and_sink_groups_use_actual_volatile_boundaries_multi_thread(
) -> Result<()> {
    for batched in [false, true] {
        tokio::time::timeout(Duration::from_secs(10), fanout(batched)).await??;
    }
    Ok(())
}

struct Continued {
    descriptor: ComponentDescriptor,
    input: Option<Envelope>,
    sequence: u64,
    completed: mpsc::Sender<u64>,
    release: Arc<Notify>,
}

#[async_trait]
impl ComputationComponent for Continued {
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
impl Transformer for Continued {
    async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        self.input = Some(input.envelope.clone());
        self.sequence += 1;
        Ok(vec![output(derived(
            &input.envelope,
            "transform",
            self.sequence,
            changes("transform", self.sequence, &[1]),
        ))])
    }
    fn has_pending_emissions(&self) -> bool {
        self.input.is_some()
    }
    async fn continue_transform(&mut self) -> Result<Vec<OutputEnvelope>> {
        let input = self.input.take().expect("continuation");
        self.sequence += 1;
        Ok(vec![output(derived(
            &input,
            "transform",
            self.sequence,
            changes("transform", self.sequence, &[2]),
        ))])
    }
    async fn delivery_completed(&mut self, _: &[OutputEnvelope]) -> Result<()> {
        self.completed.send(self.sequence).await?;
        self.release.notified().await;
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn handled_progress_waits_for_every_output_and_continuation_callback() -> Result<()> {
    let owner = progress("transform");
    let (completed, mut completion) = mpsc::channel(2);
    let release = Arc::new(Notify::new());
    let received = Received::default();
    let mut graph = ComputationGraph::builder(GRAPH)
        .source(Box::new(FiniteSource::new(
            "source",
            vec![output(root("source", 1, &[1]))],
        )))
        .transformer(Box::new(Tracked {
            inner: Continued {
                descriptor: descriptor("transform", &["in"], &["out"]),
                input: None,
                sequence: 0,
                completed,
                release: release.clone(),
            },
            progress: Some(owner.clone()),
            consumers: None,
        }))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            received.clone(),
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("transform", "out"), stream("transform"))
        .connect(
            edge("source", "transform"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .connect(
            edge("transform", "sink"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .build()?;
    let run = graph.start()?;
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run, async {
            for expected in 1..=2 {
                assert_eq!(completion.recv().await, Some(expected));
                assert!(
                    owner.snapshot().checkpoints.is_empty(),
                    "uncompleted output is not handled input"
                );
                release.notify_one();
            }
        })
    })
    .await?;
    result?;
    assert_eq!(
        owner.snapshot().checkpoints[&SourceProgressKey::Stream(stream("source"))].sequence,
        1
    );
    assert_eq!(received.lock().expect("sink").len(), 2);
    Ok(())
}

#[test]
fn invalid_volatile_input_roles_scopes_and_double_publication_are_rejected() {
    for (owner, accepted, component_owned) in [
        (progress("wrong"), false, false),
        (progress("sink"), true, false),
        (progress("sink"), false, true),
    ] {
        struct InvalidSink {
            descriptor: ComponentDescriptor,
            progress: Arc<QuerySourceProgress>,
            accepted: bool,
            component_owned: bool,
        }
        #[async_trait]
        impl ComputationComponent for InvalidSink {
            fn descriptor(&self) -> &ComponentDescriptor {
                &self.descriptor
            }
            fn recovery_contract(&self) -> ComponentRecovery {
                let contract = ComponentRecovery::default()
                    .with_volatile_input_progress(self.progress.clone());
                if self.component_owned {
                    contract.with_committed_progress(self.progress.clone())
                } else {
                    contract
                }
            }
            async fn start(&mut self) -> Result<()> {
                panic!("invalid owner cannot start")
            }
            async fn stop(&mut self) -> Result<()> {
                Ok(())
            }
        }
        #[async_trait]
        impl EnvelopeSink for InvalidSink {
            fn completion(&self) -> SinkCompletion {
                if self.accepted {
                    SinkCompletion::Accepted
                } else {
                    SinkCompletion::Handled
                }
            }
            async fn handle(&mut self, _: InputEnvelope) -> Result<()> {
                panic!("invalid input")
            }
        }
        assert!(ComputationGraph::builder(GRAPH)
            .sink(Box::new(InvalidSink {
                descriptor: descriptor("sink", &["in"], &[]),
                progress: owner,
                accepted,
                component_owned,
            }))
            .build()
            .is_err());
    }

    assert!(ComputationGraph::builder(GRAPH)
        .source(Box::new(Tracked {
            inner: FiniteSource::new("source", Vec::new()),
            progress: Some(progress("source")),
            consumers: None,
        }))
        .build()
        .is_err());
    let foreign =
        Arc::new(QuerySourceProgress::new("foreign", component("sink")).expect("foreign"));
    assert!(ComputationGraph::builder(GRAPH)
        .sink(Box::new(Tracked {
            inner: CollectSink::new("sink", &["in"], Received::default()),
            progress: Some(foreign),
            consumers: None,
        }))
        .build()
        .is_err());
}

struct InterruptedSink {
    descriptor: ComponentDescriptor,
    entered: mpsc::Sender<&'static str>,
    processing: Arc<Notify>,
    stopping: Arc<Notify>,
}

#[async_trait]
impl ComputationComponent for InterruptedSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.entered.send("stop").await?;
        self.stopping.notified().await;
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for InterruptedSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> Result<()> {
        self.entered.send("handle").await?;
        self.processing.notified().await;
        anyhow::bail!("effect failed")
    }
}

async fn interrupted(fail: bool) -> Result<()> {
    let owner = progress("sink");
    let (entered, mut events) = mpsc::channel(2);
    let processing = Arc::new(Notify::new());
    let stopping = Arc::new(Notify::new());
    let mut graph = ComputationGraph::builder(GRAPH)
        .source(Box::new(FiniteSource::new(
            "source",
            vec![output(root("source", 1, &[1]))],
        )))
        .sink(Box::new(Tracked {
            inner: InterruptedSink {
                descriptor: descriptor("sink", &["in"], &[]),
                entered,
                processing: processing.clone(),
                stopping: stopping.clone(),
            },
            progress: Some(owner.clone()),
            consumers: None,
        }))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .build()?;
    let run = graph.start()?;
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        assert_eq!(events.recv().await, Some("handle"));
        assert!(owner.snapshot().ready);
        assert!(owner.snapshot().checkpoints.is_empty());
        if fail {
            let mut publication = owner.subscribe();
            processing.notify_one();
            publication
                .wait_for(|state| state.failure.is_some())
                .await
                .expect("failure");
        }
        control.cancel();
        assert_eq!(events.recv().await, Some("stop"));
        let snapshot = owner.snapshot();
        assert!(!snapshot.ready && !snapshot.admitting);
        assert!(
            snapshot.failure.is_some(),
            "failed/cancelled handling must fence its owner"
        );
        assert!(
            snapshot.checkpoints.is_empty(),
            "uncompleted effects cannot acknowledge input"
        );
        stopping.notify_one();
    });
    assert!(result.is_err());
    assert!(!owner.snapshot().ready);
    assert!(owner.snapshot().checkpoints.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn failed_and_cancelled_handlers_fence_progress_until_owned_cleanup_current_thread(
) -> Result<()> {
    for fail in [false, true] {
        tokio::time::timeout(Duration::from_secs(10), interrupted(fail)).await??;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_and_cancelled_handlers_fence_progress_until_owned_cleanup_multi_thread(
) -> Result<()> {
    for fail in [false, true] {
        tokio::time::timeout(Duration::from_secs(10), interrupted(fail)).await??;
    }
    Ok(())
}

mod query_row_provenance {
    use super::*;
    use drasi_core::{
        computation::InMemoryComputationProvider,
        models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    };

    fn descriptor(name: &str, input: bool, output: bool) -> ComponentDescriptor {
        ComponentDescriptor::try_new(
            component(name),
            [(input, "in", PortDirection::Input), (output, "out", PortDirection::Output)]
                .into_iter()
                .filter(|(include, _, _)| *include)
                .map(|(_, name, direction)| {
                    PortDescriptor::new(
                        port(name),
                        direction,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
        )
        .expect("query row descriptor")
    }
    struct Source {
        descriptor: ComponentDescriptor,
        outputs: std::collections::VecDeque<OutputEnvelope>,
    }
    struct Expand {
        descriptor: ComponentDescriptor,
        sequence: u64,
    }
    struct Sink {
        descriptor: ComponentDescriptor,
        received: Received,
        batching: bool,
    }
    macro_rules! lifecycle {
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
    lifecycle!(Source);
    lifecycle!(Expand);
    lifecycle!(Sink);
    #[async_trait]
    impl EnvelopeSource for Source {
        async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
            Ok(self.outputs.pop_front())
        }
    }
    #[async_trait]
    impl Transformer for Expand {
        async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
            let mut outputs = Vec::new();
            for _ in 0..2 {
                self.sequence += 1;
                outputs.push(output(input.envelope.derive(
                    emission_id(&stream("expand"), self.sequence)?,
                    input.envelope.changes().clone(),
                    SystemMetadata::new(stream("expand"), self.sequence),
                )));
            }
            Ok(outputs)
        }
    }
    #[async_trait]
    impl EnvelopeSink for Sink {
        fn completion(&self) -> SinkCompletion {
            SinkCompletion::Handled
        }
        fn batching(&self) -> Result<Option<SinkBatching>> {
            Ok(self.batching.then_some(SinkBatching {
                max_inputs: 2,
                max_operations: 2,
                max_bytes: 64 * 1024,
                max_delay_ms: 20,
            }))
        }
        async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
            self.received.lock().expect("rows").push(input);
            Ok(())
        }
        async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
            self.received.lock().expect("rows").extend(inputs);
            Ok(())
        }
    }

    async fn query_outputs() -> Result<Vec<OutputEnvelope>> {
        let mut query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: GRAPH.into(),
                id: component("query"),
                query: "MATCH (n:Item) RETURN n.value AS value".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: stream("query"),
                outbox_capacity: std::num::NonZeroUsize::new(8).expect("capacity"),
            },
            Arc::new(InMemoryComputationProvider),
        )
        .await?;
        query.start().await?;
        let outputs = query
            .transform(InputEnvelope {
                port: port("in"),
                envelope: GraphChangeCodec::encode_change(
                    SourceChange::Insert {
                        element: Element::Node {
                            metadata: ElementMetadata {
                                reference: ElementReference::new("data", "item"),
                                labels: Arc::from([Arc::from("Item")]),
                                effective_from: 1000,
                            },
                            properties: ElementPropertyMap::from(serde_json::json!({"value":1})),
                        },
                    },
                    stream("data"),
                    1,
                    None,
                )?,
            })
            .await?;
        query.stop().await?;
        anyhow::ensure!(outputs.len() == 1, "one actual query output");
        Ok(outputs)
    }

    struct Relay {
        descriptor: ComponentDescriptor,
        sequence: u64,
    }
    #[async_trait]
    impl ComputationComponent for Relay {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        fn recovery_contract(&self) -> ComponentRecovery {
            ComponentRecovery::stateless()
        }
        async fn start(&mut self) -> Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }
    #[async_trait]
    impl Transformer for Relay {
        async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
            self.sequence += 1;
            Ok(vec![output(input.envelope.derive(
                emission_id(&stream("relay"), self.sequence)?,
                input.envelope.changes().clone(),
                SystemMetadata::new(stream("relay"), self.sequence),
            ))])
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn validated_stateless_paths_keep_original_query_positions_not_replay_transport(
    ) -> Result<()> {
        let outputs = query_outputs().await?;
        for batching in [false, true] {
            let received = Received::default();
            let owner = progress("sink");
            let mut graph = ComputationGraph::builder(GRAPH)
                .source(Box::new(Tracked {
                    inner: Source {
                        descriptor: descriptor("source", false, true),
                        outputs: vec![outputs[0].clone(), output(outputs[0].envelope.reemit(2)?)]
                            .into(),
                    },
                    progress: None,
                    consumers: Some(consumers("source", vec![owner.clone()])),
                }))
                .transformer(Box::new(Relay {
                    descriptor: descriptor("relay", true, true),
                    sequence: 0,
                }))
                .sink(Box::new(Tracked {
                    inner: Sink {
                        descriptor: descriptor("sink", true, false),
                        received: received.clone(),
                        batching,
                    },
                    progress: Some(owner.clone()),
                    consumers: None,
                }))
                .bind_stream(endpoint("source", "out"), stream("query"))
                .bind_stream(endpoint("relay", "out"), stream("relay"))
                .connect(
                    edge("source", "relay"),
                    Box::new(BoundedPipeConfig { capacity: 2 }),
                )
                .connect(
                    edge("relay", "sink"),
                    Box::new(BoundedPipeConfig { capacity: 2 }),
                )
                .build()?;
            graph.start()?.await?;
            assert_eq!(received.lock().expect("rows").len(), 1);
            let state = owner.snapshot();
            assert_eq!(
                state.checkpoints[&SourceProgressKey::Stream(stream("query"))].sequence,
                1
            );
            assert_eq!(state.transport_sequences[&stream("query")], 2);
            assert_eq!(state.transport_sequences[&stream("relay")], 2);
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inherited_query_positions_do_not_drop_a_transformers_distinct_outputs() -> Result<()> {
        let outputs = query_outputs().await?;
        for (expand, grouped, batching) in [
            (false, false, false),
            (false, false, true),
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (true, true, true),
        ] {
            let received = Received::default();
            let owner = progress("sink");
            let mut builder = ComputationGraph::builder(GRAPH)
                .source(Box::new(Source {
                    descriptor: descriptor("source", false, true),
                    outputs: if expand {
                        outputs.clone().into()
                    } else {
                        vec![outputs[0].clone(), output(outputs[0].envelope.reemit(2)?)].into()
                    },
                }))
                .sink(Box::new(Tracked {
                    inner: Sink {
                        descriptor: descriptor("sink", true, false),
                        received: received.clone(),
                        batching,
                    },
                    progress: Some(owner.clone()),
                    consumers: None,
                }))
                .bind_stream(endpoint("source", "out"), stream("query"));
            if expand {
                builder = builder
                    .transformer(Box::new(Tracked {
                        inner: Expand {
                            descriptor: descriptor("expand", true, true),
                            sequence: 0,
                        },
                        progress: None,
                        consumers: grouped.then(|| consumers("expand", vec![owner.clone()])),
                    }))
                    .bind_stream(endpoint("expand", "out"), stream("expand"))
                    .connect(
                        edge("source", "expand"),
                        Box::new(BoundedPipeConfig { capacity: 2 }),
                    );
            }
            let producer = if expand { "expand" } else { "source" };
            let mut graph = builder
                .connect(
                    edge(producer, "sink"),
                    Box::new(BoundedPipeConfig { capacity: 2 }),
                )
                .build()?;
            graph.start()?.await?;
            assert_eq!(
                received.lock().expect("rows").len(),
                if expand { 2 } else { 1 }
            );
            assert_eq!(
                owner.snapshot().checkpoints
                    [&SourceProgressKey::Stream(stream(if expand { "expand" } else { "query" }))]
                    .sequence,
                if expand { 2 } else { 1 },
            );
            assert_eq!(
                owner.snapshot().transport_sequences
                    [&stream(if expand { "expand" } else { "query" })],
                2
            );
        }
        Ok(())
    }
}
