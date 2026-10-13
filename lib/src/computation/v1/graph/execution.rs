// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use futures::{
    future::{AbortHandle, BoxFuture},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use tokio::{sync::RwLock, task::JoinHandle};

use super::controller::InstanceSlot;
use super::{topology, GraphError, GraphResult};
use crate::computation::v1::{
    ComponentInterrupted, ComponentPanic, ComponentUnwind, FailurePhase, OperationEpoch,
    ResourceId, ResourceUnwind,
};

pub(super) use crate::computation::v1::bounded_panic_message as panic_message;

#[derive(Clone)]
pub(super) struct Identity {
    pub slot: Arc<InstanceSlot>,
    pub epoch: OperationEpoch,
    pub phase: FailurePhase,
    pub action: &'static str,
}

impl Identity {
    pub fn panic(&self, payload: Box<dyn std::any::Any + Send>) -> GraphError {
        let source = ComponentPanic::new(ComponentUnwind {
            component: self.slot.id.clone(),
            generation: self.slot.generation,
            operation: self.epoch,
            phase: self.phase,
            message: panic_message(payload),
        });
        self.error(source.into())
    }

    pub fn error(&self, source: anyhow::Error) -> GraphError {
        GraphError::Component {
            component: self.slot.id.clone(),
            operation: self.action,
            source,
        }
    }

    pub fn interrupted(&self, reason: &'static str) -> GraphError {
        self.retain(
            self.error(
                ComponentInterrupted {
                    component: self.slot.id.clone(),
                    generation: self.slot.generation,
                    operation: self.epoch,
                    phase: self.phase,
                    reason,
                }
                .into(),
            ),
        )
    }

    pub fn retain(&self, error: GraphError) -> GraphError {
        if error.damages_generation() {
            self.slot.preserve_failure(error)
        } else {
            error
        }
    }

    /// Covers construction/polling and, separately, destruction of the work
    /// future. The instance lease is returned by RAII before the task is joined.
    pub async fn run<T>(&self, work: impl Future<Output = GraphResult<T>>) -> GraphResult<T> {
        TaskIdentity::Component(self.clone()).run(work).await
    }

    pub fn call<T>(&self, call: impl FnOnce() -> GraphResult<T>) -> GraphResult<T> {
        catch_unwind(AssertUnwindSafe(call))
            .unwrap_or_else(|payload| Err(self.panic(payload)))
            .map_err(|error| self.retain(error))
    }
}

#[derive(Clone)]
pub(super) struct ResourceIdentity {
    pub resource: ResourceId,
    pub generation: u64,
    pub epoch: OperationEpoch,
    pub phase: FailurePhase,
    pub damage: Arc<Mutex<Option<Arc<GraphError>>>>,
}

impl ResourceIdentity {
    pub fn error(&self, source: anyhow::Error) -> GraphError {
        if self.phase == FailurePhase::Creation {
            GraphError::ResourceCreation {
                resource: self.resource.clone(),
                source,
            }
        } else {
            GraphError::ResourceCleanup {
                resource: self.resource.clone(),
                source,
            }
        }
    }

    fn panic(&self, payload: Box<dyn std::any::Any + Send>) -> GraphError {
        self.error(
            ComponentPanic::new(ResourceUnwind {
                resource: self.resource.clone(),
                generation: self.generation,
                operation: self.epoch,
                phase: self.phase,
                message: panic_message(payload),
            })
            .into(),
        )
    }

    fn retain(&self, error: GraphError) -> GraphError {
        let mut damage = self
            .damage
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(cause) = damage.as_ref() {
            GraphError::Cleanup {
                primary: Some(Box::new(GraphError::Reported {
                    cause: cause.clone(),
                })),
                errors: vec![error],
            }
        } else if error.damages_generation() {
            let cause = Arc::new(error);
            *damage = Some(cause.clone());
            GraphError::Reported { cause }
        } else {
            error
        }
    }

    pub async fn run<T>(&self, work: impl Future<Output = GraphResult<T>>) -> GraphResult<T> {
        TaskIdentity::Resource(self.clone()).run(work).await
    }
}

#[derive(Clone)]
pub(super) enum TaskIdentity {
    Component(Identity),
    Resource(ResourceIdentity),
    Graph(GraphIdentity),
}

#[derive(Clone)]
pub(super) struct GraphIdentity {
    pub graph: Arc<str>,
    pub run_epoch: u64,
    pub phase: FailurePhase,
}

impl GraphIdentity {
    fn error(&self, source: anyhow::Error) -> GraphError {
        GraphError::Driver {
            graph: self.graph.clone(),
            run_epoch: self.run_epoch,
            source,
        }
    }
    fn panic(&self, payload: Box<dyn std::any::Any + Send>) -> GraphError {
        self.error(
            ComponentPanic::new(anyhow::anyhow!(
                "graph callback panicked during {:?}: {}",
                self.phase,
                panic_message(payload)
            ))
            .into(),
        )
    }
    pub fn call<T>(&self, call: impl FnOnce() -> GraphResult<T>) -> GraphResult<T> {
        catch_unwind(AssertUnwindSafe(call)).unwrap_or_else(|payload| Err(self.panic(payload)))
    }
    pub async fn run<T>(&self, work: impl Future<Output = GraphResult<T>>) -> GraphResult<T> {
        TaskIdentity::Graph(self.clone()).run(work).await
    }
}

