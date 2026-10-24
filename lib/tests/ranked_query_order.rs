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

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{
    bootstrap::BootstrapResult,
    channels::{
        BootstrapEvent, ChangeReceiver, ComponentStatusHandle, SourceEvent, SourceEventWrapper,
        SubscriptionResponse,
    },
    config::SourceSubscriptionSettings,
    profiling::ProfilingMetadata,
    ComponentStatus, DrasiLib, Source, SourceRuntimeContext,
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Mutex};

enum Message {
    Event(Arc<SourceEventWrapper>),
    Barrier(oneshot::Sender<()>),
}
struct Receiver(mpsc::Receiver<Message>);
#[async_trait]
impl ChangeReceiver<SourceEventWrapper> for Receiver {
    async fn recv(&mut self) -> anyhow::Result<Arc<SourceEventWrapper>> {
        loop {
            match self.0.recv().await {
                Some(Message::Event(event)) => return Ok(event),
                Some(Message::Barrier(done)) => {
                    let _ = done.send(());
                }
                None => anyhow::bail!("source closed"),
            }
        }
    }
}
struct Subscription {
    sender: mpsc::Sender<Message>,
    bootstrap: Option<mpsc::Sender<BootstrapEvent>>,
    complete: Option<oneshot::Sender<anyhow::Result<BootstrapResult>>>,
    bootstrap_count: usize,
}
#[derive(Clone)]
struct OrderedSource {
    id: String,
    status: ComponentStatusHandle,
    sequence: Arc<AtomicU64>,
    subscription_count: Arc<AtomicUsize>,
    subscribers: Arc<Mutex<BTreeMap<String, Subscription>>>,
}
impl OrderedSource {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            status: ComponentStatusHandle::new(id),
            sequence: Arc::new(AtomicU64::new(1)),
            subscription_count: Arc::new(AtomicUsize::new(0)),
            subscribers: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
    async fn wait_subscriptions(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.subscription_count.load(Ordering::Acquire) < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("query subscriptions were not established");
    }
    async fn emit(&self, timestamp: i64) {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let mut event = SourceEventWrapper::with_sequence(
            self.id.clone(),
            SourceEvent::Change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(&self.id, &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        // Deliberately unrelated to the reported wrapper time.
                        effective_from: 50_000 - sequence,
                    },
                    properties: ElementPropertyMap::from(serde_json::json!({
                        "origin": self.id, "sequence": sequence,
                    })),
                },
            }),
            chrono::DateTime::from_timestamp_millis(timestamp).unwrap(),
            sequence,
            Some(ProfilingMetadata {
                source_ns: Some(777),
                source_receive_ns: Some(sequence),
                ..Default::default()
            }),
        );
        event.set_source_position(Bytes::copy_from_slice(&sequence.to_be_bytes()));
        let event = Arc::new(event);
        for subscription in self.subscribers.lock().await.values() {
            subscription
                .sender
                .send(Message::Event(event.clone()))
                .await
                .unwrap();
        }
    }
    async fn flush(&self) {
        let mut barriers = Vec::new();
        for subscription in self.subscribers.lock().await.values() {
            let (done, wait) = oneshot::channel();
            subscription
                .sender
                .send(Message::Barrier(done))
                .await
                .unwrap();
            barriers.push(wait);
        }
        for barrier in barriers {
            tokio::time::timeout(Duration::from_secs(5), barrier)
                .await
                .unwrap()
                .unwrap();
        }
    }
    async fn release_bootstrap(&self) {
        for subscription in self.subscribers.lock().await.values_mut() {
            drop(subscription.bootstrap.take());
            if let Some(complete) = subscription.complete.take() {
                complete
                    .send(Ok(BootstrapResult {
                        event_count: subscription.bootstrap_count,
                        source_position: None,
                    }))
                    .unwrap();
            }
        }
    }
    async fn bootstrap_event(&self, change: SourceChange) {
        for subscription in self.subscribers.lock().await.values_mut() {
            let sequence = u64::try_from(subscription.bootstrap_count).unwrap();
            subscription
                .bootstrap
                .as_ref()
                .unwrap()
                .send(BootstrapEvent {
                    source_id: self.id.clone(),
                    change: change.clone(),
                    timestamp: chrono::DateTime::from_timestamp_millis(1_000).unwrap(),
                    sequence,
                })
                .await
                .unwrap();
            subscription.bootstrap_count += 1;
        }
    }
}
#[async_trait]
impl Source for OrderedSource {
    fn id(&self) -> &str {
        &self.id
    }
    fn type_name(&self) -> &str {
        "ranked-source-fixture"
    }
    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        Default::default()
    }
    fn supports_replay(&self) -> bool {
        false
    }
    async fn initialize(&self, context: SourceRuntimeContext) {
        self.status.wire(context.update_tx).await;
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.status.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.status.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.status.get_status().await
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        let (sender, receiver) = mpsc::channel(16);
        let (bootstrap, bootstrap_receiver) = mpsc::channel(1);
        let (complete, bootstrap_result_receiver) = oneshot::channel();
        self.subscribers.lock().await.insert(
            settings.query_id.clone(),
            Subscription {
                sender,
                bootstrap: Some(bootstrap),
                complete: Some(complete),
                bootstrap_count: 0,
            },
        );
        self.subscription_count.fetch_add(1, Ordering::Release);
        Ok(SubscriptionResponse {
            query_id: settings.query_id,
            source_id: self.id.clone(),
            receiver: Box::new(Receiver(receiver)),
            bootstrap_receiver: Some(bootstrap_receiver),
            bootstrap_result_receiver: Some(bootstrap_result_receiver),
            position_handle: None,
        })
    }
}

