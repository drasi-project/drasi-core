// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::Duration,
};

use futures::{
    future::{AbortHandle, Abortable, BoxFuture},
    stream::FuturesUnordered,
    task::AtomicWaker,
    FutureExt, StreamExt,
};

use super::{
    controller::{failure, update},
    execution::{ResourceIdentity, Tasks},
    topology, ComputationGraph, GraphError, GraphResult, ResourceHandle, ResourceId,
};
use crate::computation::v1::{
    FailurePhase, OperationEpoch, ResourceConstructor, ResourceRealization, ResourceRole,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ResourceOperation {
    Construct,
    Shutdown,
}

type Outcome = Result<(), Arc<GraphError>>;

#[derive(Default)]
struct Value {
    handle: Option<ResourceHandle>,
    outcome: Option<Outcome>,
}

/// A pending callback's result and actual resource stay owned independently of
/// the controller or any caller waiting for its join.
pub(super) struct ResourceJob {
    pub identity: ResourceIdentity,
    operation: ResourceOperation,
    value: Arc<Mutex<Value>>,
    join: Mutex<Option<BoxFuture<'static, GraphResult<()>>>>,
    abort: AbortHandle,
    applied: AtomicBool,
}

impl ResourceJob {
    fn poll(&self, cx: &mut Context<'_>) -> Poll<GraphResult<()>> {
        let mut join = self.join.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(work) = join.as_mut() {
            let result = match work.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => result,
            };
            *join = None;
            if let Err(error) = result {
                self.value
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .outcome = Some(Err(Arc::new(error)));
            }
        }
        let mut value = self.value.lock().unwrap_or_else(|error| error.into_inner());
        let result = value.outcome.get_or_insert_with(|| {
            Err(Arc::new(self.identity.error(anyhow::anyhow!(
                "resource callback exited without publishing its result"
            ))))
        });
        Poll::Ready(
            result
                .clone()
                .map_err(|cause| GraphError::Reported { cause }),
        )
    }

    pub async fn wait(&self) -> GraphResult<()> {
        futures::future::poll_fn(|cx| self.poll(cx)).await
    }

    pub fn joined(&self) -> bool {
        self.join
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none()
    }
}

/// Temporary operation records only. Resource membership and publication remain
/// in the graph's existing resource maps and observed registry.
#[derive(Default)]
pub(super) struct ResourceTasks {
    tasks: Arc<Tasks<()>>,
    jobs: Mutex<BTreeMap<ResourceId, Arc<ResourceJob>>>,
    next: AtomicU64,
    changed: AtomicWaker,
}

impl ResourceTasks {
    pub fn contains(&self, id: &ResourceId) -> bool {
        self.jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(id)
    }

    pub fn is_empty(&self) -> bool {
        self.jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    }

    fn get(&self, id: &ResourceId) -> Option<Arc<ResourceJob>> {
        self.jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .cloned()
    }

