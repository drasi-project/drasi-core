// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]

#[allow(dead_code)]
mod computation_support;

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::{
    computation::v1::*,
    observability::{
        ComponentLogKey, ComponentLogRuntime, ComponentLogType, ExecutionScope, LogPolicy,
        TelemetryOptions, TelemetryProfile, TelemetrySettings, TraceContextV1,
    },
};
use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc, time::Duration};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

fn context(id: u8, sampled: bool) -> ExecutionContext {
    ExecutionContext::new(
        TraceContextV1::new(
            [id; 16],
            [id; 8],
            u8::from(sampled),
            Some("vendor=test".into()),
        )
        .unwrap(),
    )
}

fn codec() -> EnvelopeCodec {
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).unwrap());
    codec
        .register_schema(Arc::new(Schema::new(
            schema_descriptor(),
            Arc::new(ReadingValidator(schema_descriptor())),
        )))
        .unwrap();
    codec
}

#[test]
fn trace_context_changes_neither_event_bytes_nor_receipt_digests() {
    let identity = GraphProducerIdentity::new(
        "host".into(),
        "graph".into(),
        component("source"),
        stream("source"),
        true,
    )
    .unwrap();
    let mut envelope = root("source", 1, &[7]);
    GraphProducerProgress::annotate(&mut envelope, &identity, 1).unwrap();
    let codec = codec();
    let mut binary = BinaryEnvelopeCodec::new(NonZeroUsize::new(1 << 20).unwrap());
    binary
        .register_schema(Arc::new(Schema::new(
            schema_descriptor(),
            Arc::new(ReadingValidator(schema_descriptor())),
        )))
        .unwrap();
    let json_bytes = codec.encode(&envelope).unwrap();
    let binary_bytes = binary.encode(&envelope).unwrap();
    let scope = DeliveryScope::new("host".into(), "graph".into(), component("sink")).unwrap();
    let expected = DeliveryBatchIdentity::for_input(
        &scope,
        &InputEnvelope::new(port("in"), envelope.clone()),
        &codec,
    )
    .unwrap();
    for execution_context in [
        ExecutionContext::default(),
        context(1, true),
        context(2, false),
        ExecutionContext::new(
            TraceContextV1::new([1; 16], [3; 8], 0x80, Some("other=value".into())).unwrap(),
        ),
    ] {
        let input = InputEnvelope::new(port("in"), envelope.clone())
            .with_execution_context(execution_context);
        assert_eq!(codec.encode(&input.envelope).unwrap(), json_bytes);
        assert_eq!(binary.encode(&input.envelope).unwrap(), binary_bytes);
        assert_eq!(
            DeliveryBatchIdentity::for_input(&scope, &input, &codec).unwrap(),
            expected
        );
    }
    let mut changed = root("source", 1, &[8]);
    GraphProducerProgress::annotate(&mut changed, &identity, 1).unwrap();
    assert_ne!(
        DeliveryBatchIdentity::for_input(&scope, &InputEnvelope::new(port("in"), changed), &codec,)
            .unwrap()
            .content_digest(),
        expected.content_digest()
    );
}

#[tokio::test]
async fn volatile_delivery_and_send_failures_retain_the_original_context() {
    let providers: Vec<Box<dyn PipeProvider>> = vec![
        Box::new(BoundedPipeConfig { capacity: 1 }),
        Box::new(ByteBoundedPipeConfig {
            capacity: 1,
            max_bytes: 4096,
        }),
        Box::new(BroadcastPipeConfig {
            capacity: 1,
            lag_policy: BroadcastLagPolicy::Report,
        }),
    ];
    for provider in providers {
        let mut provided = provider.create().unwrap();
        let sender = provided.pipe.sender();
        let mut receiver = provided.pipe.take_receiver().unwrap();
        let expected = context(5, false);
        sender
            .send_message(EnvelopeMessage::new(
                root("source", 1, &[1]),
                expected.clone(),
            ))
            .await
            .unwrap();
        let delivery = receiver.receive().await.unwrap().unwrap();
        assert_eq!(delivery.execution_context(), &expected);
        let (envelope, actual, acknowledgement) = delivery.into_parts();
        assert_eq!(actual, expected);
        assert_eq!(envelope.system().sequence(), 1);
        assert!(acknowledgement.is_none());
        provided.control.close();
        let failure = sender
            .send_message(EnvelopeMessage::new(
                root("source", 2, &[2]),
                expected.clone(),
            ))
            .await
            .unwrap_err();
        assert_eq!(failure.execution_context, expected);
        assert_eq!(failure.envelope.system().sequence(), 2);
        assert_eq!(failure.acceptance(), AcceptanceState::NotAccepted);
    }
}

