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

use super::{
    instance::GraphSlot,
    v1::{
        ComponentGeneration, ComponentId, ComponentLifecycle, ComputationGraph,
        ComputationInspector, GraphControl, GraphError, GraphSelection,
    },
};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::{sync::watch, task::JoinHandle};
use tracing::Instrument;

/// Owns a nested graph's driver independently of the parent's polling task.
/// Cancelling a `run()` borrow keeps the driver owned; stop quiesces its work,
/// and shutdown joins the driver before disposing its graph.
pub(crate) struct ScopedGraph {
    graph: Arc<GraphSlot>,
    run: Option<JoinHandle<anyhow::Result<()>>>,
    cancel: watch::Sender<bool>,
    control: watch::Receiver<Option<GraphControl>>,
    inspector: ComputationInspector,
    paused: BTreeMap<ComponentId, ComponentGeneration>,
    terminal: Option<String>,
    disposed: bool,
}

impl ScopedGraph {
    pub(crate) fn new(mut graph: ComputationGraph, execution_scope: Arc<str>) -> Self {
        graph.set_execution_scope(execution_scope);
        let inspector = graph.inspector();
        let slot = Arc::new(GraphSlot(Mutex::new(Some(graph))));
        let (publish, control) = watch::channel(None);
        let (cancel, mut cancellation) = watch::channel(false);
        let owner = slot.clone();
        let run = tokio::spawn(
            async move {
                let mut graph = owner.take()?;
                let run = graph.run()?;
                let control = run.control();
                publish.send_replace(Some(control.clone()));
                tokio::pin!(run);
                tokio::select! {
                    biased;
                    _ = async {
                        let _ = cancellation.wait_for(|cancelled| *cancelled).await;
                    } => {
                        control.cancel();
                        run.await.map_err(Into::into)
                    }
                    result = &mut run => result.map_err(Into::into),
                }
            }
            .instrument(tracing::Span::current()),
        );
        Self {
            graph: slot,
            run: Some(run),
            cancel,
            control,
            inspector,
            paused: BTreeMap::new(),
            terminal: None,
            disposed: false,
        }
    }

    pub(crate) fn inspector(&self) -> ComputationInspector {
        self.inspector.clone()
    }