    fn epoch(&self) -> GraphResult<OperationEpoch> {
        self.next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                epoch.checked_add(1)
            })
            .map(OperationEpoch)
            .map_err(|_| topology("resource operation epoch exhausted"))
    }

    fn start<F, W>(
        &self,
        identity: ResourceIdentity,
        operation: ResourceOperation,
        handle: Option<ResourceHandle>,
        work: W,
    ) -> Arc<ResourceJob>
    where
        F: Future<Output = GraphResult<()>> + Send + 'static,
        W: FnOnce(Arc<Mutex<Value>>) -> F + Send + 'static,
    {
        let value = Arc::new(Mutex::new(Value {
            handle,
            outcome: None,
        }));
        let (abort, registration) = AbortHandle::new_pair();
        let job = Arc::new(ResourceJob {
            identity: identity.clone(),
            operation,
            value: value.clone(),
            join: Mutex::new(None),
            abort: abort.clone(),
            applied: AtomicBool::new(false),
        });
        self.jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(identity.resource.clone(), job.clone());
        let outcome = value.clone();
        let joined = self.tasks.spawn(identity.clone(), abort, async move {
            let result = identity
                .run(async move {
                    Abortable::new(async move { work(value).await }, registration)
                        .await
                        .unwrap_or(Err(GraphError::Cancelled))
                })
                .await;
            outcome
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .outcome = Some(result.map_err(Arc::new));
        });
        match joined {
            Ok(joined) => {
                *job.join.lock().unwrap_or_else(|error| error.into_inner()) = Some(joined)
            }
            Err(error) => {
                job.value
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .outcome = Some(Err(Arc::new(error)));
            }
        }
        self.changed.wake();
        job
    }

    pub fn cancel_construction(&self) {
        for job in self
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            if job.operation == ResourceOperation::Construct {
                job.abort.abort();
            }
        }
    }

    pub async fn next(&self) -> Arc<ResourceJob> {
        futures::future::poll_fn(|cx| {
            self.changed.register(cx.waker());
            for job in self
                .jobs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .values()
            {
                if !job.applied.load(Ordering::Acquire) && job.poll(cx).is_ready() {
                    return Poll::Ready(job.clone());
                }
            }
            Poll::Pending
        })
        .await
    }

    pub async fn settle(&self, timeout: Duration) -> Vec<(Arc<ResourceJob>, GraphResult<()>)> {
        let jobs: Vec<_> = self
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .cloned()
            .collect();
        let mut pending: FuturesUnordered<_> = jobs
            .into_iter()
            .map(|job| async move {
                let result = tokio::time::timeout(timeout, job.wait())
                    .await
                    .map_err(|_| {
                        job.identity.error(
                            crate::context::workers::WorkerCleanupError::TimedOut { timeout }
                                .into(),
                        )
                    });
                (job, result.and_then(|result| result))
            })
            .collect();
        let mut results = Vec::new();
        while let Some(result) = pending.next().await {
            results.push(result);
        }
        results
    }

    pub fn shutdown_rejected(
        &self,
        id: &ResourceId,
        handle: Option<ResourceHandle>,
        timeout: Duration,
        release_only: bool,
    ) -> GraphResult<Arc<ResourceJob>> {
        if let Some(job) = self.get(id) {
            return Ok(job);
        }
        let identity = ResourceIdentity {
            resource: id.clone(),
            generation: 0,
            epoch: self.epoch()?,
            phase: FailurePhase::Removal,
            damage: Arc::default(),
        };
        let handle = handle.ok_or_else(|| topology("rejected resource has no cleanup owner"))?;
        Ok(self.start_shutdown(identity, handle, timeout, release_only))
    }

    pub fn complete_rejected(
        &self,
        job: &Arc<ResourceJob>,
    ) -> (Option<ResourceHandle>, GraphResult<()>) {
        if !job.joined() {
            return (None, Err(GraphError::OperationInProgress));
        }
        let mut value = job.value.lock().unwrap_or_else(|error| error.into_inner());
        let result = value
            .outcome
            .clone()
            .ok_or_else(|| topology("rejected resource cleanup has no joined result"))
            .and_then(|outcome| outcome.map_err(|cause| GraphError::Reported { cause }));
        let handle = value.handle.take();
        if result.is_ok() || handle.is_some() {
            self.jobs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&job.identity.resource);
        }
        (handle, result)
    }

    fn start_shutdown(
        &self,
        identity: ResourceIdentity,
        handle: ResourceHandle,
        timeout: Duration,
        release_only: bool,
    ) -> Arc<ResourceJob> {
        let id = identity.resource.clone();
        self.start(
            identity,
            ResourceOperation::Shutdown,
            Some(handle),
            move |value| async move {
                let handle = value
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .handle
                    .take();
                let mut lease = ResourceLease { value, handle };
                if !release_only {
                    tokio::time::timeout(timeout, async {
                        lease
                            .handle
                            .as_ref()
                            .expect("owned cleanup resource")
                            .shutdown()
                            .await
                    })
                    .await
                    .map_err(|error| GraphError::ResourceCleanup {
                        resource: id.clone(),
                        source: error.into(),
                    })?
                    .map_err(|source| GraphError::ResourceCleanup {
                        resource: id,
                        source,
                    })?;
                }
                drop(lease.handle.take());
                Ok(())
            },
        )
    }
}

fn identity(
    graph: &ComputationGraph,
    id: &ResourceId,
    epoch: OperationEpoch,
    phase: FailurePhase,
) -> GraphResult<ResourceIdentity> {
    let observed = graph.observed();
    let resource = observed
        .resources
        .get(id)
        .ok_or_else(|| topology(format!("resource {id} is not declared")))?;
    Ok(ResourceIdentity {
        resource: id.clone(),
        generation: resource.generation,
        epoch,
        phase,
        damage: Arc::new(Mutex::new(
            resource
                .failure
                .as_ref()
                .filter(|failure| failure.cause.damages_generation())
                .map(|failure| failure.cause.clone()),
        )),
    })
}

