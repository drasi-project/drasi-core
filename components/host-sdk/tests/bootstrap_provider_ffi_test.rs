// Copyright 2025 The Drasi Authors.
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

//! Integration tests for the push-based bootstrap provider FFI contract.
//!
//! These drive the real production path in-process, no cdylib required:
//! a test `BootstrapProvider` is wrapped by `build_bootstrap_provider_vtable`
//! (the producer side compiled into plugins) and consumed through
//! `BootstrapProviderProxy` (the host side), exactly as drasi-lib invokes it.
//!
//! The key invariants under test:
//! - backpressure reaches the provider: a stalled consumer bounds in-flight
//!   events to the sum of the channel capacities, and the provider stalls
//!   instead of streaming the snapshot into memory
//! - no async worker is ever pinned: concurrent bootstraps larger than every
//!   buffer complete on a 2-worker runtime
//! - completion, error, and source_position handover semantics
//! - cancellation: dropping the consuming future stops the provider

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_host_sdk::BootstrapProviderProxy;
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::events::{BootstrapEvent, BootstrapEventSender};
use drasi_plugin_sdk::ffi::{
    build_bootstrap_provider_vtable, release_bootstrap_receiver, release_result_receiver,
    wrap_result_receiver, BootstrapStreamConsumer, FfiBootstrapProviderProxy, FfiStr,
    BOOTSTRAP_BRIDGE_CAPACITY, BOOTSTRAP_PROVIDER_CAPACITY,
};

/// Mirrors the query-side channel in drasi-lib sources/base.rs.
const EVENT_CHANNEL_CAP: usize = 1000;
/// Upper bound on in-flight events when the consumer is stalled: provider
/// channel + consumer bridge + query channel + a small slack for events held
/// by the forwarder threads themselves.
const MAX_IN_FLIGHT: usize =
    BOOTSTRAP_PROVIDER_CAPACITY + BOOTSTRAP_BRIDGE_CAPACITY + EVENT_CHANNEL_CAP + 10;

fn make_node(i: usize) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("probe-src", &format!("n{i}")),
            labels: Arc::from(vec![Arc::from("Probe")]),
            effective_from: 0,
        },
        properties: drasi_core::models::ElementPropertyMap::from(
            serde_json::json!({"idx": i as i64}),
        ),
    }
}

/// Test provider that streams `n` events and tracks progress.
struct StreamingProvider {
    n: usize,
    produced: Arc<AtomicUsize>,
    done_producing: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    fail_after: Option<usize>,
    source_position: Option<Vec<u8>>,
}

