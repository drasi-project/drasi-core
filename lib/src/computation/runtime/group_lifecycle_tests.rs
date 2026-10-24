// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{computation::v1::*, DrasiLib};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Notify;

struct Service {
    descriptor: ComponentDescriptor,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    running: Arc<AtomicUsize>,
    start_gate: Option<Arc<Notify>>,
    failure: Option<Arc<Notify>>,
    stop_gate: Option<Arc<Notify>>,
}

struct Running(Arc<AtomicUsize>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.start_gate {
            gate.notified().await;
        }
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.stop_gate {
            gate.notified().await;
        }
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Service {
    async fn run(&mut self) -> anyhow::Result<()> {
        self.running.fetch_add(1, Ordering::SeqCst);
        let _running = Running(self.running.clone());
        if let Some(failure) = &self.failure {
            failure.notified().await;
            anyhow::bail!("injected processing failure");
        }
        std::future::pending().await
    }
}

#[async_trait]
impl EnvelopeSource for Service {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        self.run().await?;
        Ok(None)
    }
}

#[async_trait]
impl EnvelopeSink for Service {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        anyhow::bail!("the failure fixture must not emit data")
    }
}

fn service() -> Service {
    Service {
        descriptor: ComponentDescriptor::try_new(ComponentId::try_new("service").unwrap(), vec![])
            .unwrap(),
        starts: Arc::new(AtomicUsize::new(0)),
        stops: Arc::new(AtomicUsize::new(0)),
        running: Arc::new(AtomicUsize::new(0)),
        start_gate: None,
        failure: None,
        stop_gate: None,
    }
}

async fn instance(component: Service) -> DrasiLib {
    DrasiLib::builder()
        .with_components(
            ComponentBatch::builder()
                .service(Box::new(component))
                .build()
                .unwrap()
                .auto_start(false),
        )
        .build()
        .await
        .unwrap()
}

fn group(core: &DrasiLib, ids: &[&str]) -> ComponentHandle {
    let control = core.computation_control().unwrap();
    let members: BTreeMap<_, _> = ids
        .iter()
        .map(|id| {
            let handle = core.computation_component(id).unwrap();
            (handle.id().clone(), handle.generation())
        })
        .collect();
    control
        .component_handle(&ComponentId::try_new(ids[0]).unwrap())
        .unwrap()
        .with_lifecycle_members(members)
        .unwrap()
}

async fn wait_count(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while counter.load(Ordering::SeqCst) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("root component did not reach the expected state");
}