pub(super) fn construct(
    graph: &ComputationGraph,
    id: &ResourceId,
    constructor: Arc<dyn ResourceConstructor>,
    dependencies: BTreeMap<ResourceId, ResourceHandle>,
    expected: ResourceRole,
) -> GraphResult<Arc<ResourceJob>> {
    if graph.tasks.resources.contains(id) {
        return Err(GraphError::OperationInProgress);
    }
    let identity = identity(
        graph,
        id,
        graph.tasks.resources.epoch()?,
        FailurePhase::Creation,
    )?;
    let id = id.clone();
    let timeout = Duration::from_secs(30);
    Ok(graph.tasks.resources.start(
        identity,
        ResourceOperation::Construct,
        None,
        move |value| async move {
            let handle = tokio::time::timeout(timeout, async {
                constructor.construct_with_dependencies(&dependencies).await
            })
            .await
            .map_err(|error| GraphError::ResourceCreation {
                resource: id.clone(),
                source: error.into(),
            })?
            .map_err(|source| GraphError::ResourceCreation {
                resource: id.clone(),
                source,
            })?;
            let actual = handle.role();
            value
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .handle = Some(handle);
            if actual != expected {
                return Err(GraphError::ResourceCreation {
                    resource: id,
                    source: anyhow::anyhow!("provider role differs from declaration"),
                });
            }
            Ok(())
        },
    ))
}

struct ResourceLease {
    value: Arc<Mutex<Value>>,
    handle: Option<ResourceHandle>,
}

impl Drop for ResourceLease {
    fn drop(&mut self) {
        self.value
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle = self.handle.take();
    }
}

pub(super) fn shutdown(
    graph: &mut ComputationGraph,
    id: &ResourceId,
) -> GraphResult<Arc<ResourceJob>> {
    if let Some(job) = graph.tasks.resources.get(id) {
        return if job.operation == ResourceOperation::Shutdown {
            Ok(job)
        } else {
            Err(GraphError::OperationInProgress)
        };
    }
    let identity = identity(
        graph,
        id,
        graph.tasks.resources.epoch()?,
        FailurePhase::Removal,
    )?;
    let handle = graph
        .resource_handles
        .remove(id)
        .or_else(|| graph.pending_resource_cleanup.remove(id))
        .ok_or_else(|| topology(format!("resource {id} has no cleanup owner")))?;
    let timeout = graph.cleanup_timeout;
    Ok(graph
        .tasks
        .resources
        .start_shutdown(identity, handle, timeout, false))
}

/// Apply only an actually joined callback. A timed-out waiter leaves this record
/// and its handle untouched; a later controller turn or shutdown can join it.
pub(super) fn complete(graph: &mut ComputationGraph, job: &Arc<ResourceJob>) -> GraphResult<()> {
    if job
        .join
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_some()
    {
        return Err(GraphError::OperationInProgress);
    }
    let id = &job.identity.resource;
    let mut value = job.value.lock().unwrap_or_else(|error| error.into_inner());
    let outcome = value.outcome.clone().unwrap_or_else(|| {
        Err(Arc::new(job.identity.error(anyhow::anyhow!(
            "resource callback has no joined result"
        ))))
    });
    if job.applied.swap(true, Ordering::AcqRel) {
        return outcome.map_err(|cause| GraphError::Reported { cause });
    }
    let current = graph.observed();
    if current
        .resources
        .get(id)
        .map(|resource| resource.generation)
        != Some(job.identity.generation)
    {
        job.applied.store(false, Ordering::Release);
        return Err(GraphError::StaleGeneration);
    }
    let failure = outcome.as_ref().err().map(|cause| {
        failure(
            GraphError::Reported {
                cause: cause.clone(),
            },
            job.identity.phase,
        )
    });
    match (job.operation, &outcome) {
        (ResourceOperation::Construct, Ok(())) => {
            let handle = value
                .handle
                .take()
                .expect("successful resource construction");
            graph.resource_handles.insert(id.clone(), handle);
        }
        (_, Err(_)) => {
            if let Some(handle) = value.handle.take() {
                graph.pending_resource_cleanup.insert(id.clone(), handle);
            }
        }
        (ResourceOperation::Shutdown, Ok(())) => {}
    }
    let retained = graph.pending_resource_cleanup.contains_key(id);
    let destroyed_with_failure =
        job.operation == ResourceOperation::Shutdown && outcome.is_err() && !retained;
    update(graph, |observed| {
        let resource = observed
            .resources
            .get_mut(id)
            .expect("resource generation checked");
        resource.realization = if retained || destroyed_with_failure {
            ResourceRealization::CleanupRequired
        } else if job.operation == ResourceOperation::Shutdown {
            ResourceRealization::Released
        } else if outcome.is_ok() {
            ResourceRealization::Created
        } else {
            ResourceRealization::Pending
        };
        if failure.is_some() {
            resource.failure = failure;
        } else if !resource
            .failure
            .as_ref()
            .is_some_and(|failure| failure.cause.damages_generation())
        {
            resource.failure = None;
        }
        resource.transition_time = chrono::Utc::now();
    });
    if !destroyed_with_failure {
        graph
            .tasks
            .resources
            .jobs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(id);
    }
    outcome.map_err(|cause| GraphError::Reported { cause })
}