#[tokio::test]
async fn retained_redelivery_and_multicast_keep_context_without_implicit_acknowledgement() {
    let store = Arc::new(MemoryEnvelopeStore::new(
        NonZeroUsize::new(2).unwrap(),
        RetentionPolicy::Backpressure,
    ));
    let mut retained = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).unwrap();
    let sender = retained.sender();
    let mut receiver = retained.take_receiver().unwrap();
    let expected = context(6, true);
    sender
        .send_message(EnvelopeMessage::new(
            root("source", 1, &[1]),
            expected.clone(),
        ))
        .await
        .unwrap();
    drop(receiver.receive().await.unwrap().unwrap());
    assert_eq!(store.progress(store.generation()).await.unwrap(), 0);
    let delivery = receiver.receive().await.unwrap().unwrap();
    assert_eq!(delivery.execution_context(), &expected);
    delivery
        .into_parts()
        .2
        .unwrap()
        .complete(HandlingOutcome::Handled)
        .await
        .unwrap();
    store.shutdown().await.unwrap();

    let definition = QosChannelDefinition {
        stream: stream("source"),
        capacity: NonZeroUsize::new(2).unwrap(),
        durable: false,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([
            ("one".into(), SubscriptionStart::Earliest),
            ("two".into(), SubscriptionStart::Earliest),
        ]),
    };
    let channel = QosChannel::volatile(definition).unwrap();
    let resource = ResourceId::try_new("channel").unwrap();
    let resources = BTreeMap::from([(resource.clone(), channel.resource())]);
    let mut one = channel
        .definition()
        .pipe(resource.clone(), "one")
        .create_with_resources(&resources)
        .unwrap();
    let mut two = channel
        .definition()
        .pipe(resource, "two")
        .create_with_resources(&resources)
        .unwrap();
    let mut first = one.pipe.take_receiver().unwrap();
    let mut second = two.pipe.take_receiver().unwrap();
    channel
        .publish_message(&EnvelopeMessage::new(
            root("source", 1, &[1]),
            expected.clone(),
        ))
        .await
        .unwrap();
    let delivery = first.receive().await.unwrap().unwrap();
    assert_eq!(delivery.execution_context(), &expected);
    delivery
        .into_parts()
        .2
        .unwrap()
        .complete(HandlingOutcome::Handled)
        .await
        .unwrap();
    let delivery = second.receive().await.unwrap().unwrap();
    assert_eq!(delivery.execution_context(), &expected);
    delivery
        .into_parts()
        .2
        .unwrap()
        .complete(HandlingOutcome::Handled)
        .await
        .unwrap();
    channel.shutdown().await.unwrap();
}

struct Probe {
    descriptor: ComponentDescriptor,
    graph: String,
    instance: String,
}

#[async_trait]
impl ComputationComponent for Probe {
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
impl Transformer for Probe {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        assert_eq!(ExecutionContext::current(), input.execution_context);
        tokio::task::yield_now().await;
        assert_eq!(ExecutionContext::current(), input.execution_context);
        let scope = ExecutionScope::current_component().expect("receiver binding");
        assert_eq!(scope.identity.instance_id(), self.instance);
        assert_eq!(scope.identity.graph_id(), self.graph);
        assert_eq!(scope.identity.component_id().unwrap(), &component("middle"));
        tracing::info_span!(parent: None, "explicit.log.root").in_scope(|| {
            tracing::info!(target: "context-probe", "context probe");
        });
        Ok(vec![OutputEnvelope::new(
            port("out"),
            derived(
                &input.envelope,
                "middle",
                input.envelope.system().sequence(),
                input.envelope.changes().clone(),
            ),
        )
        .with_execution_context(input.execution_context)])
    }
}

async fn graph_scenario(instance: &str, seed: u8) {
    let logging = ComponentLogRuntime::new(LogPolicy::new("info").unwrap()).unwrap();
    let subscriber = tracing_subscriber::registry().with(logging.layer());
    let contexts = [context(seed, true), ExecutionContext::default(), context(seed + 1, false)];
    let received = Received::default();
    let sibling = Received::default();
    let graph_id = format!("graph-{instance}");
    let runtime_instance = graph_id.clone();
    let mut graph = ComputationGraph::builder(graph_id.clone())
        .with_component_logging(logging.clone())
        .with_telemetry(
            TelemetryOptions::new(
                TelemetrySettings::new(TelemetryProfile::Basic).with_metrics(false),
            )
            .with_instance_id(instance.to_owned()),
        )
        .source(Box::new(FiniteSource::new(
            "source",
            contexts
                .iter()
                .enumerate()
                .map(|(index, context)| {
                    output(root("source", index as u64 + 1, &[1]))
                        .with_execution_context(context.clone())
                })
                .collect(),
        )))
        .transformer(Box::new(Probe {
            descriptor: descriptor("middle", &["in"], &["out"]),
            graph: graph_id,
            instance: runtime_instance.clone(),
        }))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            received.clone(),
        )))
        .sink(Box::new(CollectSink::new(
            "sibling",
            &["in"],
            sibling.clone(),
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("middle", "out"), stream("middle"))
        .connect(
            edge("source", "middle"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("middle", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("middle", "sibling"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    async {
        tokio::time::timeout(Duration::from_secs(10), graph.start().unwrap())
            .await
            .unwrap()
            .unwrap();
        graph.dispose().await.unwrap();
    }
    .with_subscriber(subscriber)
    .await;
    for received in [received, sibling] {
        let received = received.lock().unwrap();
        assert_eq!(received.len(), contexts.len());
        for (input, expected) in received.iter().zip(&contexts) {
            assert_eq!(&input.execution_context, expected);
        }
    }
    logging.flush().await.unwrap();
    let kind = ComponentLogType::Transformer;
    let logs = logging
        .registry()
        .get_history_by_key(&ComponentLogKey::new(&runtime_instance, kind, "middle"))
        .await;
    assert_eq!(
        logs.iter()
            .filter(|log| log.message.contains("context probe"))
            .count(),
        3
    );
    assert!(logs.iter().all(|log| log.instance_id == runtime_instance));
    logging.shutdown(Duration::from_secs(2)).await.unwrap();
    assert!(ExecutionContext::current().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn graph_scope_isolation_current_thread() {
    tokio::join!(graph_scenario("one", 10), graph_scenario("two", 20));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_scope_isolation_multi_thread() {
    tokio::join!(graph_scenario("one", 30), graph_scenario("two", 40));
}