impl From<GraphIdentity> for TaskIdentity {
    fn from(value: GraphIdentity) -> Self {
        Self::Graph(value)
    }
}

impl From<Identity> for TaskIdentity {
    fn from(value: Identity) -> Self {
        Self::Component(value)
    }
}

impl From<ResourceIdentity> for TaskIdentity {
    fn from(value: ResourceIdentity) -> Self {
        Self::Resource(value)
    }
}

impl TaskIdentity {
    fn phase(&self) -> FailurePhase {
        match self {
            Self::Component(identity) => identity.phase,
            Self::Resource(identity) => identity.phase,
            Self::Graph(identity) => identity.phase,
        }
    }

    fn panic(&self, payload: Box<dyn std::any::Any + Send>) -> GraphError {
        match self {
            Self::Component(identity) => identity.panic(payload),
            Self::Resource(identity) => identity.panic(payload),
            Self::Graph(identity) => identity.panic(payload),
        }
    }

    fn error(&self, error: anyhow::Error) -> GraphError {
        match self {
            Self::Component(identity) => identity.error(error),
            Self::Resource(identity) => identity.error(error),
            Self::Graph(identity) => identity.error(error),
        }
    }

    fn retain(&self, error: GraphError) -> GraphError {
        match self {
            Self::Component(identity) => identity.retain(error),
            Self::Resource(identity) => identity.retain(error),
            Self::Graph(_) => error,
        }
    }

    pub(super) fn call<T>(&self, call: impl FnOnce() -> GraphResult<T>) -> GraphResult<T> {
        catch_unwind(AssertUnwindSafe(call))
            .unwrap_or_else(|payload| Err(self.panic(payload)))
            .map_err(|error| self.retain(error))
    }

    pub(super) async fn run<T>(
        &self,
        work: impl Future<Output = GraphResult<T>>,
    ) -> GraphResult<T> {
        let mut work = Box::pin(work);
        let result = AssertUnwindSafe(work.as_mut()).catch_unwind().await;
        let dropped = catch_unwind(AssertUnwindSafe(|| drop(work)));
        let result = match result {
            Ok(result) => result,
            Err(payload) => Err(self.panic(payload)),
        };
        let result = match dropped {
            Ok(()) => result,
            Err(payload) => Err(GraphError::Cleanup {
                primary: result.err().map(Box::new),
                errors: vec![self.panic(payload)],
            }),
        };
        result.map_err(|error| self.retain(error))
    }
}

struct Task<T> {
    identity: TaskIdentity,
    abort: AbortHandle,
    join: RwLock<Option<JoinHandle<T>>>,
}

/// The graph, not the controller future, owns every submitted task and join.
/// Waiters may be dropped at any await without releasing that ownership.
pub(super) struct Tasks<T> {
    next: AtomicU64,
    entries: Mutex<BTreeMap<u64, Arc<Task<T>>>>,
}

impl<T> Default for Tasks<T> {
    fn default() -> Self {
        Self {
            next: AtomicU64::new(0),
            entries: Mutex::new(BTreeMap::new()),
        }
    }
}

impl<T: Send + 'static> Tasks<T> {
    pub fn spawn(
        self: &Arc<Self>,
        identity: impl Into<TaskIdentity>,
        abort: AbortHandle,
        work: impl Future<Output = T> + Send + 'static,
    ) -> GraphResult<BoxFuture<'static, GraphResult<T>>> {
        let id = self
            .next
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| topology("task identity exhausted"))?;
        let task = Arc::new(Task {
            identity: identity.into(),
            abort,
            join: RwLock::new(None),
        });
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, task.clone());
        if let Err(error) = crate::context::workers::try_spawn_owned_worker(&task.join, work) {
            self.entries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&id);
            return Err(task.identity.error(error.into()));
        }
        Ok(self.wait(id, task))
    }

    fn wait(self: &Arc<Self>, id: u64, task: Arc<Task<T>>) -> BoxFuture<'static, GraphResult<T>> {
        let owner = self.clone();
        async move {
            let mut join = task.join.write().await;
            let Some(handle) = join.as_mut() else {
                return Err(topology("task completion was already joined"));
            };
            let result = handle.await;
            *join = None;
            owner
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&id);
            result.map_err(|error| {
                let error = if error.is_panic() {
                    task.identity.panic(error.into_panic())
                } else {
                    task.identity.error(error.into())
                };
                task.identity.retain(error)
            })
        }
        .boxed()
    }

    pub fn is_empty(&self) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty()
    }

    pub fn cancel(&self) {
        for task in self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            task.abort.abort();
        }
    }

    pub fn cancel_processing(&self) {
        for task in self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            if task.identity.phase() != FailurePhase::Stop {
                task.abort.abort();
            }
        }
    }

    pub async fn settle(self: &Arc<Self>, timeout: Duration) -> Vec<GraphResult<T>> {
        let pending: Vec<_> = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|(&id, task)| (id, task.clone()))
            .collect();
        let mut joins: FuturesUnordered<_> = pending
            .into_iter()
            .map(|(id, task)| {
                let join = self.wait(id, task.clone());
                async move {
                    tokio::time::timeout(timeout, join).await.map_err(|_| {
                        task.identity.error(
                            crate::context::workers::WorkerCleanupError::TimedOut { timeout }
                                .into(),
                        )
                    })?
                }
            })
            .collect();
        let mut outcomes = Vec::new();
        while let Some(outcome) = joins.next().await {
            outcomes.push(outcome);
        }
        outcomes
    }
}