#[async_trait]
impl BootstrapProvider for StreamingProvider {
    async fn bootstrap(
        &self,
        _request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> anyhow::Result<BootstrapResult> {
        for i in 0..self.n {
            if let Some(fail_at) = self.fail_after {
                if i == fail_at {
                    anyhow::bail!("provider failed deliberately at event {i}");
                }
            }
            let event = BootstrapEvent {
                source_id: context.source_id.clone(),
                change: SourceChange::Insert {
                    element: make_node(i),
                },
                timestamp: chrono::Utc::now(),
                sequence: i as u64,
            };
            if event_tx.send(event).await.is_err() {
                self.cancelled.store(true, Ordering::SeqCst);
                anyhow::bail!("bootstrap event channel closed");
            }
            self.produced.fetch_add(1, Ordering::SeqCst);
        }
        self.done_producing.store(true, Ordering::SeqCst);
        Ok(BootstrapResult {
            event_count: self.n,
            source_position: self.source_position.clone().map(bytes::Bytes::from),
        })
    }
}

/// The bootstrap path never invokes the vtable executor; return null so any
/// future misuse surfaces as a clean null result instead of a bogus pointer.
extern "C" fn noop_executor(_f: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

struct ProviderHandles {
    produced: Arc<AtomicUsize>,
    done: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

fn make_proxy(n: usize, fail_after: Option<usize>) -> (BootstrapProviderProxy, ProviderHandles) {
    let handles = ProviderHandles {
        produced: Arc::new(AtomicUsize::new(0)),
        done: Arc::new(AtomicBool::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let provider = StreamingProvider {
        n,
        produced: handles.produced.clone(),
        done_producing: handles.done.clone(),
        cancelled: handles.cancelled.clone(),
        fail_after,
        source_position: None,
    };
    let vtable = build_bootstrap_provider_vtable(Box::new(provider), noop_executor);
    (BootstrapProviderProxy::new(vtable, None), handles)
}

fn req() -> BootstrapRequest {
    BootstrapRequest {
        query_id: "probe-query".to_string(),
        node_labels: vec!["Probe".to_string()],
        relation_labels: vec![],
        request_id: "probe-request".to_string(),
    }
}

fn ctx() -> BootstrapContext {
    BootstrapContext::new_minimal("probe-server".to_string(), "probe-src".to_string())
}

/// A consumer that stays idle until the gate opens, then drains everything.
async fn gated_consumer(
    mut rx: tokio::sync::mpsc::Receiver<BootstrapEvent>,
    consumed: Arc<AtomicUsize>,
    mut gate: tokio::sync::watch::Receiver<bool>,
) -> Vec<u64> {
    let mut sequences = Vec::new();
    while !*gate.borrow() {
        if gate.changed().await.is_err() {
            return sequences;
        }
    }
    while let Some(event) = rx.recv().await {
        sequences.push(event.sequence);
        consumed.fetch_add(1, Ordering::SeqCst);
    }
    sequences
}

/// Backpressure: with the consumer fully stalled, the provider must stall at
/// the sum of the channel capacities instead of producing the whole snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_consumer_bounds_in_flight_events() {
    const N: usize = 20_000;
    let (proxy, handles) = make_proxy(N, None);

    let (tx, rx) = tokio::sync::mpsc::channel(EVENT_CHANNEL_CAP);
    let consumed = Arc::new(AtomicUsize::new(0));
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let consumer = tokio::spawn(gated_consumer(rx, consumed.clone(), gate_rx));

    let boot = tokio::spawn(async move { proxy.bootstrap(req(), &ctx(), tx, None).await });

    // Wait for production to plateau (stable over 2 seconds).
    let mut last = 0usize;
    let mut stable_since = Instant::now();
    let start = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let current = handles.produced.load(Ordering::SeqCst);
        if current != last {
            last = current;
            stable_since = Instant::now();
        }
        if stable_since.elapsed() > Duration::from_secs(2)
            || start.elapsed() > Duration::from_secs(60)
        {
            break;
        }
    }

    let plateau = handles.produced.load(Ordering::SeqCst);
    assert!(
        !handles.done.load(Ordering::SeqCst),
        "provider must be stalled by backpressure, not done (produced {plateau})"
    );
    assert!(
        plateau <= MAX_IN_FLIGHT,
        "in-flight events must be bounded by channel capacities: {plateau} > {MAX_IN_FLIGHT}"
    );
    assert_eq!(consumed.load(Ordering::SeqCst), 0);
    assert!(!boot.is_finished());

    // Release the consumer: everything drains, nothing is lost.
    gate_tx.send(true).unwrap();
    let result = boot.await.unwrap().expect("bootstrap failed");
    assert_eq!(result.event_count, N);
    let sequences = consumer.await.unwrap();
    assert_eq!(sequences.len(), N, "no events lost");
    assert!(
        sequences.windows(2).all(|w| w[0] < w[1]),
        "events arrive in order"
    );
    assert!(handles.done.load(Ordering::SeqCst));
}

/// Liveness: concurrent bootstraps larger than every buffer complete on a
/// 2-worker runtime because no FFI call ever blocks an async worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_bootstraps_complete_on_two_workers() {
    const N: usize = 5_000;
    const CONCURRENCY: usize = 4;

    let mut boots = Vec::new();
    let mut consumers = Vec::new();
    let mut gates = Vec::new();
    for _ in 0..CONCURRENCY {
        let (proxy, _handles) = make_proxy(N, None);
        let (tx, rx) = tokio::sync::mpsc::channel(EVENT_CHANNEL_CAP);
        let consumed = Arc::new(AtomicUsize::new(0));
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(true);
        gates.push(gate_tx);
        consumers.push((
            tokio::spawn(gated_consumer(rx, consumed.clone(), gate_rx)),
            consumed,
        ));
        boots.push(tokio::spawn(async move {
            proxy.bootstrap(req(), &ctx(), tx, None).await
        }));
    }

    let all = async {
        for boot in boots {
            let result = boot.await.unwrap().expect("bootstrap failed");
            assert_eq!(result.event_count, N);
        }
        for (consumer, consumed) in consumers {
            consumer.await.unwrap();
            assert_eq!(consumed.load(Ordering::SeqCst), N);
        }
    };
    tokio::time::timeout(Duration::from_secs(120), all)
        .await
        .expect("concurrent bootstraps deadlocked or stalled");
}

/// Provider failure surfaces as an error on the host side after the events
/// sent so far were delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_error_propagates() {
    const N: usize = 500;
    let (proxy, _handles) = make_proxy(N, Some(100));

    let (tx, mut rx) = tokio::sync::mpsc::channel(EVENT_CHANNEL_CAP);
    let drain = tokio::spawn(async move {
        let mut count = 0usize;
        while rx.recv().await.is_some() {
            count += 1;
        }
        count
    });

    let err = proxy
        .bootstrap(req(), &ctx(), tx, None)
        .await
        .expect_err("provider failure must propagate");
    assert!(
        err.to_string().contains("Bootstrap failed"),
        "unexpected error: {err}"
    );
    assert!(
        err.to_string()
            .contains("provider failed deliberately at event 100"),
        "provider error text must cross the FFI boundary: {err}"
    );
    assert_eq!(
        drain.await.unwrap(),
        100,
        "events before the failure arrive"
    );
}

/// The source_position handover metadata survives the crossing intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_position_round_trips() {
    const N: usize = 10;
    let position = b"lsn:0/16B3748".to_vec();
    let provider = StreamingProvider {
        n: N,
        produced: Arc::new(AtomicUsize::new(0)),
        done_producing: Arc::new(AtomicBool::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
        fail_after: None,
        source_position: Some(position.clone()),
    };
    let vtable = build_bootstrap_provider_vtable(Box::new(provider), noop_executor);
    let proxy = BootstrapProviderProxy::new(vtable, None);

    let (tx, mut rx) = tokio::sync::mpsc::channel(EVENT_CHANNEL_CAP);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let result = proxy
        .bootstrap(req(), &ctx(), tx, None)
        .await
        .expect("bootstrap failed");
    drain.await.unwrap();

    assert_eq!(result.event_count, N);
    assert_eq!(result.source_position.as_deref(), Some(position.as_slice()));
}

/// Cancellation: dropping the consuming future mid-stream propagates a channel
/// close to the provider, which stops producing instead of running to
/// completion against a dead stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_bootstrap_future_cancels_provider() {
    const N: usize = 50_000;
    let (proxy, handles) = make_proxy(N, None);

