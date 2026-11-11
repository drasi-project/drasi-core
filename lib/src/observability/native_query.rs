// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    ComponentObservations, ExecutionContext, ExecutionScope, OperationObservation,
    QueryObservation, QueryObservationKind, QueryObservationOutcome, QueryObservationRequest,
    QueryObservationResult, QueryPhase, Result, TelemetryError, TelemetryLifetime, TelemetryRecord,
};
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
};

/// Transient attribution for one native query transaction. Capturing this scope
/// never retains a storage owner or starts a processing span.
#[derive(Clone)]
pub struct NativeQueryScope(Arc<TransactionScope>);

struct TransactionScope {
    owner: Arc<ComponentObservations>,
    lifetime: TelemetryLifetime,
    scope: ExecutionScope,
    phase: QueryPhase,
    detail: bool,
    causal_context: Mutex<CausalContext>,
    armed: AtomicBool,
}

struct CausalContext {
    context: ExecutionContext,
    root: Option<Arc<QueryRoot>>,
}

struct QueryRoot {
    owner: Weak<ComponentObservations>,
    span: tracing::Span,
    bootstrap: bool,
    inputs: AtomicU64,
}

impl Drop for QueryRoot {
    fn drop(&mut self) {
        if self.bootstrap {
            self.span
                .record("input_count", self.inputs.load(Ordering::Relaxed) as i64);
        }
    }
}

pub(super) fn query_input(
    owner: &ComponentObservations,
    phase: super::QueryObservationPhase,
) -> Result<()> {
    if phase != super::QueryObservationPhase::Bootstrap {
        return Ok(());
    }
    let active_root = CURRENT_ROOT.with(|current| current.borrow().clone());
    if let Some(root) = active_root.filter(|root| std::ptr::eq(root.owner.as_ptr(), owner)) {
        root.input();
        return Ok(());
    }
    let Some(transaction) = NativeQueryScope::current()
        .filter(|transaction| std::ptr::eq(transaction.0.owner.as_ref(), owner))
    else {
        return Ok(());
    };
    let causal = transaction
        .0
        .causal_context
        .lock()
        .map_err(|_| TelemetryError::Poisoned)?;
    if let Some(root) = causal.root.as_ref().filter(|root| root.bootstrap) {
        root.input();
    }
    Ok(())
}

thread_local! {
    static CURRENT: RefCell<Option<NativeQueryScope>> = const { RefCell::new(None) };
    static CURRENT_ROOT: RefCell<Option<Arc<QueryRoot>>> = const { RefCell::new(None) };
}

impl QueryRoot {
    fn input(&self) {
        if self.bootstrap {
            let _ = self
                .inputs
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    Some(count.saturating_add(1).min(i64::MAX as u64))
                });
        }
    }
}

struct CurrentRootGuard(Option<Arc<QueryRoot>>);

impl Drop for CurrentRootGuard {
    fn drop(&mut self) {
        CURRENT_ROOT.with(|current| current.replace(self.0.take()));
    }
}

struct CurrentGuard(Option<NativeQueryScope>);

impl Drop for CurrentGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| current.replace(self.0.take()));
    }
}

impl NativeQueryScope {
    /// Capture only the transaction being accepted on this thread. Independent
    /// operations and empty scheduled probes cannot inherit a prior transaction.
    pub fn current() -> Option<Self> {
        CURRENT.with(|current| current.borrow().clone())
    }

    /// Observe the actual host session commit. Its parent is selected at commit
    /// start, after the plugin has discovered bootstrap/scheduled/replay work.
    pub fn observer(&self) -> Option<Arc<dyn drasi_core::computation::QueryObserver>> {
        if !self.0.lifetime.is_active().unwrap_or(false) {
            return None;
        }
        self.0
            .owner
            .observes_query_operations(self.0.detail)
            .then(|| {
                Arc::new(NativeQueryObserver {
                    scope: self.clone(),
                }) as Arc<dyn drasi_core::computation::QueryObserver>
            })
    }

    /// Restore transaction attribution for one synchronous callback or future
    /// poll. No thread-local guard survives a suspension.
    pub fn in_scope(&self, work: &mut dyn FnMut()) -> Result<()> {
        self.with_scope(work)
    }

    fn with_scope<T>(&self, work: impl FnOnce() -> T) -> Result<T> {
        if !self.0.lifetime.is_active()? {
            return Err(TelemetryError::Retired);
        }
        let context = self.context()?;
        let _current = CurrentGuard(CURRENT.with(|current| current.replace(Some(self.clone()))));
        Ok(self
            .0
            .owner
            .in_scope(|| self.0.scope.with_context(context).in_scope(work)))
    }