#[tokio::test]
async fn root_group_progress_and_restart_do_not_require_an_api_waiter() {
    let component = service();
    let starts = component.starts.clone();
    let stops = component.stops.clone();
    let running = component.running.clone();
    let core = instance(component).await;
    for expected in 1..=2 {
        let handle = group(&core, &["service"]);
        handle.start().await.unwrap();
        drop(handle);
        wait_count(&running, 1).await;
        assert_eq!(starts.load(Ordering::SeqCst), expected);
        group(&core, &["service"]).stop().await.unwrap();
        assert_eq!(stops.load(Ordering::SeqCst), expected);
        assert_eq!(running.load(Ordering::SeqCst), 0);
    }
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn root_shutdown_before_component_activation_does_not_start_it() {
    let component = service();
    let starts = component.starts.clone();
    let stops = component.stops.clone();
    let core = instance(component).await;
    core.shutdown().await.unwrap();
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert_eq!(stops.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn root_quiescence_preserves_component_instances() {
    let component = service();
    let starts = component.starts.clone();
    let stops = component.stops.clone();
    let running = component.running.clone();
    let core = instance(component).await;
    let handle = group(&core, &["service"]);
    handle.start().await.unwrap();
    wait_count(&running, 1).await;
    let control = core.computation_control().unwrap();
    for _ in 0..2 {
        control
            .quiesce_components(
                control.desired_snapshot().revision,
                GraphSelection::Exact(vec![handle.id().clone()]),
            )
            .await
            .unwrap();
        wait_count(&running, 0).await;
        handle.start().await.unwrap();
        wait_count(&running, 1).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(stops.load(Ordering::SeqCst), 0);
    }
    core.shutdown().await.unwrap();
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_stop_waiter_retains_the_actual_cleanup_owner() {
    let mut component = service();
    let stops = component.stops.clone();
    let running = component.running.clone();
    let gate = Arc::new(Notify::new());
    component.stop_gate = Some(gate.clone());
    let core = instance(component).await;
    let handle = group(&core, &["service"]);
    handle.start().await.unwrap();
    wait_count(&running, 1).await;
    {
        let stop = handle.stop();
        tokio::pin!(stop);
        tokio::select! {
            result = &mut stop => panic!("cleanup passed its gate: {result:?}"),
            _ = wait_count(&stops, 1) => {}
        }
    }
    assert_eq!(
        handle.observed().unwrap().lifecycle,
        ComponentLifecycle::Stopping
    );
    gate.notify_one();
    handle.stop().await.unwrap();
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(running.load(Ordering::SeqCst), 0);
    core.shutdown().await.unwrap();
}

async fn stop_during_startup() {
    let mut component = service();
    let starts = component.starts.clone();
    let stops = component.stops.clone();
    let running = component.running.clone();
    component.start_gate = Some(Arc::new(Notify::new()));
    let core = instance(component).await;
    let handle = group(&core, &["service"]);
    let activation = tokio::spawn({
        let handle = handle.clone();
        async move { handle.start().await }
    });
    wait_count(&starts, 1).await;
    tokio::time::timeout(Duration::from_secs(2), handle.stop())
        .await
        .expect("stop must cancel unfinished startup")
        .unwrap();
    assert!(activation.await.unwrap().is_err());
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(running.load(Ordering::SeqCst), 0);
    core.shutdown().await.unwrap();
}

async fn stop_during_failure_cleanup() {
    let mut source = service();
    let mut sink = service();
    let running = source.running.clone();
    let stops = sink.stops.clone();
    let failure = Arc::new(Notify::new());
    let stop_gate = Arc::new(Notify::new());
    let schema = SchemaDescriptor::try_new(
        SchemaId::try_new("test.failure").unwrap(),
        SchemaVersion::try_new(1).unwrap(),
        "test",
        bytes::Bytes::from_static(b"failure-test"),
    )
    .unwrap();
    let from = Endpoint::new(
        ComponentId::try_new("source").unwrap(),
        PortId::try_new("out").unwrap(),
    );
    let to = Endpoint::new(
        ComponentId::try_new("sink").unwrap(),
        PortId::try_new("in").unwrap(),
    );
    source.descriptor = ComponentDescriptor::try_new(
        from.component.clone(),
        vec![PortDescriptor::new(
            from.port.clone(),
            PortDirection::Output,
            schema.clone(),
            PipeRequirements::default(),
        )],
    )
    .unwrap();
    sink.descriptor = ComponentDescriptor::try_new(
        to.component.clone(),
        vec![PortDescriptor::new(
            to.port.clone(),
            PortDirection::Input,
            schema,
            PipeRequirements::default(),
        )],
    )
    .unwrap();
    source.failure = Some(failure.clone());
    sink.stop_gate = Some(stop_gate.clone());
    let edge = EdgeDefinition::new(from.clone(), to);
    let batch = ComponentBatch::builder()
        .source(Box::new(source))
        .sink(Box::new(sink))
        .bind_stream(from, StreamId::try_new("source/out").unwrap())
        .connect(edge.clone(), Box::new(BoundedPipeConfig { capacity: 1 }))
        .relationship_policy(
            edge,
            RelationshipPolicy {
                propagate_failure: true,
                ..Default::default()
            },
        )
        .build()
        .unwrap()
        .auto_start(false);
    let core = DrasiLib::builder()
        .with_components(batch)
        .build()
        .await
        .unwrap();
    let handle = group(&core, &["source", "sink"]);
    handle.start().await.unwrap();
    wait_count(&running, 1).await;
    failure.notify_one();
    wait_count(&stops, 1).await;
    {
        let stop = handle.stop();
        tokio::pin!(stop);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut stop)
                .await
                .is_err(),
            "stop must wait for the existing cleanup owner"
        );
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        stop_gate.notify_one();
        tokio::time::timeout(Duration::from_secs(2), stop)
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(running.load(Ordering::SeqCst), 0);
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_during_startup_current_thread() {
    stop_during_startup().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_startup_multi_thread() {
    stop_during_startup().await;
}
#[tokio::test]
async fn stop_during_failure_cleanup_current_thread() {
    stop_during_failure_cleanup().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_failure_cleanup_multi_thread() {
    stop_during_failure_cleanup().await;
}
