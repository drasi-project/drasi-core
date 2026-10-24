// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[allow(dead_code)]
mod computation_support;

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use computation_support::*;
use drasi_lib::computation::v1::*;

#[cfg(feature = "computation-rocksdb-tests")]
#[path = "computation_time_merge/recovery.rs"]
mod recovery;

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("positive test limit")
}

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(
        schema_descriptor(),
        Arc::new(ReadingValidator(schema_descriptor())),
    ))
}

fn definition() -> SourceTimeMergeDefinition {
    SourceTimeMergeDefinition {
        graph_id: "merge-test".into(),
        id: component("merge"),
        sources: vec![stream("a"), stream("b")],
        output_stream: stream("merge"),
        late_output_stream: None,
        reorder_window_ms: 100,
        max_wait_ms: 1000,
        idle_timeout_ms: None,
        max_buffered_events: size(32),
        max_buffered_bytes: size(128 * 1024),
        late_policy: LateEventPolicy::FailAndRetain,
    }
}

fn time(value: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(value).expect("test source time")
}

fn event(source: &str, sequence: u64, timestamp: i64) -> ChangeEnvelope {
    let mut envelope = ChangeEnvelope::new(
        emission_id(&stream(source), sequence).expect("source emission identity"),
        changes(
            source,
            sequence,
            &[u16::try_from(sequence).expect("test reading")],
        ),
        SystemMetadata::new(stream(source), sequence).with_timestamp(time(timestamp)),
    );
    envelope
        .append_annotation(annotation(source))
        .expect("source annotation");
    envelope
}

fn input(envelope: ChangeEnvelope) -> InputEnvelope {
    InputEnvelope {
        port: port("in"),
        envelope,
    }
}

async fn open(definition: SourceTimeMergeDefinition) -> SourceTimeMergeTransformer {
    let mut merger = SourceTimeMergeTransformer::new(definition, schema()).expect("valid merger");
    merger.start().await.expect("start merger");
    merger
}

fn timestamp(output: &OutputEnvelope) -> i64 {
    output
        .envelope
        .system()
        .timestamp()
        .expect("output source time")
        .timestamp_millis()
}

fn logical(output: &OutputEnvelope) -> u64 {
    GraphProducerProgress::from_envelope(&output.envelope)
        .expect("valid output progress")
        .expect("explicit output progress")
        .sequence()
}

async fn collect(
    merger: &mut SourceTimeMergeTransformer,
    mut batch: Vec<OutputEnvelope>,
) -> Vec<OutputEnvelope> {
    let mut result = Vec::new();
    loop {
        merger
            .delivery_completed(&batch)
            .await
            .expect("confirm output");
        result.append(&mut batch);
        if !merger.has_pending_emissions() {
            return result;
        }
        batch = merger.continue_transform().await.expect("next output");
        assert_eq!(batch.len(), 1, "bounded continuations");
    }
}

