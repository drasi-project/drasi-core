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

#[allow(dead_code)]
mod computation_support;

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::{computation::v1::*, DrasiError, DrasiLib};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};

#[derive(Default)]
struct Calls {
    starts: AtomicUsize,
    stops: AtomicUsize,
    fail_stop: AtomicBool,
    stop_entered: Notify,
    stop_release: Notify,
    hold_stop: AtomicBool,
    disposals: AtomicUsize,
    fail_dispose: AtomicBool,
}

#[derive(Debug, thiserror::Error)]
#[error("resource cleanup needs retry")]
struct ResourceCleanupFailure;

#[derive(Debug, thiserror::Error)]
#[error("incomplete native cleanup")]
struct NativeCleanupFailure;

#[async_trait]
impl ResourceCleanup for Calls {
    async fn shutdown(&self) -> anyhow::Result<()> {
        tokio::task::yield_now().await;
        self.disposals.fetch_add(1, Ordering::SeqCst);
        if self.fail_dispose.swap(false, Ordering::AcqRel) {
            return Err(ResourceCleanupFailure.into());
        }
        Ok(())
    }
}

struct Source {
    descriptor: ComponentDescriptor,
    input: mpsc::Receiver<OutputEnvelope>,
    calls: Arc<Calls>,
}
#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.calls.starts.fetch_add(1, Ordering::SeqCst);
        tracing::info!("native source started");
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.stops.fetch_add(1, Ordering::SeqCst);
        self.calls.stop_entered.notify_one();
        if self.calls.hold_stop.load(Ordering::Acquire) {
            self.calls.stop_release.notified().await;
        }
        if self.calls.fail_stop.swap(false, Ordering::AcqRel) {
            return Err(NativeCleanupFailure.into());
        }
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.input.recv().await)
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
fn components(
    calls: Arc<Calls>,
) -> (
    ComponentBatch,
    mpsc::Sender<OutputEnvelope>,
    mpsc::Receiver<ChangeEnvelope>,
) {
    let (input, receiver) = mpsc::channel(8);
    let (sender, output) = mpsc::channel(8);
    let graph = ComponentBatch::builder()
        .declare_resource(ResourceSpecification {
            id: ResourceId::try_new("cleanup").expect("resource"),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: Arc::from("cleanup"),
        })
        .expect("declaration")
        .provide_resource(
            ResourceId::try_new("cleanup").expect("resource"),
            ResourceHandle::new(ResourceRole::StateStore, calls.clone())
                .with_cleanup(calls.clone()),
        )
        .expect("resource")
        .source(Box::new(Source {
            descriptor: descriptor("native-source", &[], &["out"]),
            input: receiver,
            calls,
        }))
        .sink(Box::new(Sink {
            descriptor: descriptor("native-sink", &["in"], &[]),
            output: sender,
        }))
        .bind_stream(endpoint("native-source", "out"), stream("native-source"))
        .connect(
            edge("native-source", "native-sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    (graph, input, output)
}
async fn roundtrip(
    input: &mpsc::Sender<OutputEnvelope>,
    output_rx: &mut mpsc::Receiver<ChangeEnvelope>,
    sequence: u64,
) {
    input
        .send(output(root("native-source", sequence, &[1])))
        .await
        .expect("input");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), output_rx.recv())
            .await
            .expect("driver stalled")
            .expect("output")
            .system()
            .sequence(),
        sequence
    );
}