    fn context(&self) -> Result<ExecutionContext> {
        self.0
            .causal_context
            .lock()
            .map(|causal| causal.context.clone())
            .map_err(|_| TelemetryError::Poisoned)
    }

    fn select_root(&self, context: &ExecutionContext, root: Option<Arc<QueryRoot>>) -> Result<()> {
        *self
            .0
            .causal_context
            .lock()
            .map_err(|_| TelemetryError::Poisoned)? = CausalContext {
            context: context.clone(),
            root,
        };
        Ok(())
    }

    fn arm(&self) {
        self.0.armed.store(true, Ordering::Release);
    }
}

struct NativeQueryObserver {
    scope: NativeQueryScope,
}

struct UnarmedObservation(TelemetryLifetime);

impl drasi_core::computation::QueryObservation for UnarmedObservation {
    fn in_scope(&self, work: &mut dyn FnMut()) {
        work();
    }

    fn in_scope_result(
        &self,
        work: &mut dyn FnMut(),
    ) -> std::result::Result<(), drasi_core::interface::IndexError> {
        if !self
            .0
            .is_active()
            .map_err(drasi_core::interface::IndexError::other)?
        {
            return Err(drasi_core::interface::IndexError::other(
                TelemetryError::Retired,
            ));
        }
        work();
        Ok(())
    }

    fn finish(&mut self, _: bool, _: Option<usize>) {}
}

impl drasi_core::computation::QueryObserver for NativeQueryObserver {
    fn start(
        &self,
        operation: drasi_core::computation::QueryOperation,
        change_count: Option<usize>,
    ) -> std::result::Result<
        Box<dyn drasi_core::computation::QueryObservation>,
        drasi_core::interface::IndexError,
    > {
        let transaction = &self.scope.0;
        if !transaction
            .lifetime
            .is_active()
            .map_err(drasi_core::interface::IndexError::other)?
        {
            return Err(drasi_core::interface::IndexError::other(
                TelemetryError::Retired,
            ));
        }
        if operation == drasi_core::computation::QueryOperation::Commit
            && !transaction.armed.swap(false, Ordering::AcqRel)
        {
            return Ok(Box::new(UnarmedObservation(transaction.lifetime.clone())));
        }
        self.scope
            .with_scope(|| {
                transaction
                    .owner
                    .begin_query_operation(
                        operation,
                        &ExecutionContext::current(),
                        transaction.phase,
                        transaction.detail,
                        change_count,
                    )
                    .map(|observation| {
                        Box::new(observation.with_lifetime(transaction.lifetime.clone()))
                            as Box<dyn drasi_core::computation::QueryObservation>
                    })
            })
            .and_then(|result| result)
            .map_err(drasi_core::interface::IndexError::other)
    }
}

pub(super) struct RuntimeQueryObservation {
    owner: Arc<ComponentObservations>,
    lifetime: TelemetryLifetime,
    transaction: Option<NativeQueryScope>,
    observation: Option<OperationObservation>,
    scope: ExecutionScope,
    phase_detail: bool,
    root: Option<Arc<QueryRoot>>,
    finished: bool,
}