    let (tx, rx) = tokio::sync::mpsc::channel(EVENT_CHANNEL_CAP);
    let consumed = Arc::new(AtomicUsize::new(0));
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(true);
    let consumer = tokio::spawn(gated_consumer(rx, consumed.clone(), gate_rx));
    drop(gate_tx);

    let boot = tokio::spawn(async move { proxy.bootstrap(req(), &ctx(), tx, None).await });

    // Let the stream get going, then abort the bootstrap task mid-flight.
    let start = Instant::now();
    while consumed.load(Ordering::SeqCst) < 1000 && start.elapsed() < Duration::from_secs(30) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        consumed.load(Ordering::SeqCst) >= 1000,
        "stream never started"
    );
    boot.abort();
    let _ = boot.await;

    // The provider must observe the cancellation and bail out.
    let start = Instant::now();
    while !handles.cancelled.load(Ordering::SeqCst)
        && !handles.done.load(Ordering::SeqCst)
        && start.elapsed() < Duration::from_secs(30)
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        handles.cancelled.load(Ordering::SeqCst),
        "provider should observe a closed channel after cancellation (produced {}, done {})",
        handles.produced.load(Ordering::SeqCst),
        handles.done.load(Ordering::SeqCst)
    );

    // The consumer's channel closes too; no hang.
    tokio::time::timeout(Duration::from_secs(30), consumer)
        .await
        .expect("consumer hung after cancellation")
        .unwrap();
}