async fn instance_lifecycle() {
    let calls = Arc::new(Calls::default());
    let (components, input, mut output_rx) = components(calls.clone());
    let drasi = DrasiLib::builder()
        .with_id("managed-instance")
        .with_components(components)
        .build()
        .await
        .expect("instance");
    let handle = drasi.computation_control().expect("instance controller");
    let (_, mut logs) = drasi
        .subscribe_computation_logs(drasi_lib::channels::ComponentType::Source, "native-source")
        .await
        .expect("scoped logs");
    drasi
        .computation_component("native-source")
        .unwrap()
        .wait_created()
        .await
        .unwrap();
    assert_eq!(calls.starts.load(Ordering::SeqCst), 0);
    assert!(handle
        .desired_snapshot()
        .nodes
        .iter()
        .any(|node| node.descriptor.id().as_str() == "native-source"));
    drasi.start().await.expect("start instance");
    let message = tokio::time::timeout(Duration::from_secs(5), logs.recv())
        .await
        .expect("missing native log")
        .expect("log");
    assert_eq!(message.instance_id, "managed-instance");
    roundtrip(&input, &mut output_rx, 1).await;
    let mut instance_events = drasi.subscribe_all_component_events();
    drasi.stop().await.expect("soft stop");
    loop {
        if drasi
            .get_graph()
            .await
            .nodes
            .iter()
            .all(|node| node.status != drasi_lib::ComponentStatus::Stopping)
        {
            break;
        }
        instance_events
            .recv()
            .await
            .expect("instance stop observations");
    }
    assert_eq!(calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        handle.observed().components[&component("native-source")].lifecycle,
        ComponentLifecycle::Stopped
    );
    drasi.start().await.expect("restart instance");
    roundtrip(&input, &mut output_rx, 2).await;
    assert_eq!(calls.starts.load(Ordering::SeqCst), 2);
    drasi.shutdown().await.expect("await all drivers");
    assert_eq!(calls.disposals.load(Ordering::SeqCst), 1);
    assert!(handle
        .start_requested(handle.desired_snapshot().revision, GraphSelection::All)
        .await
        .is_err());
    assert!(drasi.start().await.is_err());
}
#[tokio::test(flavor = "current_thread")]
async fn managed_driver_runs_and_restarts_on_current_thread() {
    tokio::time::timeout(Duration::from_secs(15), instance_lifecycle())
        .await
        .expect("deadlock");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_driver_runs_and_restarts_on_multiple_threads() {
    tokio::time::timeout(Duration::from_secs(15), instance_lifecycle())
        .await
        .expect("deadlock");
}

#[tokio::test]
async fn instance_stop_preserves_typed_member_failure_and_allows_cleanup_retry() {
    let calls = Arc::new(Calls::default());
    let (batch, _input, _output) = components(calls.clone());
    let drasi = DrasiLib::builder()
        .with_components(batch)
        .build()
        .await
        .unwrap();
    drasi.start().await.unwrap();
    calls.fail_stop.store(true, Ordering::Release);
    let error = tokio::time::timeout(Duration::from_secs(5), drasi.stop())
        .await
        .expect("stop must report its failed member")
        .unwrap_err();
    assert!(
        error.downcast_ref::<NativeCleanupFailure>().is_some(),
        "{error:?}"
    );
    let report = error
        .downcast_ref::<LifecycleReportError>()
        .expect("typed stop report");
    let LifecycleReportError::Stop(report) = report else {
        panic!("expected stop report");
    };
    assert!(matches!(
        report.components[&component("native-source")],
        StopOutcome::StopFailed(_)
    ));
    tokio::time::timeout(Duration::from_secs(5), drasi.shutdown())
        .await
        .expect("cleanup retry must finish")
        .unwrap();
    assert_eq!(calls.disposals.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn component_batches_share_identity_and_preserve_existing_instance_members() {
    let drasi = DrasiLib::builder().build().await.expect("instance");
    let control = drasi.computation_control().unwrap();
    let original: Vec<_> = control
        .desired_snapshot()
        .nodes
        .iter()
        .map(|node| node.descriptor.id().clone())
        .collect();
    let calls = Arc::new(Calls::default());
    let (batch, input, mut output_rx) = components(calls.clone());
    drasi
        .add_components(batch.auto_start(false))
        .await
        .expect("register");
    drasi.start().await.expect("instance");
    assert_eq!(calls.starts.load(Ordering::SeqCst), 0);
    let (duplicate, _, _) = components(Arc::new(Calls::default()));
    assert!(matches!(
        drasi.add_components(duplicate).await,
        Err(DrasiError::AlreadyExists { .. })
    ));
    control
        .start_requested(
            control.desired_snapshot().revision,
            GraphSelection::Exact(vec![component("native-source"), component("native-sink")]),
        )
        .await
        .expect("manual native activation");
    roundtrip(&input, &mut output_rx, 1).await;
    let preview = control
        .preview(
            control.desired_snapshot().revision,
            vec![
                DesiredMutation::RemoveComponents {
                    selection: GraphSelection::Exact(vec![
                        component("native-source"),
                        component("native-sink"),
                    ]),
                    policy: RemovalPolicy::Reject,
                },
                DesiredMutation::RemoveResource {
                    resource: ResourceId::try_new("cleanup").unwrap(),
                    policy: RemovalPolicy::Reject,
                },
            ],
        )
        .await
        .unwrap();
    control
        .reconcile(preview, TopologyBindings::default())
        .await
        .unwrap();
    assert!(drasi.is_running().await);
    assert!(drasi.computation_component("native-source").is_err());
    assert_eq!(
        control
            .desired_snapshot()
            .nodes
            .iter()
            .map(|node| node.descriptor.id().clone())
            .collect::<Vec<_>>(),
        original
    );
    drasi.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn cancelled_shutdown_retains_the_driver_and_actual_async_cleanup_owner() {
    let drasi = DrasiLib::builder().build().await.expect("instance");
    let calls = Arc::new(Calls::default());
    calls.hold_stop.store(true, Ordering::Release);
    let (batch, _input, _) = components(calls.clone());
    drasi
        .add_components(batch.auto_start(false))
        .await
        .expect("register");
    let handle = drasi.computation_control().unwrap();
    handle
        .start_requested(
            handle.desired_snapshot().revision,
            GraphSelection::Exact(vec![component("native-source"), component("native-sink")]),
        )
        .await
        .expect("start");
    let mut shutdown = Box::pin(drasi.shutdown());
    tokio::select! {
        result = &mut shutdown => panic!("cleanup was not awaited: {result:?}"),
        _ = calls.stop_entered.notified() => {}
    }
    drop(shutdown);
    calls.hold_stop.store(false, Ordering::Release);
    calls.stop_release.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), drasi.shutdown())
        .await
        .expect("cleanup deadlock")
        .expect("retry shutdown");
    assert_eq!(calls.stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn builder_rejection_awaits_all_owned_resources_without_starting_components() {
    let first = Arc::new(Calls::default());
    let second = Arc::new(Calls::default());
    let (one, _, _) = components(first.clone());
    let (two, _, _) = components(second.clone());
    let result = DrasiLib::builder()
        .with_components(one)
        .with_components(two)
        .build()
        .await;
    assert!(matches!(result, Err(DrasiError::AlreadyExists { .. })));
    for calls in [first, second] {
        assert_eq!(calls.starts.load(Ordering::SeqCst), 0);
        assert_eq!(calls.disposals.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn query_creation_failure_retains_component_resources_until_explicit_cleanup() {
    let calls = Arc::new(Calls::default());
    let (batch, _, _) = components(calls.clone());
    let mut query = drasi_lib::Query::cypher("invalid")
        .query("MATCH (n) RETURN n")
        .build();
    query.storage_backend = Some(drasi_lib::indexes::StorageBackendRef::Named(
        "missing".into(),
    ));
    let drasi = DrasiLib::builder()
        .with_query(query)
        .with_components(batch)
        .build()
        .await
        .expect("node-first builder retains failed query declarations");
    let query = drasi
        .computation_component("invalid")
        .expect("query handle");
    let failure = query
        .wait_created()
        .await
        .expect_err("missing backend must fail creation");
    assert!(failure.to_string().contains("missing"));
    assert_eq!(calls.starts.load(Ordering::SeqCst), 0);
    assert_eq!(calls.disposals.load(Ordering::SeqCst), 0);
    drasi
        .shutdown()
        .await
        .expect("dispose transferred resources");
    assert_eq!(calls.disposals.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_builder_rollback_returns_the_actual_retryable_cleanup_owner() {
    let first = Arc::new(Calls::default());
    let second = Arc::new(Calls::default());
    first.fail_dispose.store(true, Ordering::Release);
    let (one, _, _) = components(first.clone());
    let (two, _, _) = components(second.clone());
    let error = match DrasiLib::builder()
        .with_components(one)
        .with_components(two)
        .build()
        .await
    {
        Ok(_) => panic!("duplicate build succeeded"),
        Err(error) => error,
    };
    let DrasiError::Internal(cause) = &error else {
        panic!("missing cleanup owner: {error}");
    };
    let owner = cause
        .downcast_ref::<ComputationCleanupError>()
        .expect("typed cleanup ownership");
    assert!(matches!(owner.cause(), DrasiError::AlreadyExists { .. }));
    assert!(owner
        .cleanup_error()
        .downcast_ref::<ResourceCleanupFailure>()
        .is_some());
    assert_eq!(first.disposals.load(Ordering::SeqCst), 1);
    assert_eq!(second.disposals.load(Ordering::SeqCst), 1);
    owner.cleanup().await.expect("retry actual owned resource");
    owner.cleanup().await.expect("idempotent completed cleanup");
    assert_eq!(first.disposals.load(Ordering::SeqCst), 2);
    assert_eq!(second.disposals.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rejected_registration_keeps_failed_cleanup_separate_from_the_live_graph() {
    let drasi = DrasiLib::builder().build().await.expect("instance");
    let live = Arc::new(Calls::default());
    let rejected = Arc::new(Calls::default());
    rejected.fail_dispose.store(true, Ordering::Release);
    let (original, _, _) = components(live.clone());
    drasi.add_components(original).await.expect("original");
    let (duplicate, _, _) = components(rejected.clone());
    let error = match drasi.add_components(duplicate).await {
        Ok(_) => panic!("duplicate registration succeeded"),
        Err(error) => error,
    };
    let DrasiError::Internal(cause) = &error else {
        panic!("missing rejection owner: {error}");
    };
    cause
        .downcast_ref::<ComputationCleanupError>()
        .expect("cleanup owner")
        .cleanup()
        .await
        .expect("retry");
    assert_eq!(rejected.disposals.load(Ordering::SeqCst), 2);
    assert_eq!(live.disposals.load(Ordering::SeqCst), 0);
    let control = drasi.computation_control().unwrap();
    control
        .start_requested(
            control.desired_snapshot().revision,
            GraphSelection::Exact(vec![component("native-source"), component("native-sink")]),
        )
        .await
        .expect("original starts");
    drasi.shutdown().await.expect("shutdown");
    assert_eq!(live.disposals.load(Ordering::SeqCst), 1);
}
