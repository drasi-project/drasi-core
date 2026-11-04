// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]

use drasi_lib::{
    channels::{
        BroadcastChangeDispatcher, ChangeDispatcher, ChangeMessage, ChannelChangeDispatcher,
        PriorityQueue, ReplayThenLiveReceiver, SourceEvent, SourceEventWrapper,
    },
    observability::{ExecutionContext, ExecutionScope, TraceContextV1},
};
use std::{collections::VecDeque, sync::Arc};

fn context(id: u8, sampled: bool) -> ExecutionContext {
    ExecutionContext::new(TraceContextV1::new([id; 16], [id; 8], u8::from(sampled), None).unwrap())
}

fn event(sequence: u64) -> Arc<SourceEventWrapper> {
    Arc::new(SourceEventWrapper::new(
        "source".into(),
        SourceEvent::Change(drasi_core::models::SourceChange::Delete {
            metadata: drasi_core::models::ElementMetadata {
                reference: drasi_core::models::ElementReference::new(
                    "source",
                    &sequence.to_string(),
                ),
                labels: Arc::from([]),
                effective_from: sequence,
            },
        }),
        chrono::Utc::now(),
        sequence,
    ))
}

#[tokio::test]
async fn ordinary_dispatch_and_replay_keep_only_live_context() -> anyhow::Result<()> {
    use drasi_lib::channels::ChangeReceiver;
    for dispatcher in [
        Box::new(ChannelChangeDispatcher::new(1)) as Box<dyn ChangeDispatcher<SourceEventWrapper>>,
        Box::new(BroadcastChangeDispatcher::new(1)),
    ] {
        let live = dispatcher.create_receiver().await?;
        ExecutionScope::new(context(1, false))
            .scope(dispatcher.dispatch_change(event(2)))
            .await?;
        let mut receiver = ReplayThenLiveReceiver::new(VecDeque::from([event(1)]), live);
        assert!(receiver.recv_message().await?.execution_context.is_empty());
        assert_eq!(
            receiver.recv_message().await?.execution_context,
            context(1, false)
        );
        dispatcher
            .dispatch_message(ChangeMessage::new(event(3), Default::default()))
            .await?;
        assert!(receiver.recv_message().await?.execution_context.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn priority_queue_context_is_not_an_ordering_key_or_persisted_payload() {
    let queue = PriorityQueue::new(2);
    let first = event(1);
    let original = first.clone();
    queue
        .enqueue_message_wait(ChangeMessage::new(event(2), context(2, true)))
        .await;
    queue
        .enqueue_message_wait(ChangeMessage::new(first, context(1, false)))
        .await;
    let first = queue.dequeue_message().await;
    assert_eq!(first.payload.sequence, 1);
    assert_eq!(first.execution_context, context(1, false));
    assert!(Arc::ptr_eq(&first.payload, &original));
    assert_eq!(
        queue.dequeue_message().await.execution_context,
        context(2, true)
    );
    assert_eq!(queue.depth().await, 0);
}

#[tokio::test]
async fn blocked_ordinary_enqueue_keeps_the_waiting_items_context() {
    let queue = PriorityQueue::new(1);
    queue
        .enqueue_message_wait(ChangeMessage::new(event(1), context(1, true)))
        .await;
    {
        let waiting = ExecutionScope::new(context(2, false)).scope(queue.enqueue_wait(event(2)));
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        assert_eq!(
            queue.dequeue_message().await.execution_context,
            context(1, true)
        );
        waiting.await;
    }
    let message = queue.dequeue_message().await;
    assert_eq!(message.execution_context, context(2, false));
    assert_eq!(message.payload.sequence, 2);
    queue.enqueue_wait(event(3)).await;
    assert!(queue.dequeue_message().await.execution_context.is_empty());
}

#[cfg(feature = "otel-context")]
mod traces {
    use super::*;
    use async_trait::async_trait;
    use drasi_core::{
        computation::InMemoryComputationProvider,
        models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    };
    use drasi_lib::{
        computation::v1::*,
        observability::{
            OpenTelemetryContextAdapter, TelemetryOptions, TelemetryProfile, TelemetrySettings,
            TELEMETRY_TARGET,
        },
    };
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
    use std::{
        num::NonZeroUsize,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::{filter::filter_fn, prelude::*};

    fn telemetry() -> TelemetryOptions {
        TelemetryOptions::new(TelemetrySettings::new(TelemetryProfile::Debug).with_metrics(false))
            .with_instance_id("async-tests")
            .with_context_adapter(Arc::new(OpenTelemetryContextAdapter))
    }

    fn definition() -> ContinuousQueryDefinition {
        ContinuousQueryDefinition {
            graph_id: "async-tests".into(),
            id: ComponentId::try_new("query").unwrap(),
            query: "MATCH (n:Item) RETURN n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out").unwrap(),
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        }
    }

    fn change(sequence: u64) -> ChangeEnvelope {
        GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: sequence,
                    },
                    properties: ElementPropertyMap::from(
                        serde_json::json!({"name": sequence.to_string()}),
                    ),
                },
            },
            StreamId::try_new("source/out").unwrap(),
            sequence,
            None,
        )
        .unwrap()
    }

    struct Bootstrap {
        rows: usize,
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    struct EmptySource(ComponentDescriptor);

    #[async_trait]
    impl ComputationComponent for EmptySource {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl EnvelopeSource for EmptySource {
        async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
            Ok(None)
        }
    }

    struct DrainSink(ComponentDescriptor);

    #[async_trait]
    impl ComputationComponent for DrainSink {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl EnvelopeSink for DrainSink {
        fn completion(&self) -> SinkCompletion {
            SinkCompletion::Handled
        }
        async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl ComputationBootstrapProvider for Bootstrap {
        async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(!ExecutionContext::current().is_empty());
            let mut rows: Vec<_> = (1..=self.rows).map(|row| Ok(change(row as u64))).collect();
            if self.fail {
                rows.push(Err(anyhow::anyhow!("snapshot failed")));
            }
            Ok(ComputationBootstrapSnapshot {
                changes: Box::pin(futures::stream::iter(rows)),
                watermarks: vec![],
            })
        }
    }

    async fn collect(future: impl std::future::Future<Output = ()>) -> Vec<SpanData> {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("async-tests"))
                .with_filter(filter_fn(|metadata| {
                    metadata.is_span() && metadata.target() == TELEMETRY_TARGET
                })),
        );
        future.with_subscriber(subscriber).await;
        provider.force_flush().unwrap();
        let spans = exporter.get_finished_spans().unwrap();
        provider.shutdown().unwrap();
        spans
    }

    fn field<'a>(span: &'a SpanData, name: &str) -> Option<&'a opentelemetry::Value> {
        span.attributes
            .iter()
            .find(|field| field.key.as_str() == name)
            .map(|field| &field.value)
    }

    async fn bootstrap_case(rows: usize, fail: bool) -> Vec<SpanData> {
        collect(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let query = ContinuousQueryTransformer::new(
                definition(),
                Arc::new(InMemoryComputationProvider),
            )
            .await
            .unwrap()
            .with_bootstrap(Arc::new(Bootstrap {
                rows,
                calls: calls.clone(),
                fail,
            }));
            let output_schema = query
                .descriptor()
                .ports()
                .iter()
                .find(|port| port.direction() == PortDirection::Output)
                .unwrap()
                .schema()
                .clone();
            let mut graph = ComputationGraph::builder("async-tests")
                .with_telemetry(telemetry())
                .source(Box::new(EmptySource(
                    ComponentDescriptor::try_new(
                        ComponentId::try_new("source").unwrap(),
                        vec![PortDescriptor::new(
                            PortId::try_new("out").unwrap(),
                            PortDirection::Output,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )
                    .unwrap(),
                )))
                .query(Box::new(query))
                .sink(Box::new(DrainSink(
                    ComponentDescriptor::try_new(
                        ComponentId::try_new("sink").unwrap(),
                        vec![PortDescriptor::new(
                            PortId::try_new("in").unwrap(),
                            PortDirection::Input,
                            output_schema,
                            PipeRequirements::default(),
                        )],
                    )
                    .unwrap(),
                )))
                .bind_stream(
                    Endpoint::new(
                        ComponentId::try_new("source").unwrap(),
                        PortId::try_new("out").unwrap(),
                    ),
                    StreamId::try_new("source/out").unwrap(),
                )
                .bind_stream(
                    Endpoint::new(
                        ComponentId::try_new("query").unwrap(),
                        PortId::try_new("out").unwrap(),
                    ),
                    StreamId::try_new("query/out").unwrap(),
                )
                .connect(
                    EdgeDefinition {
                        from: Endpoint::new(
                            ComponentId::try_new("source").unwrap(),
                            PortId::try_new("out").unwrap(),
                        ),
                        to: Endpoint::new(
                            ComponentId::try_new("query").unwrap(),
                            PortId::try_new("in").unwrap(),
                        ),
                    },
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .connect(
                    EdgeDefinition::new(
                        Endpoint::new(
                            ComponentId::try_new("query").unwrap(),
                            PortId::try_new("out").unwrap(),
                        ),
                        Endpoint::new(
                            ComponentId::try_new("sink").unwrap(),
                            PortId::try_new("in").unwrap(),
                        ),
                    ),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .build()
                .unwrap();
            // No data inputs: exercise real lifecycle/bootstrap, then dispose.
            let run = graph.start().unwrap();
            let control = run.control();
            let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::join!(run, async {
                    if fail {
                        let mut observed = control.subscribe_observed();
                        observed
                            .wait_for(|graph| {
                                graph.components[&ComponentId::try_new("query").unwrap()]
                                    .failure
                                    .is_some()
                            })
                            .await
                            .unwrap();
                        control.cancel();
                    }
                })
            })
            .await
            .expect("bootstrap graph must finish");
            assert_eq!(result.is_err(), fail);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            graph.shutdown().await.unwrap();
        })
        .await
    }

    #[tokio::test]
    async fn bootstrap_is_one_linked_root_without_per_row_details() {
        for rows in [0, 1, 25] {
            let spans = bootstrap_case(rows, false).await;
            let bootstrap: Vec<_> = spans
                .iter()
                .filter(|span| span.name == "query.bootstrap")
                .collect();
            assert_eq!(bootstrap.len(), 1);
            let bootstrap = bootstrap[0];
            assert_eq!(
                bootstrap.parent_span_id,
                opentelemetry::trace::SpanId::INVALID
            );
            assert_eq!(bootstrap.links.links.len(), 1);
            let start = spans
                .iter()
                .find(|span| {
                    span.name == "component.start"
                        && field(span, "component_id")
                            == Some(&opentelemetry::Value::String("query".into()))
                })
                .unwrap();
            assert_eq!(
                bootstrap.links.links[0].span_context.trace_id(),
                start.span_context.trace_id()
            );
            assert_eq!(
                bootstrap.links.links[0].span_context.span_id(),
                start.span_context.span_id()
            );
            assert_ne!(
                bootstrap.span_context.trace_id(),
                start.span_context.trace_id()
            );
            assert_eq!(
                field(bootstrap, "input_count"),
                Some(&opentelemetry::Value::I64(rows as i64))
            );
            assert_eq!(
                spans
                    .iter()
                    .filter(|span| span.name == "bootstrap.snapshot")
                    .count(),
                1
            );
            assert!(!spans
                .iter()
                .any(|span| matches!(span.name.as_ref(), "query.evaluate" | "query.commit")));
        }
    }

    #[tokio::test]
    async fn bootstrap_error_is_not_success() {
        let spans = bootstrap_case(2, true).await;
        let bootstrap = spans
            .iter()
            .find(|span| span.name == "query.bootstrap")
            .unwrap();
        assert_eq!(
            field(bootstrap, "outcome"),
            Some(&opentelemetry::Value::String("error".into()))
        );
        assert_eq!(
            field(bootstrap, "input_count"),
            Some(&opentelemetry::Value::I64(2))
        );
    }

    struct WorkService {
        descriptor: ComponentDescriptor,
        inputs: tokio::sync::mpsc::Receiver<ExecutionContext>,
        done: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl ComputationComponent for WorkService {
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
    impl ComputationService for WorkService {
        async fn run(&mut self) -> anyhow::Result<()> {
            let worker = drasi_lib::observability::WorkerTelemetry::capture();
            let mut processed = 0;
            while let Some(context) = self.inputs.recv().await {
                worker
                    .service_work(&context)?
                    .run(async {
                        tokio::task::yield_now().await;
                        assert_eq!(
                            ExecutionScope::current_component()
                                .unwrap()
                                .identity
                                .component_id()
                                .unwrap()
                                .as_str(),
                            "worker-service",
                        );
                        Ok::<_, anyhow::Error>(())
                    })
                    .await?;
                processed += 1;
                if processed == 2 {
                    self.done.notify_one();
                }
            }
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn graph_service_traces_actual_units_not_its_idle_lifetime() {
        let spans = collect(async {
            let (send, receive) = tokio::sync::mpsc::channel(2);
            let done = Arc::new(tokio::sync::Notify::new());
            let mut graph = ComputationGraph::builder("async-tests")
                .with_telemetry(telemetry())
                .service(Box::new(WorkService {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new("worker-service").unwrap(),
                        vec![],
                    )
                    .unwrap(),
                    inputs: receive,
                    done: done.clone(),
                }))
                .build()
                .unwrap();
            let run = graph.start().unwrap();
            let control = run.control();
            let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::join!(run, async {
                    for id in [21, 22] {
                        send.send(context(id, true)).await.unwrap();
                    }
                    done.notified().await;
                    control.cancel();
                })
            })
            .await
            .unwrap();
            assert!(matches!(result, Err(GraphError::Cancelled)));
            graph.shutdown().await.unwrap();
        })
        .await;
        let work: Vec<_> = spans
            .iter()
            .filter(|span| span.name == "component.process")
            .collect();
        assert_eq!(work.len(), 2);
        for id in [21, 22] {
            assert!(work
                .iter()
                .any(|span| span.span_context.trace_id().to_bytes() == [id; 16]
                    && span.parent_span_id.to_bytes() == [id; 8]));
        }
        assert!(work.iter().all(|span| field(span, "component_id")
            == Some(&opentelemetry::Value::String("worker-service".into()))));
    }
}