    pub(crate) fn control(&self) -> anyhow::Result<GraphControl> {
        self.control
            .borrow()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("nested graph is not deployed"))
    }

    async fn published_control(&mut self) -> anyhow::Result<GraphControl> {
        if self.control.borrow().is_none() {
            let mut published = self.control.clone();
            self.drive(async move {
                published.wait_for(Option::is_some).await?;
                Ok(())
            })
            .await?;
        }
        self.control()
    }

    pub(crate) async fn ready(&mut self) -> anyhow::Result<GraphControl> {
        let control = self.published_control().await?;
        let request = control.clone();
        let report = self
            .drive(async move { Ok(request.deployment_report().await?) })
            .await?;
        if report.summary != super::v1::OperationSummary::Completed {
            anyhow::bail!("nested graph deployment failed: {:?}", report.components);
        }
        Ok(control)
    }

    pub(crate) async fn drive<T>(
        &mut self,
        operation: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let run = self.run.as_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "nested graph has ended: {}",
                self.terminal.as_deref().unwrap_or("completed")
            )
        })?;
        tokio::pin!(operation);
        tokio::select! {
            biased;
            result = &mut operation => result,
            result = run => {
                let result = driver_result(result);
                self.run = None;
                self.terminal = Some(match &result { Ok(()) => "completed".into(), Err(error) => format!("{error:#}") });
                result?;
                anyhow::bail!("nested graph ended before the requested operation completed")
            }
        }
    }

    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        let result = driver_result(
            self.run
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("nested graph has ended"))?
                .await,
        );
        self.run = None;
        if let Err(error) = &result {
            self.terminal = Some(format!("{error:#}"));
        }
        result
    }

    pub(crate) async fn quiesce(&mut self) -> anyhow::Result<()> {
        let control = self.published_control().await?;
        let running: BTreeMap<_, _> = control
            .observed()
            .components
            .iter()
            .filter(|(_, node)| {
                matches!(
                    node.lifecycle,
                    ComponentLifecycle::Running | ComponentLifecycle::Starting
                )
            })
            .map(|(id, node)| (id.clone(), node.generation))
            .collect();
        let revision = control.desired_snapshot().revision;
        self.drive(async move {
            control
                .quiesce_components(revision, GraphSelection::All)
                .await?;
            Ok(())
        })
        .await?;
        self.paused.extend(running);
        Ok(())
    }

    pub(crate) async fn resume(&mut self) -> anyhow::Result<()> {
        if self.paused.is_empty() {
            return Ok(());
        }
        let control = self.control()?;
        let observed = control.observed();
        let selected: Vec<_> = self
            .paused
            .iter()
            .filter(|(id, generation)| {
                observed.components.get(*id).is_some_and(|node| {
                    node.generation == **generation
                        && node.lifecycle == ComponentLifecycle::Quiesced
                })
            })
            .map(|(id, _)| id.clone())
            .collect();
        if !selected.is_empty() {
            let revision = control.desired_snapshot().revision;
            let report = self
                .drive(async move {
                    Ok(control
                        .start_requested(revision, GraphSelection::Exact(selected))
                        .await?)
                })
                .await?;
            anyhow::ensure!(
                report.summary == super::v1::OperationSummary::Completed,
                "nested graph resume failed: {:?}",
                report.components
            );
        }
        self.paused.clear();
        Ok(())
    }

    pub(crate) async fn stop(&mut self) -> anyhow::Result<()> {
        if self.run.is_none() {
            let mut graph = self.graph.take()?;
            graph.shutdown().await?;
            return Ok(());
        }
        let control = self.published_control().await?;
        let members = control
            .observed()
            .components
            .iter()
            .map(|(id, node)| (id.clone(), node.generation))
            .collect();
        let report = self
            .drive(async move { Ok(control.stop_members(members).await?) })
            .await?;
        if report.summary != super::v1::OperationSummary::Completed {
            anyhow::bail!("nested graph stop failed: {:?}", report.components);
        }
        self.paused.clear();
        Ok(())
    }

    /// Test-controlled queries must be cancellable even with a deliberately
    /// blocked output consumer. Use the graph's existing abort-and-await stop,
    /// rather than its graceful edge-drain boundary.
    #[cfg(feature = "test-support")]
    pub(crate) async fn stop_test_query(&mut self) -> anyhow::Result<()> {
        if self.run.is_none() {
            return self.stop().await;
        }
        let control = self.published_control().await?;
        let revision = control.desired_snapshot().revision;
        let report = self
            .drive(async move {
                Ok(control
                    .stop_components(revision, GraphSelection::All)
                    .await?)
            })
            .await?;
        anyhow::ensure!(
            report.summary == super::v1::OperationSummary::Completed,
            "controlled query stop failed: {:?}",
            report.components
        );
        self.paused.clear();
        Ok(())
    }

    pub(crate) async fn shutdown(&mut self) -> anyhow::Result<()> {
        if self.disposed {
            return Ok(());
        }
        self.cancel.send_replace(true);
        if let Some(control) = self.control.borrow().as_ref() {
            control.cancel();
        }
        let primary = if self.run.is_some() {
            match self.run().await {
                Ok(()) => None,
                Err(error)
                    if matches!(
                        error.downcast_ref::<GraphError>(),
                        Some(GraphError::Cancelled)
                    ) =>
                {
                    None
                }
                Err(error) => Some(error),
            }
        } else {
            None
        };
        let mut graph = self.graph.take()?;
        graph.dispose().await?;
        graph.graph.take();
        self.disposed = true;
        if let Some(error) = primary {
            return Err(error);
        }
        Ok(())
    }
}

fn driver_result(result: Result<anyhow::Result<()>, tokio::task::JoinError>) -> anyhow::Result<()> {
    result.map_err(|error| anyhow::anyhow!("nested graph driver failed: {error}"))?
}