#[tokio::test(start_paused = true)]
async fn reorders_within_and_across_sources_using_only_source_time() {
    let mut merger = open(definition()).await;
    let inputs = [event("a", 1, 90), event("b", 1, 70), event("a", 2, 30), event("b", 2, 10)];
    for original in &inputs {
        assert!(merger
            .transform(input(original.clone()))
            .await
            .unwrap()
            .is_empty());
    }
    tokio::time::advance(Duration::from_millis(999)).await;
    assert!(!merger.has_pending_emissions());
    assert!(merger.on_wakeup().await.unwrap().is_empty());
    tokio::time::advance(Duration::from_millis(1)).await;
    merger.wakeup_source().unwrap().wait().await.unwrap();
    let batch = merger.on_wakeup().await.unwrap();
    let outputs = collect(&mut merger, batch).await;
    assert_eq!(
        outputs.iter().map(timestamp).collect::<Vec<_>>(),
        [10, 30, 70, 90]
    );
    assert_eq!(
        outputs.iter().map(logical).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    for (output, original) in outputs.iter().zip(inputs.iter().rev()) {
        assert!(Arc::ptr_eq(output.envelope.changes(), original.changes()));
        assert_eq!(
            output.envelope.lineage().unwrap().envelope_id(),
            original.id()
        );
        assert_eq!(
            output.envelope.lineage().unwrap().system(),
            original.system()
        );
        assert_eq!(
            output
                .envelope
                .annotations()
                .entries()
                .last()
                .unwrap()
                .contributor(),
            original
                .annotations()
                .entries()
                .last()
                .unwrap()
                .contributor()
        );
    }
    assert_eq!(merger.subscribe().borrow().buffered_bytes, 0);
    assert!(!merger.wakeup_source().unwrap().has_pending().await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn all_cross_source_interleavings_have_the_same_sorted_output() {
    // All six interleavings preserving each producer's admission sequence.
    for sources in ["aabb", "abab", "abba", "baab", "baba", "bbaa"] {
        let mut merger = open(definition()).await;
        let mut sequences = [0, 0];
        for source in sources.chars() {
            let rank = usize::from(source == 'b');
            sequences[rank] += 1;
            let sequence = sequences[rank];
            let timestamp = match (rank, sequence) {
                (0, 1) => 60,
                (0, 2) => 20,
                (1, 1) => 40,
                _ => 10,
            };
            assert!(merger
                .transform(input(event(&source.to_string(), sequence, timestamp)))
                .await
                .unwrap()
                .is_empty());
        }
        tokio::time::advance(Duration::from_millis(1000)).await;
        let batch = merger.on_wakeup().await.unwrap();
        assert_eq!(
            collect(&mut merger, batch)
                .await
                .iter()
                .map(timestamp)
                .collect::<Vec<_>>(),
            [10, 20, 40, 60]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn equal_times_use_source_rank_then_sequence_and_are_not_late() {
    let mut merger = open(definition()).await;
    for original in [event("b", 1, 50), event("a", 1, 50), event("a", 2, 50)] {
        assert!(merger.transform(input(original)).await.unwrap().is_empty());
    }
    tokio::time::advance(Duration::from_millis(1000)).await;
    let batch = merger.on_wakeup().await.unwrap();
    let outputs = collect(&mut merger, batch).await;
    let ancestors: Vec<_> = outputs
        .iter()
        .map(|output| {
            let system = output.envelope.lineage().unwrap().system();
            (system.stream().clone(), system.sequence())
        })
        .collect();
    assert_eq!(
        ancestors,
        [(stream("a"), 1), (stream("a"), 2), (stream("b"), 1)]
    );
    assert!(merger
        .transform(input(event("a", 3, 50)))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(merger.subscribe().borrow().late_events, 0);
}

#[tokio::test(start_paused = true)]
async fn progress_releases_early_but_missing_and_idle_sources_are_explicit() {
    let mut config = definition();
    config.idle_timeout_ms = Some(100);
    let mut merger = open(config).await;
    assert!(merger
        .transform(input(event("a", 1, 10)))
        .await
        .unwrap()
        .is_empty());
    tokio::time::advance(Duration::from_millis(90)).await;
    assert!(merger
        .transform(input(event("a", 2, 110)))
        .await
        .unwrap()
        .is_empty());
    tokio::time::advance(Duration::from_millis(9)).await;
    assert!(!merger.has_pending_emissions());
    tokio::time::advance(Duration::from_millis(1)).await;
    let batch = merger.on_wakeup().await.unwrap();
    assert_eq!(timestamp(&batch[0]), 10);
    collect(&mut merger, batch).await;
    // b becomes active again and constrains early release.
    assert!(merger
        .transform(input(event("b", 1, 20)))
        .await
        .unwrap()
        .is_empty());
    let batch = merger.transform(input(event("b", 2, 210))).await.unwrap();
    assert!(batch.is_empty(), "a's high-water minus 100 is still 10");
    let batch = merger.transform(input(event("a", 3, 210))).await.unwrap();
    assert_eq!(
        collect(&mut merger, batch)
            .await
            .iter()
            .map(timestamp)
            .collect::<Vec<_>>(),
        [20, 110]
    );
}

#[tokio::test(start_paused = true)]
async fn expired_later_event_releases_newer_arriving_earlier_events_first() {
    let mut merger = open(definition()).await;
    merger.transform(input(event("a", 1, 90))).await.unwrap();
    tokio::time::advance(Duration::from_millis(900)).await;
    merger.transform(input(event("b", 1, 10))).await.unwrap();
    tokio::time::advance(Duration::from_millis(100)).await;
    let batch = merger.on_wakeup().await.unwrap();
    assert_eq!(
        collect(&mut merger, batch)
            .await
            .iter()
            .map(timestamp)
            .collect::<Vec<_>>(),
        [10, 90]
    );
}

#[tokio::test(start_paused = true)]
async fn default_policy_holds_late_input_until_explicit_stopped_discard() {
    let mut merger = open(definition()).await;
    merger.transform(input(event("a", 1, 100))).await.unwrap();
    tokio::time::advance(Duration::from_millis(1000)).await;
    let batch = merger.on_wakeup().await.unwrap();
    collect(&mut merger, batch).await;
    let late = event("b", 1, 90);
    let error = merger.transform(input(late.clone())).await.unwrap_err();
    assert!(matches!(
        error.downcast_ref(),
        Some(SourceTimeMergeError::Late { .. })
    ));
    assert_eq!(
        merger
            .subscribe()
            .borrow()
            .held_event
            .as_ref()
            .unwrap()
            .id(),
        late.id()
    );
    assert!(
        merger.discard_held_event().await.is_err(),
        "administrative action requires stop"
    );
    merger.stop().await.unwrap();
    assert!(merger.start().await.is_err());
    merger.discard_held_event().await.unwrap();
    merger.start().await.unwrap();
    assert!(
        merger.transform(input(late)).await.unwrap().is_empty(),
        "held discard retained its receipt"
    );
    assert_eq!(merger.subscribe().borrow().discarded, 1);
    assert!(merger.transform(input(event("b", 2, 110))).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn route_and_discard_preserve_main_frontier_and_per_output_progress() {
    for policy in [LateEventPolicy::Route, LateEventPolicy::Discard] {
        let mut config = definition();
        config.late_policy = policy;
        config.late_output_stream = (policy == LateEventPolicy::Route).then(|| stream("late"));
        let mut merger = open(config).await;
        merger.transform(input(event("a", 1, 100))).await.unwrap();
        tokio::time::advance(Duration::from_millis(1000)).await;
        let batch = merger.on_wakeup().await.unwrap();
        assert_eq!(
            collect(&mut merger, batch)
                .await
                .iter()
                .map(logical)
                .collect::<Vec<_>>(),
            [1]
        );
        for sequence in 1..=2 {
            let batch = merger
                .transform(input(event("b", sequence, 90)))
                .await
                .unwrap();
            if policy == LateEventPolicy::Route {
                assert_eq!(batch[0].port, port("late"));
                assert_eq!(logical(&batch[0]), sequence);
                assert_eq!(timestamp(&batch[0]), 90);
                assert!(batch[0]
                    .envelope
                    .annotations()
                    .entries()
                    .any(|entry| entry.key() == "drasi.time-merge.late-after"));
            } else {
                assert!(batch.is_empty());
            }
            collect(&mut merger, batch).await;
        }
        assert_eq!(
            merger.subscribe().borrow().last_emitted_time,
            Some(time(100))
        );
        assert_eq!(merger.subscribe().borrow().late_events, 2);
        merger.transform(input(event("a", 2, 200))).await.unwrap();
        tokio::time::advance(Duration::from_millis(1000)).await;
        let batch = merger.on_wakeup().await.unwrap();
        assert_eq!(
            collect(&mut merger, batch)
                .await
                .iter()
                .map(logical)
                .collect::<Vec<_>>(),
            [2]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn exact_byte_and_count_limits_include_unconfirmed_output() {
    let original = event("a", 1, 10);
    let mut config = definition();
    config.max_buffered_bytes = size(BinaryEnvelopeCodec::encoded_size(&original).unwrap());
    config.max_buffered_events = size(1);
    let mut merger = open(config.clone()).await;
    merger.transform(input(original.clone())).await.unwrap();
    let rejected = event("b", 1, 20);
    assert!(matches!(
        merger
            .transform(input(rejected))
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(SourceTimeMergeError::Capacity)
    ));
    assert_eq!(merger.subscribe().borrow().buffered_events, 1);
    tokio::time::advance(Duration::from_millis(1000)).await;
    let batch = merger.on_wakeup().await.unwrap();
    assert_eq!(merger.subscribe().borrow().buffered_events, 1);
    merger.delivery_completed(&batch).await.unwrap();
    assert_eq!(merger.subscribe().borrow().buffered_events, 0);
    config.max_buffered_bytes = size(config.max_buffered_bytes.get() - 1);
    let mut merger = open(config).await;
    assert!(matches!(
        merger
            .transform(input(original))
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(SourceTimeMergeError::Capacity)
    ));
    assert_eq!(merger.subscribe().borrow().buffered_bytes, 0);
}

#[tokio::test]
async fn invalid_inputs_and_replays_do_not_change_admitted_state() {
    let mut merger = open(definition()).await;
    assert!(matches!(
        merger
            .transform(input(root("a", 1, &[1])))
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(SourceTimeMergeError::MissingSourceTime)
    ));
    assert!(matches!(
        merger
            .transform(input(event("unknown", 1, 10)))
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(SourceTimeMergeError::UnknownSource(_))
    ));
    assert!(merger
        .transform(InputEnvelope {
            port: port("wrong"),
            envelope: event("a", 1, 10)
        })
        .await
        .is_err());
    let wrong = ChangeEnvelope::new(
        event("a", 1, 10).id().clone(),
        changes_with_schema(named_schema("foreign"), "a", 1, &[1]),
        SystemMetadata::new(stream("a"), 1).with_timestamp(time(10)),
    );
    assert!(merger.transform(input(wrong)).await.is_err());
    assert!(merger.transform(input(event("a", 0, 10))).await.is_err());
    assert_eq!(merger.subscribe().borrow().buffered_events, 0);
    let first = event("a", 1, 10);
    merger.transform(input(first.clone())).await.unwrap();
    merger.transform(input(first.clone())).await.unwrap();
    assert_eq!(merger.subscribe().borrow().replayed_inputs, 1);
    assert!(matches!(
        merger
            .transform(input(event("a", 1, 20)))
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(SourceTimeMergeError::ReplayConflict)
    ));
    merger.transform(input(event("a", 2, 20))).await.unwrap();
    assert!(matches!(
        merger
            .transform(input(first))
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(SourceTimeMergeError::ReceiptExpired { .. })
    ));
    assert_eq!(merger.subscribe().borrow().buffered_events, 2);
}

#[tokio::test(start_paused = true)]
async fn confirmation_is_exact_and_restart_replays_without_resequencing_logical_output() {
    let mut merger = open(definition()).await;
    merger.transform(input(event("a", 1, 10))).await.unwrap();
    tokio::time::advance(Duration::from_millis(1000)).await;
    let batch = merger.on_wakeup().await.unwrap();
    merger.start().await.unwrap(); // Idempotent start must not clear the in-flight obligation.
    assert!(merger.delivery_completed(&[]).await.is_err());
    let wrong = vec![OutputEnvelope {
        port: port("late"),
        envelope: batch[0].envelope.clone(),
    }];
    assert!(merger.delivery_completed(&wrong).await.is_err());
    merger.stop().await.unwrap();
    merger.start().await.unwrap();
    let replay = merger.continue_transform().await.unwrap();
    assert_eq!(logical(&replay[0]), logical(&batch[0]));
    assert!(replay[0].envelope.system().sequence() > batch[0].envelope.system().sequence());
    assert_eq!(
        replay[0].envelope.lineage().unwrap().envelope_id(),
        batch[0].envelope.lineage().unwrap().envelope_id()
    );
    assert!(merger.delivery_completed(&batch).await.is_err());
    merger.delivery_completed(&replay).await.unwrap();
}

#[test]
fn rejects_invalid_configuration() {
    let base = definition();
    let mut configs = Vec::new();
    let mut c = base.clone();
    c.sources.clear();
    configs.push(c);
    let mut c = base.clone();
    c.sources.push(stream("a"));
    configs.push(c);
    let mut c = base.clone();
    c.sources = (0..257).map(|n| stream(&format!("s{n}"))).collect();
    configs.push(c);
    let mut c = base.clone();
    c.output_stream = stream("a");
    configs.push(c);
    let mut c = base.clone();
    c.max_wait_ms = 0;
    configs.push(c);
    let mut c = base.clone();
    c.idle_timeout_ms = Some(0);
    configs.push(c);
    let mut c = base.clone();
    c.reorder_window_ms = u64::MAX;
    configs.push(c);
    let mut c = base.clone();
    c.late_policy = LateEventPolicy::Route;
    configs.push(c);
    let mut c = base.clone();
    c.late_output_stream = Some(stream("late"));
    configs.push(c);
    let mut c = base.clone();
    c.graph_id.clear();
    configs.push(c);
    let mut c = base;
    c.max_buffered_bytes = size(usize::MAX);
    configs.push(c);
    for config in configs {
        assert!(SourceTimeMergeTransformer::new(config, schema()).is_err());
    }
}

async fn finite_graph() {
    let mut config = definition();
    config.max_wait_ms = 20;
    let merger = SourceTimeMergeTransformer::new(config, schema()).expect("valid merger");
    let observation = merger.subscribe();
    let received = Received::default();
    let mut graph = ComputationGraph::builder("merge-test")
        .source(Box::new(FiniteSource::new(
            "a",
            vec![output(event("a", 1, 40)), output(event("a", 2, 10))],
        )))
        .source(Box::new(FiniteSource::new(
            "b",
            vec![output(event("b", 1, 30)), output(event("b", 2, 20))],
        )))
        .transformer(Box::new(merger))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            received.clone(),
        )))
        .bind_stream(endpoint("a", "out"), stream("a"))
        .bind_stream(endpoint("b", "out"), stream("b"))
        .bind_stream(endpoint("merge", "out"), stream("merge"))
        .connect(
            edge("a", "merge"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("b", "merge"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("merge", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("valid finite graph");
    tokio::time::timeout(
        Duration::from_secs(5),
        graph.start().expect("start finite graph"),
    )
    .await
    .expect("finite drain deadline")
    .expect("finite graph drains");
    assert_eq!(graph.state(), GraphState::Completed);
    assert_eq!(
        received
            .lock()
            .expect("received lock")
            .iter()
            .map(|input| input
                .envelope
                .system()
                .timestamp()
                .expect("output time")
                .timestamp_millis())
            .collect::<Vec<_>>(),
        [10, 20, 30, 40]
    );
    assert_eq!(observation.borrow().buffered_events, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn finite_sources_drain_timer_on_current_thread() {
    finite_graph().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finite_sources_drain_timer_on_multi_thread() {
    finite_graph().await;
}

#[tokio::test(start_paused = true)]
async fn downstream_query_uses_merger_progress_not_reordered_source_sequences() {
    use drasi_core::{
        computation::InMemoryComputationProvider,
        models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    };
    use drasi_lib::channels::{SourceEvent, SourceEventWrapper};
    let mut merger =
        SourceTimeMergeTransformer::new(definition(), GraphChangeCodec::schema()).unwrap();
    merger.start().await.unwrap();
    for (source, sequence, timestamp) in [("a", 1, 90), ("a", 2, 10), ("b", 1, 80), ("b", 2, 20)] {
        let envelope = GraphChangeCodec::encode_source_event(
            Arc::new(SourceEventWrapper {
                source_id: source.into(),
                event: SourceEvent::Change(SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new(source, &format!("node{sequence}")),
                            labels: Arc::from([Arc::from("Item")]),
                            effective_from: timestamp,
                        },
                        properties: ElementPropertyMap::from(
                            serde_json::json!({"value": sequence, "timestamp": 999999}),
                        ),
                    },
                }),
                timestamp: time(timestamp as i64),
                sequence,
                source_position: Some(bytes::Bytes::copy_from_slice(&sequence.to_be_bytes())),
                profiling: None,
            }),
            &component(source),
            stream(source),
            sequence,
            None,
        )
        .unwrap();
        assert!(merger.transform(input(envelope)).await.unwrap().is_empty());
    }
    tokio::time::advance(Duration::from_millis(1000)).await;
    let batch = merger.on_wakeup().await.unwrap();
    let outputs = collect(&mut merger, batch).await;
    assert_eq!(
        outputs.iter().map(timestamp).collect::<Vec<_>>(),
        [10, 20, 80, 90]
    );
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "merge-test".into(),
            id: component("query"),
            query: "MATCH (n:Item) RETURN n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("query"),
            outbox_capacity: size(8),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await
    .unwrap();
    query.start().await.unwrap();
    for (index, output) in outputs.into_iter().enumerate() {
        assert!(GraphChangeCodec::source_metadata(&output.envelope)
            .unwrap()
            .is_some());
        let results = query.transform(input(output.envelope)).await.unwrap();
        query.delivery_completed(&results).await.unwrap();
        assert_eq!(query.results().snapshot().unwrap().rows.len(), index + 1);
    }
    query.stop().await.unwrap();
    merger.stop().await.unwrap();
}

struct QuietSource {
    descriptor: ComponentDescriptor,
    outputs: std::collections::VecDeque<OutputEnvelope>,
}

#[async_trait::async_trait]
impl ComputationComponent for QuietSource {
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

#[async_trait::async_trait]
impl EnvelopeSource for QuietSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        match self.outputs.pop_front() {
            Some(output) => Ok(Some(output)),
            None => std::future::pending().await,
        }
    }
}

struct BlockedSink {
    descriptor: ComponentDescriptor,
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl ComputationComponent for BlockedSink {
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

#[async_trait::async_trait]
impl EnvelopeSink for BlockedSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        self.entered.notify_one();
        std::future::pending().await
    }
}

async fn quiet_graph_cancellation() {
    let mut config = definition();
    config.max_wait_ms = 20;
    let merger = SourceTimeMergeTransformer::new(config, schema()).expect("valid merger");
    let mut observation = merger.subscribe();
    let entered = Arc::new(tokio::sync::Notify::new());
    let mut graph = ComputationGraph::builder("merge-test")
        .source(Box::new(QuietSource {
            descriptor: descriptor("a", &[], &["out"]),
            outputs: (1..=10)
                .map(|sequence| output(event("a", sequence, sequence as i64)))
                .collect(),
        }))
        .source(Box::new(QuietSource {
            descriptor: descriptor("b", &[], &["out"]),
            outputs: Default::default(),
        }))
        .transformer(Box::new(merger))
        .sink(Box::new(BlockedSink {
            descriptor: descriptor("sink", &["in"], &[]),
            entered: entered.clone(),
        }))
        .bind_stream(endpoint("a", "out"), stream("a"))
        .bind_stream(endpoint("b", "out"), stream("b"))
        .bind_stream(endpoint("merge", "out"), stream("merge"))
        .connect(
            edge("a", "merge"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("b", "merge"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("merge", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("valid quiet graph");
    let mut run = Box::pin(graph.start().expect("start quiet graph"));
    let control = run.control();
    tokio::select! {
        result = &mut run => panic!("quiet graph unexpectedly finished: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(5), async {
            entered.notified().await;
            loop {
                if observation.borrow_and_update().emitted >= 3 { break; }
                observation.changed().await.expect("merger observation");
            }
        }) => result.expect("quiet graph wakeup deadline"),
    }
    assert!(
        observation.borrow().buffered_events > 0,
        "backpressure must retain pending work"
    );
    control.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("cancellation deadline"),
        Err(GraphError::Cancelled)
    ));
    assert!(!observation.borrow().running);
}

#[tokio::test(flavor = "current_thread")]
async fn quiet_sources_wake_and_blocked_output_cancels_current_thread() {
    quiet_graph_cancellation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_sources_wake_and_blocked_output_cancels_multi_thread() {
    quiet_graph_cancellation().await;
}

#[tokio::test(start_paused = true)]
async fn large_reversed_burst_stays_bounded_and_drains_without_loss() {
    let mut config = definition();
    config.reorder_window_ms = 10_000;
    config.max_buffered_events = size(1024);
    config.max_buffered_bytes = size(4 << 20);
    let mut merger = open(config.clone()).await;
    for sequence in 1..=512 {
        for (source, offset) in [("a", 0), ("b", 1)] {
            assert!(merger
                .transform(input(event(
                    source,
                    sequence,
                    (1024 - sequence * 2 + offset) as i64
                )))
                .await
                .unwrap()
                .is_empty());
        }
        let state = merger.subscribe().borrow().clone();
        assert_eq!(state.buffered_events, sequence as usize * 2);
        assert!(state.buffered_bytes <= config.max_buffered_bytes.get());
    }
    assert!(matches!(
        merger
            .transform(input(event("a", 513, 1025)))
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(SourceTimeMergeError::Capacity)
    ));
    tokio::time::advance(Duration::from_millis(1000)).await;
    let batch = merger.on_wakeup().await.unwrap();
    assert_eq!(
        collect(&mut merger, batch)
            .await
            .iter()
            .map(timestamp)
            .collect::<Vec<_>>(),
        (0..1024).collect::<Vec<_>>()
    );
    assert_eq!(merger.subscribe().borrow().buffered_bytes, 0);
}

#[tokio::test(start_paused = true)]
async fn idle_sources_do_not_invent_a_watermark_or_release_future_data() {
    let mut config = definition();
    config.idle_timeout_ms = Some(50);
    let mut merger = open(config).await;
    merger.transform(input(event("a", 1, 100))).await.unwrap();
    tokio::time::advance(Duration::from_millis(50)).await;
    assert!(
        merger.on_wakeup().await.unwrap().is_empty(),
        "all sources idle is not a watermark"
    );
    assert!(!merger.has_pending_emissions());
    tokio::time::advance(Duration::from_millis(950)).await;
    let batch = merger.on_wakeup().await.unwrap();
    collect(&mut merger, batch).await;
    assert!(matches!(
        merger
            .transform(input(event("b", 1, 90)))
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(SourceTimeMergeError::Late { .. })
    ));
}

#[tokio::test]
async fn volatile_producer_incarnation_changes_are_not_new_input() {
    let mut merger = open(definition()).await;
    for attempt in 0..2 {
        let identity = GraphProducerIdentity::volatile(
            "test".into(),
            "test".into(),
            component("a"),
            stream("a"),
        )
        .unwrap();
        let mut original = event("a", attempt + 1, 10);
        GraphProducerProgress::annotate(&mut original, &identity, attempt + 1).unwrap();
        let result = merger.transform(input(original)).await;
        if attempt == 0 {
            result.unwrap();
        } else {
            assert!(matches!(
                result.unwrap_err().downcast_ref(),
                Some(SourceTimeMergeError::ReplayConflict)
            ));
        }
    }
}

#[tokio::test]
async fn query_controls_are_not_source_changes() {
    let mut merger =
        SourceTimeMergeTransformer::new(definition(), GraphChangeCodec::schema()).unwrap();
    merger.start().await.unwrap();
    let notification =
        GraphChangeCodec::encode_futures_due(&component("a"), stream("a"), 1, time(10)).unwrap();
    assert!(merger.transform(input(notification)).await.is_err());
    assert_eq!(merger.subscribe().borrow().buffered_events, 0);
}

#[tokio::test(start_paused = true)]
async fn source_idle_grace_starts_at_activation_not_construction() {
    let mut config = definition();
    config.reorder_window_ms = 0;
    config.idle_timeout_ms = Some(100);
    let mut merger = SourceTimeMergeTransformer::new(config, schema()).unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    merger.start().await.unwrap();
    assert!(merger
        .transform(input(event("a", 1, 10)))
        .await
        .unwrap()
        .is_empty());
    tokio::time::advance(Duration::from_millis(99)).await;
    assert!(
        !merger.has_pending_emissions(),
        "b gets its full startup idle grace"
    );
}