/// Layered topology: host wraps a plugin provider proxy back into a vtable for
/// a source plugin, which consumes it through FfiBootstrapProviderProxy. Both
/// producer and both consumers run in a chain, and backpressure still bounds
/// the whole path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_hop_chain_stays_bounded_and_lossless() {
    const N: usize = 8_000;
    let (inner_proxy, handles) = make_proxy(N, None);

    // Host-side wrap of the (already proxied) provider, as source.rs does when
    // passing a bootstrap provider into a source plugin.
    let outer_vtable = build_bootstrap_provider_vtable(Box::new(inner_proxy), noop_executor);
    let outer_proxy = FfiBootstrapProviderProxy::new(outer_vtable);

    let (tx, rx) = tokio::sync::mpsc::channel(EVENT_CHANNEL_CAP);
    let consumed = Arc::new(AtomicUsize::new(0));
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let consumer = tokio::spawn(gated_consumer(rx, consumed.clone(), gate_rx));

    let boot = tokio::spawn(async move { outer_proxy.bootstrap(req(), &ctx(), tx, None).await });

    // With the consumer stalled, the provider must plateau even through two
    // hops (each hop adds one provider channel and one bridge).
    let mut last = 0usize;
    let mut stable_since = Instant::now();
    let start = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let current = handles.produced.load(Ordering::SeqCst);
        if current != last {
            last = current;
            stable_since = Instant::now();
        }
        if stable_since.elapsed() > Duration::from_secs(2)
            || start.elapsed() > Duration::from_secs(60)
        {
            break;
        }
    }
    let plateau = handles.produced.load(Ordering::SeqCst);
    // Each hop adds one provider channel and one bridge; the query channel
    // appears once at the end of the chain.
    let two_hop_bound =
        2 * (BOOTSTRAP_PROVIDER_CAPACITY + BOOTSTRAP_BRIDGE_CAPACITY) + EVENT_CHANNEL_CAP + 20;
    assert!(
        !handles.done.load(Ordering::SeqCst),
        "provider must stall through the double hop (produced {plateau})"
    );
    assert!(
        plateau <= two_hop_bound,
        "double-hop in-flight events must stay bounded: {plateau} > {two_hop_bound}"
    );

    gate_tx.send(true).unwrap();
    let result = boot.await.unwrap().expect("double-hop bootstrap failed");
    assert_eq!(result.event_count, N);
    consumer.await.unwrap();
    assert_eq!(consumed.load(Ordering::SeqCst), N, "no events lost");
}

/// Error-path cleanup: releasing the receivers of an unconsumed stream must
/// unblock and cancel the provider instead of leaking it, and the provider
/// must not have produced past its bounded channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn releasing_receivers_cancels_provider() {
    const N: usize = 10_000;
    // Drive the raw vtable surface directly, the way a consumer that fails
    // mid-setup would.
    let handles = ProviderHandles {
        produced: Arc::new(AtomicUsize::new(0)),
        done: Arc::new(AtomicBool::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let provider = StreamingProvider {
        n: N,
        produced: handles.produced.clone(),
        done_producing: handles.done.clone(),
        cancelled: handles.cancelled.clone(),
        fail_after: None,
        source_position: None,
    };
    let vtable = build_bootstrap_provider_vtable(Box::new(provider), noop_executor);

    let stream_ptr = (vtable.bootstrap_fn)(
        vtable.state,
        FfiStr::from_str("probe-query"),
        std::ptr::null(),
        0,
        std::ptr::null(),
        0,
        FfiStr::from_str("probe-request"),
        FfiStr::from_str("probe-server"),
        FfiStr::from_str("probe-src"),
    );
    assert!(!stream_ptr.is_null());
    let stream = unsafe { *Box::from_raw(stream_ptr) };
    let events = unsafe { *Box::from_raw(stream.events) };
    let result = unsafe { *Box::from_raw(stream.result) };

    // Simulate a consumer that fails before consuming: release both handles.
    release_bootstrap_receiver(events);
    release_result_receiver(result);

    // The provider must observe the closed event channel and bail out.
    let start = Instant::now();
    while !handles.cancelled.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(30) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        handles.cancelled.load(Ordering::SeqCst),
        "provider must be cancelled after receivers are released (produced {})",
        handles.produced.load(Ordering::SeqCst)
    );
    // It can only have filled its bounded channel before stalling.
    assert!(
        handles.produced.load(Ordering::SeqCst) <= BOOTSTRAP_PROVIDER_CAPACITY + 1,
        "provider must not produce past its bounded channel: {}",
        handles.produced.load(Ordering::SeqCst)
    );

    (vtable.drop_fn)(vtable.state);
}