impl Drop for ScopedGraph {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        if let Some(run) = &self.run {
            run.abort();
            if !self.disposed {
                log::warn!("Nested computation graph dropped without awaited shutdown; asynchronous plugin cleanup is not guaranteed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computation::v1::*;
    use async_trait::async_trait;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    struct Service {
        descriptor: ComponentDescriptor,
        starts: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
        running: Arc<AtomicUsize>,
        start_gate: Option<Arc<tokio::sync::Notify>>,
        fail_run: Option<Arc<tokio::sync::Notify>>,
        stop_gate: Option<Arc<tokio::sync::Notify>>,
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
            if let Some(gate) = &self.fail_run {
                gate.notified().await;
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

    fn scope(
        starts: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
        running: Arc<AtomicUsize>,
    ) -> ScopedGraph {
        let graph = ComputationGraph::builder("nested")
            .service(Box::new(Service {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("service").unwrap(),
                    vec![],
                )
                .unwrap(),
                starts,
                stops,
                running,
                start_gate: None,
                fail_run: None,
                stop_gate: None,
            }))
            .build()
            .unwrap();
        ScopedGraph::new(graph, Arc::from("instance"))
    }

    async fn wait_running(running: &AtomicUsize, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while running.load(Ordering::SeqCst) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("driver did not reach the expected processing state");
    }

    #[tokio::test]
    async fn cancelled_poll_borrow_keeps_a_nested_graph_alive_for_awaited_stop_and_restart() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicUsize::new(0));
        let mut scope = scope(starts.clone(), stops.clone(), running.clone());
        let control = scope.ready().await.unwrap();
        for expected in 1..=2 {
            let request = control.clone();
            scope
                .drive(async move {
                    Ok(request
                        .start_components(GraphRevision(1), GraphSelection::All)
                        .await?)
                })
                .await
                .unwrap();
            assert_eq!(starts.load(Ordering::SeqCst), expected);
            {
                let polling = scope.run();
                tokio::pin!(polling);
                tokio::select! {
                    result = &mut polling => panic!("service unexpectedly ended: {result:?}"),
                    _ = tokio::task::yield_now() => {}
                }
            }
            scope.stop().await.unwrap();
            assert_eq!(stops.load(Ordering::SeqCst), expected);
            assert_eq!(running.load(Ordering::SeqCst), 0);
        }
        scope.shutdown().await.unwrap();
        assert!(scope.run.is_none());
        assert!(scope.disposed);
        assert_eq!(stops.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn driver_progress_does_not_require_polling_the_parent_service() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicUsize::new(0));
        let mut scope = scope(starts, stops.clone(), running.clone());
        let control = scope.ready().await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            control.start_components(GraphRevision(1), GraphSelection::All),
        )
        .await
        .expect("nested controller still depends on the parent polling it")
        .unwrap();
        wait_running(&running, 1).await;
        scope.shutdown().await.unwrap();
        assert_eq!(running.load(Ordering::SeqCst), 0);
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shutdown_before_first_driver_poll_joins_before_disposal() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let mut scope = scope(starts.clone(), stops.clone(), Arc::new(AtomicUsize::new(0)));
        assert!(scope.control.borrow().is_none());
        scope.shutdown().await.unwrap();
        assert!(scope.run.is_none());
        assert!(scope.disposed);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(stops.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dropping_the_scope_aborts_its_owned_driver() {
        let running = Arc::new(AtomicUsize::new(0));
        let mut scope = scope(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            running.clone(),
        );
        let control = scope.ready().await.unwrap();
        control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap();
        wait_running(&running, 1).await;
        drop(scope);
        wait_running(&running, 0).await;
    }

    #[tokio::test]
    async fn quiescence_awaits_nested_work_and_resume_does_not_restart_instances() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicUsize::new(0));
        let mut scope = scope(starts.clone(), stops.clone(), running.clone());
        let control = scope.ready().await.unwrap();
        control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap();
        wait_running(&running, 1).await;
        for _ in 0..2 {
            scope.quiesce().await.unwrap();
            assert_eq!(running.load(Ordering::SeqCst), 0);
            scope.quiesce().await.unwrap();
            scope.resume().await.unwrap();
            wait_running(&running, 1).await;
            assert_eq!(starts.load(Ordering::SeqCst), 1);
            assert_eq!(stops.load(Ordering::SeqCst), 0);
        }
        scope.shutdown().await.unwrap();
        assert_eq!(running.load(Ordering::SeqCst), 0);
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelling_shutdown_keeps_the_driver_owned_until_cleanup_finishes() {
        let stops = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicUsize::new(0));
        let stop_gate = Arc::new(tokio::sync::Notify::new());
        let graph = ComputationGraph::builder("nested")
            .service(Box::new(Service {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("service").unwrap(),
                    vec![],
                )
                .unwrap(),
                starts: Arc::new(AtomicUsize::new(0)),
                stops: stops.clone(),
                running: running.clone(),
                start_gate: None,
                fail_run: None,
                stop_gate: Some(stop_gate.clone()),
            }))
            .build()
            .unwrap();
        let mut scope = ScopedGraph::new(graph, Arc::from("instance"));
        let control = scope.ready().await.unwrap();
        control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap();
        wait_running(&running, 1).await;
        {
            let shutdown = scope.shutdown();
            tokio::pin!(shutdown);
            tokio::select! {
                result = &mut shutdown => panic!("cleanup passed its gate: {result:?}"),
                _ = wait_running(&stops, 1) => {}
            }
        }
        assert!(scope.run.is_some());
        assert!(!scope.disposed);
        stop_gate.notify_one();
        scope.shutdown().await.unwrap();
        assert!(scope.run.is_none());
        assert!(scope.disposed);
        assert_eq!(running.load(Ordering::SeqCst), 0);
        assert_eq!(stops.load(Ordering::SeqCst), 1);
    }

