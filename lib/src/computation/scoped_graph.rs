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
    v1::{ComputationGraph, ComputationInspector, GraphControl, GraphError, GraphSelection},
};
use futures::future::BoxFuture;
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

/// A nested graph's future is owned independently of the current polling call.
/// Cancelling a service's `run()` borrow parks it; stop can still drive its
/// controller to quiescence and shutdown can await all of its actual cleanup.
pub(crate) struct ScopedGraph {
    graph: Arc<GraphSlot>,
    run: Option<BoxFuture<'static, anyhow::Result<()>>>,
    control: watch::Receiver<Option<GraphControl>>,
    inspector: ComputationInspector,
    terminal: Option<String>,
    disposed: bool,
}

impl ScopedGraph {
    pub(crate) fn new(mut graph: ComputationGraph, execution_scope: Arc<str>) -> Self {
        graph.set_execution_scope(execution_scope);
        let inspector = graph.inspector();
        let slot = Arc::new(GraphSlot(Mutex::new(Some(graph))));
        let (publish, control) = watch::channel(None);
        let owner = slot.clone();
        let run = Box::pin(async move {
            let mut graph = owner.take()?;
            let run = graph.run()?;
            publish.send_replace(Some(run.control()));
            run.await.map_err(Into::into)
        });
        Self {
            graph: slot,
            run: Some(run),
            control,
            inspector,
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
                self.run = None;
                self.terminal = Some(match &result { Ok(()) => "completed".into(), Err(error) => format!("{error:#}") });
                result?;
                anyhow::bail!("nested graph ended before the requested operation completed")
            }
        }
    }

    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        let result = self
            .run
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("nested graph has ended"))?
            .await;
        self.run = None;
        if let Err(error) = &result {
            self.terminal = Some(format!("{error:#}"));
        }
        result
    }

    pub(crate) async fn stop(&mut self) -> anyhow::Result<()> {
        if self.run.is_none() {
            let mut graph = self.graph.take()?;
            graph.shutdown().await?;
            return Ok(());
        }
        let control = self.published_control().await?;
        let revision = control.desired_snapshot().revision;
        let request = control.clone();
        let quiesced = self
            .drive(async move {
                Ok(request
                    .quiesce_components(revision, GraphSelection::All)
                    .await?)
            })
            .await;
        let report = self
            .drive(async move {
                Ok(control
                    .stop_components(revision, GraphSelection::All)
                    .await?)
            })
            .await?;
        quiesced?;
        if report.summary != super::v1::OperationSummary::Completed {
            anyhow::bail!("nested graph stop failed: {:?}", report.components);
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&mut self) -> anyhow::Result<()> {
        if self.disposed {
            return Ok(());
        }
        if self.control.borrow().is_none() {
            self.run = None;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computation::v1::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Service {
        descriptor: ComponentDescriptor,
        starts: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ComputationComponent for Service {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[async_trait]
    impl ComputationService for Service {
        async fn run(&mut self) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancelled_poll_borrow_keeps_a_nested_graph_alive_for_awaited_stop_and_restart() {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let graph = ComputationGraph::builder("nested")
            .service(Box::new(Service {
                descriptor: ComponentDescriptor::try_new(
                    ComponentId::try_new("service").unwrap(),
                    vec![],
                )
                .unwrap(),
                starts: starts.clone(),
                stops: stops.clone(),
            }))
            .build()
            .unwrap();
        let mut scope = ScopedGraph::new(graph, Arc::from("instance"));
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
        }
        scope.shutdown().await.unwrap();
        assert!(scope.run.is_none());
        assert!(scope.disposed);
        assert_eq!(stops.load(Ordering::SeqCst), 2);
    }
}