/// Null label arrays with a non-zero count must be treated as empty instead of
/// dereferenced, and the stream must still work end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn null_label_arrays_are_treated_as_empty() {
    const N: usize = 50;
    let handles = ProviderHandles {
        produced: Arc::new(AtomicUsize::new(0)),
        done: Arc::new(AtomicBool::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let provider = StreamingProvider {
        n: N,
        produced: handles.produced.clone(),
        done_producing: handles.done.clone(),
        cancelled: handles.cancelled.clone(),
        fail_after: None,
        source_position: None,
    };
    let vtable = build_bootstrap_provider_vtable(Box::new(provider), noop_executor);

    // Null arrays with NON-ZERO counts: must not be walked.
    let stream_ptr = (vtable.bootstrap_fn)(
        vtable.state,
        FfiStr::from_str("probe-query"),
        std::ptr::null(),
        3,
        std::ptr::null(),
        2,
        FfiStr::from_str("probe-request"),
        FfiStr::from_str("probe-server"),
        FfiStr::from_str("probe-src"),
    );
    assert!(!stream_ptr.is_null());
    let stream = unsafe { *Box::from_raw(stream_ptr) };
    let events = unsafe { *Box::from_raw(stream.events) };
    let result = unsafe { *Box::from_raw(stream.result) };

    let consumer = BootstrapStreamConsumer::new(events);
    let (result_rx, _guard) = wrap_result_receiver(result);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<BootstrapEvent>(EVENT_CHANNEL_CAP);
    let drain = tokio::spawn(async move {
        let mut n = 0usize;
        while rx.recv().await.is_some() {
            n += 1;
        }
        n
    });
    let forwarded = consumer.forward_into(&tx).await;
    drop(tx);
    let outcome = result_rx
        .await
        .expect("result must arrive")
        .expect("bootstrap failed");

    assert_eq!(forwarded, N);
    assert_eq!(outcome.event_count, N);
    assert_eq!(drain.await.unwrap(), N);

    (vtable.drop_fn)(vtable.state);
}

/// A provider that panics before producing a result must surface as the
/// null-result error ("ended without a result"), not a hang or a success.
struct PanickingProvider;

#[async_trait]
impl BootstrapProvider for PanickingProvider {
    async fn bootstrap(
        &self,
        _request: BootstrapRequest,
        _context: &BootstrapContext,
        _event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> anyhow::Result<BootstrapResult> {
        panic!("provider panicked before producing a result");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_provider_yields_ended_without_result_error() {
    let vtable = build_bootstrap_provider_vtable(Box::new(PanickingProvider), noop_executor);
    let proxy = BootstrapProviderProxy::new(vtable, None);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<BootstrapEvent>(EVENT_CHANNEL_CAP);
    let drain = tokio::spawn(async move {
        let mut n = 0usize;
        while rx.recv().await.is_some() {
            n += 1;
        }
        n
    });

    // NOTE: the provider thread's panic message on stderr is expected here.
    let err = proxy
        .bootstrap(req(), &ctx(), tx, None)
        .await
        .expect_err("panicking provider must surface an error");
    assert!(
        err.to_string().contains("ended without a result"),
        "unexpected error: {err}"
    );
    assert_eq!(drain.await.unwrap(), 0, "no events before the panic");
}