async fn scenario() {
    let z = OrderedSource::new("z-source");
    let a = OrderedSource::new("a-source");
    let earlier = OrderedSource::new("early");
    let quiet = OrderedSource::new("quiet");
    let mut builder = DrasiLib::builder()
        .with_source(z.clone())
        .with_source(a.clone())
        .with_source(earlier.clone())
        .with_source(quiet.clone());
    for (id, sources) in [
        ("z-first", ["z-source", "a-source", "early", "quiet"]),
        ("a-first", ["a-source", "z-source", "early", "quiet"]),
    ] {
        let mut query = drasi_lib::Query::cypher(id)
            .query("MATCH (n:Item) RETURN n.origin AS origin, n.sequence AS sequence")
            .with_priority_queue_capacity(16)
            .with_outbox_capacity(32)
            .enable_bootstrap(true)
            .auto_start(true);
        for source in sources {
            query = query.from_source(source);
        }
        builder = builder.with_query(query.build());
    }
    let core = builder.build().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), core.start())
        .await
        .unwrap()
        .unwrap();
    for source in [&a, &z, &earlier, &quiet] {
        source.wait_subscriptions(2).await;
    }
    a.emit(2_000).await;
    a.emit(2_000).await;
    z.emit(2_000).await;
    z.emit(2_000).await;
    earlier.emit(1_000).await;
    // A receiver asks for a barrier only after its previous event was admitted.
    // Thus the whole bounded backlog exists before either query can dequeue.
    for source in [&a, &z, &earlier, &quiet] {
        source.flush().await;
    }
    for source in [&a, &z, &earlier, &quiet] {
        source.release_bootstrap().await;
    }
    for id in ["z-first", "a-first"] {
        core.computation_component(id)
            .expect("query belongs to the computation graph")
            .wait_started()
            .await
            .unwrap();
        let query = core.query_manager().get_query_instance(id).await.unwrap();
        let outbox = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let outbox = query.fetch_outbox(0).await.unwrap();
                if outbox.results.len() == 5 {
                    break outbox;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let expected = if id == "z-first" {
            vec![("early", 1), ("z-source", 1), ("z-source", 2), ("a-source", 1), ("a-source", 2)]
        } else {
            vec![("early", 1), ("a-source", 1), ("a-source", 2), ("z-source", 1), ("z-source", 2)]
        };
        for ((origin, source_sequence), result) in expected.into_iter().zip(&outbox.results) {
            assert_eq!(result.query_id, id);
            assert_eq!(result.metadata["source_id"], origin);
            assert_eq!(result.results.len(), 1);
            let drasi_lib::channels::ResultDiff::Add { data, .. } = &result.results[0] else {
                panic!("expected add")
            };
            assert_eq!(
                data,
                &serde_json::json!({"origin":origin,"sequence":source_sequence})
            );
            let profiling = result.profiling.as_ref().expect("profiling retained");
            assert_eq!(profiling.source_ns, Some(777));
            assert_eq!(profiling.source_receive_ns, Some(source_sequence));
            assert!(profiling.query_receive_ns.is_some());
            assert!(profiling.query_core_call_ns.is_some());
            assert!(profiling.query_core_return_ns.is_some());
            assert!(profiling.query_send_ns.is_some());
        }
        assert_eq!(
            outbox
                .results
                .iter()
                .map(|result| result.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
    }
    core.stop().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut stopped = true;
            for source in ["z-source", "a-source", "early", "quiet"] {
                stopped &=
                    core.get_source_status(source).await.unwrap() == ComponentStatus::Stopped;
            }
            if stopped {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), core.start())
        .await
        .unwrap()
        .unwrap();
    for source in [&a, &z, &earlier, &quiet] {
        source.wait_subscriptions(4).await;
    }
    a.emit(3_000).await;
    z.emit(3_000).await;
    for source in [&a, &z, &earlier, &quiet] {
        source.flush().await;
    }
    for source in [&a, &z, &earlier, &quiet] {
        source.release_bootstrap().await;
    }
    for id in ["z-first", "a-first"] {
        let query = core.query_manager().get_query_instance(id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = query.fetch_snapshot().await.unwrap();
                if snapshot
                    .to_vec()
                    .iter()
                    .any(|row| row["origin"] == "z-source" && row["sequence"] == 3)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn query_specific_source_ranks_do_not_use_lexical_or_effective_time_ordering() {
    scenario().await;
}

#[tokio::test]
async fn scheduled_batches_keep_original_source_provenance() {
    let source = OrderedSource::new("source");
    let core = DrasiLib::builder()
            .with_source(source.clone())
            .with_query(drasi_lib::Query::cypher("timed")
                .query("MATCH (n:Item) WHERE drasi.trueFor(n.ready, duration({seconds:5})) RETURN n.name AS name")
                .from_source("source")
                .enable_bootstrap(true)
                .auto_start(true)
                .build())
            .build().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), core.start())
        .await
        .unwrap()
        .unwrap();
    source.wait_subscriptions(1).await;
    for index in 0..3u64 {
        source
            .bootstrap_event(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &format!("node-{index}")),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: 1_000 + index,
                    },
                    properties: ElementPropertyMap::from(serde_json::json!({
                        "name": format!("node-{index}"), "ready":true,
                    })),
                },
            })
            .await;
    }
    source.release_bootstrap().await;
    let query = core
        .query_manager()
        .get_query_instance("timed")
        .await
        .unwrap();
    let results = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let outbox = query.fetch_outbox(0).await.unwrap();
            if outbox.results.len() == 3 {
                break outbox.results;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    for (index, result) in results.iter().enumerate() {
        assert_eq!(result.sequence, index as u64 + 1);
        assert_eq!(result.query_id, "timed");
        assert_eq!(result.metadata["source_id"], "source");
        assert!(result.profiling.is_some());
        let [drasi_lib::channels::ResultDiff::Add { data, .. }] = result.results.as_slice() else {
            panic!("expected one scheduled add");
        };
        assert_eq!(data, &serde_json::json!({"name": format!("node-{index}")}));
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn source_framework_sequence_is_shared_before_fanout_and_preserves_resume_floors() {
    use drasi_lib::sources::{SourceBase, SourceBaseParams};
    let source = SourceBase::new(SourceBaseParams::new("shared")).unwrap();
    let mut first = source.try_test_subscribe().await.unwrap();
    let mut second = source.try_test_subscribe().await.unwrap();
    let event = |sequence| {
        SourceEventWrapper::with_profiling(
            "shared".into(),
            SourceEvent::Change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("shared", "node"),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: 123,
                    },
                    properties: ElementPropertyMap::default(),
                },
            }),
            chrono::DateTime::from_timestamp_millis(1000).unwrap(),
            ProfilingMetadata {
                source_ns: Some(777),
                ..Default::default()
            },
            sequence,
        )
    };
    for expected in [1, 2] {
        source
            .dispatch_event(event(source.next_sequence()))
            .await
            .unwrap();
        let one = first.recv().await.unwrap();
        let two = second.recv().await.unwrap();
        assert_eq!(one.sequence, expected);
        assert_eq!(two.sequence, one.sequence);
        assert_eq!(one.timestamp, two.timestamp);
        assert_eq!(one.profiling, two.profiling);
        assert_eq!(one.profiling.as_ref().unwrap().source_ns, Some(777));
        assert_eq!(one.event, two.event);
    }
    let response = source
        .subscribe_with_bootstrap(
            &SourceSubscriptionSettings {
                source_id: "shared".into(),
                query_id: "resumed".into(),
                nodes: Default::default(),
                relations: Default::default(),
                enable_bootstrap: false,
                resume_sequence: Some(500),
                resume_from: None,
                request_position_handle: false,
            },
            "ranked-sequence",
        )
        .await
        .unwrap();
    let mut resumed = response.receiver;
    source
        .dispatch_event(event(source.next_sequence()))
        .await
        .unwrap();
    for receiver in [&mut first, &mut second, &mut resumed] {
        assert_eq!(receiver.recv().await.unwrap().sequence, 501);
    }
    source.dispatch_event(event(900)).await.unwrap();
    source.set_next_sequence(1);
    source
        .dispatch_event(event(source.next_sequence()))
        .await
        .unwrap();
    for receiver in [&mut first, &mut second, &mut resumed] {
        assert_eq!(receiver.recv().await.unwrap().sequence, 900);
        assert_eq!(receiver.recv().await.unwrap().sequence, 901);
    }
}