    async fn stop_during_startup() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicUsize::new(0));
        let graph = ComputationGraph::builder("nested")
            .service(Box::new(Service {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("service").unwrap(),
                    vec![],
                )
                .unwrap(),
                starts: starts.clone(),
                stops: stops.clone(),
                running: running.clone(),
                start_gate: Some(Arc::new(tokio::sync::Notify::new())),
                fail_run: None,
                stop_gate: None,
            }))
            .build()
            .unwrap();
        let mut scope = ScopedGraph::new(graph, Arc::from("instance"));
        let control = scope.ready().await.unwrap();
        let activation = tokio::spawn(async move {
            control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await
        });
        wait_running(&starts, 1).await;
        tokio::time::timeout(Duration::from_secs(2), scope.stop())
            .await
            .expect("stop must cancel unfinished startup")
            .unwrap();
        let report = activation.await.unwrap().unwrap();
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        assert!(matches!(
            report.components[&ComponentId::try_new("service").unwrap()],
            StartOutcome::StartFailed(_)
        ));
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert_eq!(running.load(Ordering::SeqCst), 0);
        scope.shutdown().await.unwrap();
    }

    async fn stop_during_failure_cleanup() {
        let stops = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicUsize::new(0));
        let failure = Arc::new(tokio::sync::Notify::new());
        let stop_gate = Arc::new(tokio::sync::Notify::new());
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
        let edge = EdgeDefinition::new(from.clone(), to.clone());
        let graph = ComputationGraph::builder("nested")
            .source(Box::new(Service {
                descriptor: ComponentDescriptor::try_new(
                    from.component.clone(),
                    vec![PortDescriptor::new(
                        from.port.clone(),
                        PortDirection::Output,
                        schema.clone(),
                        PipeRequirements::default(),
                    )],
                )
                .unwrap(),
                starts: Arc::new(AtomicUsize::new(0)),
                stops: Arc::new(AtomicUsize::new(0)),
                running: running.clone(),
                start_gate: None,
                fail_run: Some(failure.clone()),
                stop_gate: None,
            }))
            .sink(Box::new(Service {
                descriptor: ComponentDescriptor::try_new(
                    to.component.clone(),
                    vec![PortDescriptor::new(
                        to.port,
                        PortDirection::Input,
                        schema,
                        PipeRequirements::default(),
                    )],
                )
                .unwrap(),
                starts: Arc::new(AtomicUsize::new(0)),
                stops: stops.clone(),
                running: Arc::new(AtomicUsize::new(0)),
                start_gate: None,
                fail_run: None,
                stop_gate: Some(stop_gate.clone()),
            }))
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
            .unwrap();
        let mut scope = ScopedGraph::new(graph, Arc::from("instance"));
        let control = scope.ready().await.unwrap();
        control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap();
        wait_running(&running, 1).await;
        failure.notify_one();
        wait_running(&stops, 1).await;
        {
            let stop = scope.stop();
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
                .expect("stop must finish after the existing cleanup completes")
                .unwrap();
        }
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert_eq!(running.load(Ordering::SeqCst), 0);
        scope.shutdown().await.unwrap();
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
}