impl RuntimeQueryObservation {
    pub(super) fn begin(
        owner: Arc<ComponentObservations>,
        lifetime: TelemetryLifetime,
        request: &QueryObservationRequest,
    ) -> Result<Option<Box<dyn QueryObservation>>> {
        request.validate()?;
        if !lifetime.is_active()? {
            return Err(TelemetryError::Retired);
        }
        let current = ExecutionContext::current();
        let parent = match &request.parent {
            Some(trace) => current.with_trace_from(&ExecutionContext::new(trace.clone())),
            None => current,
        };
        let scope = ExecutionScope::capture()
            .with_context(parent.clone())
            .with_adapter(owner.telemetry().context_adapter().cloned());
        let transaction = if request.kind == QueryObservationKind::Transaction {
            Some(NativeQueryScope(Arc::new(TransactionScope {
                owner: owner.clone(),
                lifetime: lifetime.clone(),
                scope: scope.clone(),
                phase: request.phase.into(),
                detail: request.phase == super::QueryObservationPhase::Steady,
                causal_context: Mutex::new(CausalContext {
                    context: parent.clone(),
                    root: None,
                }),
                armed: AtomicBool::new(false),
            })))
        } else {
            NativeQueryScope::current()
                .filter(|transaction| Arc::ptr_eq(&transaction.0.owner, &owner))
        };
        let observation = owner
            .begin_native_query(request, &parent)?
            .map(|observation| observation.with_lifetime(lifetime.clone()));
        let root_kind = matches!(
            request.kind,
            QueryObservationKind::Bootstrap
                | QueryObservationKind::FutureQueue
                | QueryObservationKind::OutputReplay
                | QueryObservationKind::InputReplay
        );
        let root = root_kind
            .then(|| {
                observation.as_ref().map(|observation| {
                    Arc::new(QueryRoot {
                        owner: Arc::downgrade(&owner),
                        span: observation.span(),
                        bootstrap: request.kind == QueryObservationKind::Bootstrap,
                        inputs: AtomicU64::new(0),
                    })
                })
            })
            .flatten();
        if let Some(transaction) = &transaction {
            if root_kind
                || matches!(
                    request.kind,
                    QueryObservationKind::Evaluate | QueryObservationKind::CommitWork
                )
            {
                transaction.arm();
            }
            if root_kind {
                if let Some(observation) = &observation {
                    transaction.select_root(observation.context(), root.clone())?;
                }
            }
        }
        if observation.is_none()
            && !matches!(
                request.kind,
                QueryObservationKind::Transaction | QueryObservationKind::CommitWork
            )
        {
            return Ok(None);
        }
        Ok(Some(Box::new(Self {
            owner,
            lifetime,
            transaction: (request.kind == QueryObservationKind::Transaction)
                .then_some(transaction)
                .flatten(),
            observation,
            scope,
            phase_detail: matches!(
                request.kind,
                QueryObservationKind::PrepareInput
                    | QueryObservationKind::PrepareOutput
                    | QueryObservationKind::Finalize
            ),
            root,
            finished: false,
        })))
    }

    fn check_active(&self) -> Result<()> {
        if self.finished {
            return Err(TelemetryError::Invalid(
                "query observation is already finished".into(),
            ));
        }
        if !self.lifetime.is_active()? {
            return Err(TelemetryError::Retired);
        }
        Ok(())
    }
}

impl QueryObservation for RuntimeQueryObservation {
    fn context(&self) -> ExecutionContext {
        if self.phase_detail {
            self.scope.context().clone()
        } else if let Some(observation) = &self.observation {
            observation.context().clone()
        } else {
            self.transaction
                .as_ref()
                .and_then(|transaction| transaction.context().ok())
                .unwrap_or_else(|| self.scope.context().clone())
        }
    }

    fn in_scope(&self, work: &mut dyn FnMut()) -> Result<()> {
        self.check_active()?;
        // A bootstrap root spans several transactions, including post-commit
        // input publications. Carry it only for this callback/poll.
        let _root = self.root.as_ref().map(|root| {
            CurrentRootGuard(CURRENT_ROOT.with(|current| current.replace(Some(root.clone()))))
        });
        if self.root.is_some() {
            if let Some(transaction) = NativeQueryScope::current()
                .filter(|transaction| Arc::ptr_eq(&transaction.0.owner, &self.owner))
            {
                transaction.select_root(&self.context(), self.root.clone())?;
            }
        }
        let scoped = || {
            if self.phase_detail {
                self.scope.in_scope(work);
            } else if let Some(observation) = &self.observation {
                observation.in_scope(work);
            } else {
                work();
            }
        };
        match &self.transaction {
            Some(transaction) => transaction.with_scope(scoped),
            None => {
                self.scope.in_scope(scoped);
                Ok(())
            }
        }
    }

    fn finish(&mut self, result: QueryObservationResult) -> Result<()> {
        result.validate()?;
        self.check_active()?;
        if let Some(observation) = &mut self.observation {
            if let Some(root) = self.root.as_ref().filter(|root| root.bootstrap) {
                observation.record("input_count", root.inputs.load(Ordering::Relaxed) as i64);
            }
            if let Some(count) = result.result_count {
                // Root completions report output envelopes, not evaluator rows.
                observation.record(
                    if self.root.is_some() {
                        "output_count"
                    } else {
                        "result_change_count"
                    },
                    i64::try_from(count).map_err(|_| {
                        TelemetryError::Invalid("query result count exceeds trace range".into())
                    })?,
                );
            }
            observation.finish(match result.outcome {
                QueryObservationOutcome::Success => 0,
                QueryObservationOutcome::Error => 1,
                QueryObservationOutcome::Cancelled => 2,
            });
        }
        self.finished = true;
        Ok(())
    }
}
