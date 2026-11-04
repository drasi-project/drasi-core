// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    BatchTraceParent, ExecutionContext, ExecutionScope, GraphTelemetry, QueryMetrics, QueryPhase,
    ReactionSubscriptionMetrics, ReactionSubscriptions, TelemetryError, TelemetryScope,
    TELEMETRY_TARGET,
};
use drasi_computation_contracts::ComponentRole;
use metrics::{Counter, Gauge, Histogram};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    future::{poll_fn, Future},
    sync::{Arc, Mutex},
};
use tokio::time::Instant;
use tracing::{field::Empty, Instrument, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ObservedOperation {
    Next,
    Publish,
    Send,
    Transform,
    Continue,
    Handle,
    HandleBatch,
    Confirm,
    Acknowledge,
    Start,
    Stop,
    Reconfigure,
    Run,
    Evaluate,
    Commit,
    Control,
    Bootstrap,
    Snapshot,
    Scheduled,
    Reaction,
    ReactionBatch,
    ServiceWork,
}

impl ObservedOperation {
    const ALL: [Self; 19] = [
        Self::Next,
        Self::Publish,
        Self::Send,
        Self::Transform,
        Self::Continue,
        Self::Handle,
        Self::HandleBatch,
        Self::Confirm,
        Self::Acknowledge,
        Self::Start,
        Self::Stop,
        Self::Reconfigure,
        Self::Run,
        Self::Evaluate,
        Self::Commit,
        Self::Control,
        Self::Reaction,
        Self::ReactionBatch,
        Self::ServiceWork,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Next => "next",
            Self::Publish => "publish",
            Self::Send => "send",
            Self::Transform => "transform",
            Self::Continue => "continue_transform",
            Self::Handle => "handle",
            Self::HandleBatch => "handle_batch",
            Self::Confirm => "confirm_outputs",
            Self::Acknowledge => "acknowledge",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Reconfigure => "reconfigure",
            Self::Run => "run",
            Self::Evaluate => "evaluate",
            Self::Commit => "commit",
            Self::Control => "control",
            Self::Bootstrap => "bootstrap",
            Self::Snapshot => "snapshot",
            Self::Scheduled => "scheduled",
            Self::Reaction => "reaction",
            Self::ReactionBatch => "reaction_batch",
            Self::ServiceWork => "service_work",
        }
    }

    fn span_name(self) -> Option<&'static str> {
        Some(match self {
            Self::Next | Self::Run => return None,
            Self::Publish => "source.publish",
            Self::Send => "pipe.send",
            Self::Transform | Self::Continue => "component.process",
            Self::Handle => "sink.handle",
            Self::HandleBatch => "sink.handle_batch",
            Self::Confirm => "component.confirm_outputs",
            Self::Acknowledge => "pipe.acknowledge",
            Self::Start => "component.start",
            Self::Stop => "component.stop",
            Self::Reconfigure => "component.reconfigure",
            Self::Evaluate => "query.evaluate",
            Self::Commit => "query.commit",
            Self::Control => "control.apply",
            Self::Bootstrap => "query.bootstrap",
            Self::Snapshot => "bootstrap.snapshot",
            Self::Scheduled => "component.scheduled",
            Self::Reaction => "reaction.process",
            Self::ReactionBatch => "reaction.process_batch",
            Self::ServiceWork => "component.process",
        })
    }

    fn common(self) -> bool {
        !matches!(
            self,
            Self::Evaluate | Self::Commit | Self::Control | Self::Send
        )
    }

    fn applies_to(self, role: ComponentRole) -> bool {
        match self {
            Self::Start | Self::Stop | Self::Reconfigure => true,
            Self::Next | Self::Publish => role == ComponentRole::Source,
            Self::Transform | Self::Continue | Self::Confirm => {
                matches!(role, ComponentRole::Transformer | ComponentRole::Query)
            }
            Self::Handle | Self::HandleBatch => role == ComponentRole::Sink,
            Self::Acknowledge => matches!(
                role,
                ComponentRole::Transformer | ComponentRole::Query | ComponentRole::Sink
            ),
            Self::Run => role == ComponentRole::Service,
            Self::ServiceWork => role == ComponentRole::Service,
            _ => false,
        }
    }

    fn error_kind(self) -> &'static str {
        match self {
            Self::Publish | Self::Send | Self::Acknowledge => "transport",
            Self::Commit => "storage",
            Self::Start | Self::Reconfigure | Self::Control => "configuration",
            _ => "processing",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OperationFields<'a> {
    pub input_port: Option<&'a str>,
    pub output_port: Option<&'a str>,
    pub input_count: Option<usize>,
    pub output_count: Option<usize>,
    pub change_count: Option<usize>,
    pub completion: Option<&'static str>,
    pub publication_mode: Option<&'static str>,
    pub queue_id: Option<&'a str>,
    pub handling_outcome: Option<&'static str>,
    pub target_count: Option<usize>,
    pub work_kind: Option<&'static str>,
    pub source_count: Option<usize>,
}

#[derive(Debug)]
struct OperationMetrics {
    calls: Counter,
    durations: [Histogram; 3],
    errors: Counter,
}

#[derive(Debug)]
pub(crate) struct ComponentObservations {
    pub generation: u64,
    config: GraphTelemetry,
    metrics: BTreeMap<ObservedOperation, OperationMetrics>,
    inputs: [Counter; 3],
    outputs: Counter,
    dispatched: [Counter; 2],
    storage_errors: Counter,
    worker_owned: std::sync::atomic::AtomicBool,
    query: std::sync::OnceLock<QueryMetrics>,
    subscriptions: Mutex<ReactionSubscriptions>,
    state: Mutex<BindingState>,
}

#[derive(Debug)]
struct BindingState {
    active: bool,
    up: Gauge,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<ComponentObservations>>> = const { RefCell::new(None) };
}

struct CurrentGuard(Option<Arc<ComponentObservations>>);

impl Drop for CurrentGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| current.replace(self.0.take()));
    }
}

impl ComponentObservations {
    pub(crate) fn new(
        config: GraphTelemetry,
        generation: u64,
    ) -> Result<Arc<Self>, TelemetryError> {
        let counter = |name, labels| -> Result<Counter, TelemetryError> {
            config.register(name, labels)?.counter()
        };
        let mut metrics = BTreeMap::new();
        if config.policy().metrics_enabled()
            && config
                .scope()
                .and_then(TelemetryScope::component_id)
                .is_some()
        {
            for operation in ObservedOperation::ALL.into_iter().filter(|operation| {
                operation.common()
                    && (config
                        .scope()
                        .and_then(TelemetryScope::role)
                        .is_some_and(|role| operation.applies_to(role))
                        || (matches!(
                            operation,
                            ObservedOperation::Reaction | ObservedOperation::ReactionBatch
                        ) && config.scope().and_then(TelemetryScope::semantic_kind)
                            == Some(drasi_computation_contracts::ComponentSemanticKind::Reaction)))
            }) {
                let labels = vec![("operation".into(), operation.label().into())];
                let duration = |outcome: &str| {
                    let mut labels = labels.clone();
                    labels.push(("outcome".into(), outcome.into()));
                    config
                        .register("drasi.component.operation.duration", labels)?
                        .histogram()
                };
                metrics.insert(
                    operation,
                    OperationMetrics {
                        calls: counter("drasi.component.calls", labels.clone())?,
                        durations: [
                            duration("success")?,
                            duration("error")?,
                            duration("cancelled")?,
                        ],
                        errors: counter(
                            "drasi.errors",
                            vec![("error_kind".into(), operation.error_kind().into())],
                        )?,
                    },
                );
            }
        }
        let mut inputs = [Counter::noop(), Counter::noop(), Counter::noop()];
        let mut outputs = Counter::noop();
        let mut dispatched = [Counter::noop(), Counter::noop()];
        let mut up = Gauge::noop();
        if config
            .scope()
            .and_then(TelemetryScope::component_id)
            .is_some()
        {
            for (handle, completion) in
                inputs
                    .iter_mut()
                    .zip(["transformed", "accepted", "handled"])
            {
                let role = config.scope().and_then(TelemetryScope::role);
                let supported =
                    match completion {
                        "transformed" => matches!(
                            role,
                            Some(ComponentRole::Transformer | ComponentRole::Query)
                        ),
                        "handled" => role == Some(ComponentRole::Sink)
                            || config.scope().and_then(TelemetryScope::semantic_kind)
                                == Some(
                                    drasi_computation_contracts::ComponentSemanticKind::Reaction,
                                ),
                        _ => role == Some(ComponentRole::Sink),
                    };
                if supported {
                    *handle = counter(
                        "drasi.component.inputs_processed",
                        vec![("completion".into(), completion.into())],
                    )?;
                }
            }
            if matches!(
                config.scope().and_then(TelemetryScope::role),
                Some(ComponentRole::Source | ComponentRole::Transformer | ComponentRole::Query)
            ) {
                outputs = counter("drasi.component.outputs_emitted", vec![])?;
            }
            up = config.register("drasi.component.up", vec![])?.gauge()?;
            if config.scope().and_then(TelemetryScope::semantic_kind)
                == Some(drasi_computation_contracts::ComponentSemanticKind::Source)
            {
                for (handle, phase) in dispatched.iter_mut().zip(["steady", "bootstrap"]) {
                    *handle = counter(
                        "drasi.source.events_dispatched",
                        vec![
                            (
                                "source_id".into(),
                                config
                                    .scope()
                                    .and_then(TelemetryScope::component_id)
                                    .expect("component scope")
                                    .to_string(),
                            ),
                            ("phase".into(), phase.into()),
                        ],
                    )?;
                }
            }
        }
        let storage_errors = if config.scope().and_then(TelemetryScope::semantic_kind)
            == Some(drasi_computation_contracts::ComponentSemanticKind::Reaction)
        {
            counter(
                "drasi.errors",
                vec![("error_kind".into(), "storage".into())],
            )?
        } else {
            Counter::noop()
        };
        up.set(0.0);
        Ok(Arc::new(Self {
            generation,
            config,
            metrics,
            inputs,
            outputs,
            dispatched,
            storage_errors,
            worker_owned: std::sync::atomic::AtomicBool::new(false),
            query: std::sync::OnceLock::new(),
            subscriptions: Mutex::new(BTreeMap::new()),
            state: Mutex::new(BindingState { active: true, up }),
        }))
    }

    pub(crate) fn current() -> Option<Arc<Self>> {
        CURRENT.with(|current| current.borrow().clone())
    }

    pub(crate) fn current_or_disabled() -> Arc<Self> {
        static DISABLED: std::sync::OnceLock<Arc<ComponentObservations>> =
            std::sync::OnceLock::new();
        Self::current().unwrap_or_else(|| {
            DISABLED
                .get_or_init(|| {
                    Self::new(GraphTelemetry::default(), 0).expect("disabled observations")
                })
                .clone()
        })
    }

    pub(crate) fn bind_query_metrics(&self) -> Result<(), TelemetryError> {
        if !self.config.policy().metrics_enabled()
            || self.query.get().is_some()
            || self.config.scope().and_then(TelemetryScope::semantic_kind)
                != Some(drasi_computation_contracts::ComponentSemanticKind::Query)
        {
            return Ok(());
        }
        let state = self.state.lock().map_err(|_| TelemetryError::Poisoned)?;
        if state.active && self.query.get().is_none() {
            self.query
                .set(QueryMetrics::new(&self.config)?)
                .expect("query metric binding is serialized");
        }
        Ok(())
    }

    pub(crate) fn query_observer(
        self: &Arc<Self>,
    ) -> Option<Arc<dyn drasi_core::computation::QueryObserver>> {
        self.query_observer_for(QueryPhase::Steady, true)
    }

    pub(crate) fn query_observer_for(
        self: &Arc<Self>,
        phase: QueryPhase,
        detail: bool,
    ) -> Option<Arc<dyn drasi_core::computation::QueryObserver>> {
        ((detail
            && (self.config.policy().allows_span("query.evaluate")
                || self.config.policy().allows_span("query.commit")))
            || self
                .config
                .policy()
                .allows_metric("drasi.query.engine.duration")
            || self
                .config
                .policy()
                .allows_metric("drasi.query.commit.duration"))
        .then(|| {
            Arc::new(QueryOperationObserver {
                owner: self.clone(),
                phase,
                detail,
            }) as Arc<dyn drasi_core::computation::QueryObserver>
        })
    }

    pub(crate) fn query_input(&self, phase: QueryPhase) {
        if self.query.get().is_none() {
            return;
        }
        self.update(|| {
            if let Some(query) = self.query.get() {
                query.inputs[phase as usize].increment(1);
            }
        });
    }

    pub(crate) fn query_results(&self, output: &crate::computation::v1::ChangeEnvelope) {
        if self.query.get().is_none() {
            return;
        }
        self.update(|| {
            if let Some(query) = self.query.get() {
                let mut counts = [0; 3];
                for change in output.changes().operations() {
                    use crate::computation::v1::ChangeOperation;
                    let index = match change {
                        ChangeOperation::Added { .. } => 0,
                        ChangeOperation::Updated { .. } => 1,
                        ChangeOperation::Deleted { .. } => 2,
                    };
                    counts[index] += 1;
                }
                for (counter, count) in query.results.iter().zip(counts) {
                    if count > 0 {
                        counter.increment(count);
                    }
                }
            }
        });
    }

    pub(crate) fn query_state(&self, outbox: usize, live: usize) {
        if self.query.get().is_none() {
            return;
        }
        self.update(|| {
            if let Some(query) = self.query.get() {
                if let Err(error) = query.state(&self.config, outbox, live) {
                    log::warn!("Cannot observe query output metrics: {error}");
                }
            }
        });
    }

    pub(crate) fn bind_reaction_queries(
        self: &Arc<Self>,
        queries: &[String],
    ) -> Result<(), TelemetryError> {
        if !self.config.policy().metrics_enabled()
            || self
                .config
                .scope()
                .and_then(TelemetryScope::component_id)
                .is_none()
        {
            return Ok(());
        }
        let mut subscriptions = self
            .subscriptions
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?;
        for query in queries {
            if !subscriptions.contains_key(query) {
                subscriptions.insert(
                    query.clone(),
                    ReactionSubscriptionMetrics::new(self, &self.config, query)?,
                );
            }
        }
        Ok(())
    }

    pub(crate) fn reaction_subscription(
        &self,
        query: &str,
    ) -> Option<Arc<ReactionSubscriptionMetrics>> {
        self.subscriptions
            .lock()
            .unwrap_or_else(|error| {
                log::error!("Reading poisoned reaction metric bindings: {error}");
                error.into_inner()
            })
            .get(query)
            .cloned()
    }

    pub(super) fn configured(&self) -> bool {
        self.config.policy().metrics_enabled()
            && self
                .config
                .scope()
                .and_then(TelemetryScope::component_id)
                .is_some()
    }

    pub(super) fn handled_inputs(&self, inputs: usize) {
        self.inputs[2].increment(inputs as u64);
    }

    pub(super) fn start_work(&self, operation: ObservedOperation) -> Option<Instant> {
        self.worker_owned
            .store(true, std::sync::atomic::Ordering::Release);
        let metrics = self.metrics.get(&operation)?;
        self.update(|| metrics.calls.increment(1));
        self.config
            .policy()
            .allows_metric("drasi.component.operation.duration")
            .then(Instant::now)
    }

    pub(super) fn finish_work(
        &self,
        operation: ObservedOperation,
        started: Option<Instant>,
        outcome: usize,
    ) {
        self.update(|| {
            if let Some(metrics) = self.metrics.get(&operation) {
                if let Some(started) = started {
                    metrics.durations[outcome].record(started.elapsed().as_secs_f64());
                }

                if outcome == 1 {
                    metrics.errors.increment(1);
                }
            }
        });
    }

    pub(crate) fn checkpoint_failed(&self) {
        self.update(|| self.storage_errors.increment(1));
    }

    fn owns_operation_errors(&self, operation: ObservedOperation) -> bool {
        operation != ObservedOperation::Run
            || !self.worker_owned.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) async fn scope<T>(self: &Arc<Self>, future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        poll_fn(|cx| {
            let _guard = CurrentGuard(CURRENT.with(|current| current.replace(Some(self.clone()))));
            future.as_mut().poll(cx)
        })
        .await
    }

    pub(crate) fn update_up(&self, up: bool) {
        let state = self.state.lock().unwrap_or_else(|error| {
            log::error!("Component telemetry state poisoned: {error}");
            error.into_inner()
        });
        if state.active {
            state.up.set(f64::from(up));
        }
    }

    pub(crate) fn revoke(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| {
            log::error!("Revoking poisoned component telemetry: {error}");
            error.into_inner()
        });
        if !state.active {
            return;
        }
        state.active = false;
        state.up.set(0.0);
        if let Some(query) = self.query.get() {
            query.retire();
        }
    }

    pub(super) fn update(&self, work: impl FnOnce()) {
        let state = self.state.lock().unwrap_or_else(|error| {
            log::error!("Updating poisoned component telemetry: {error}");
            error.into_inner()
        });
        if state.active {
            work();
        }
    }

    pub(crate) fn outputs(&self, count: usize) {
        self.update(|| self.outputs.increment(count as u64));
    }

    pub(crate) fn failed(&self, operation: ObservedOperation) {
        self.update(|| {
            let record_metrics = !matches!(
                operation,
                ObservedOperation::Reaction | ObservedOperation::ReactionBatch
            );
            if let Some(metrics) = self.metrics.get(&operation).filter(|_| record_metrics) {
                metrics.errors.increment(1);
            }
        });
    }

    pub(crate) fn dispatched(&self, bootstrap: bool) {
        self.update(|| self.dispatched[usize::from(bootstrap)].increment(1));
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        operation: ObservedOperation,
        parent: &ExecutionContext,
        fields: OperationFields<'_>,
    ) -> Result<OperationObservation, TelemetryError> {
        self.begin_with_links(operation, parent, fields, &[], false)
    }

    pub(crate) fn begin_untraced(
        self: &Arc<Self>,
        operation: ObservedOperation,
        parent: &ExecutionContext,
        fields: OperationFields<'_>,
    ) -> Result<OperationObservation, TelemetryError> {
        self.begin_operation(operation, parent, fields, &[], false, false)
    }

    pub(crate) fn root(
        self: &Arc<Self>,
        operation: ObservedOperation,
        link: &ExecutionContext,
        fields: OperationFields<'_>,
    ) -> Result<OperationObservation, TelemetryError> {
        self.begin_with_links(
            operation,
            &ExecutionContext::default(),
            fields,
            link.trace().map(std::slice::from_ref).unwrap_or_default(),
            false,
        )
    }

    pub(crate) fn batch<'a>(
        self: &Arc<Self>,
        contexts: impl IntoIterator<Item = &'a ExecutionContext>,
        fields: OperationFields<'_>,
    ) -> Result<OperationObservation, TelemetryError> {
        self.batch_operation(ObservedOperation::HandleBatch, contexts, fields)
    }

    pub(crate) fn batch_operation<'a>(
        self: &Arc<Self>,
        operation: ObservedOperation,
        contexts: impl IntoIterator<Item = &'a ExecutionContext>,
        fields: OperationFields<'_>,
    ) -> Result<OperationObservation, TelemetryError> {
        let selected = if operation
            .span_name()
            .is_some_and(|name| self.config.policy().allows_span(name))
        {
            BatchTraceParent::select(contexts, self.config.link_limits())
        } else {
            BatchTraceParent {
                parent: ExecutionContext::default(),
                links: vec![],
                links_truncated: false,
            }
        };
        self.begin_with_links(
            operation,
            &selected.parent,
            fields,
            &selected.links,
            selected.links_truncated,
        )
    }

    fn begin_with_links(
        self: &Arc<Self>,
        operation: ObservedOperation,
        parent: &ExecutionContext,
        fields: OperationFields<'_>,
        links: &[super::TraceContextV1],
        truncated: bool,
    ) -> Result<OperationObservation, TelemetryError> {
        self.begin_operation(operation, parent, fields, links, truncated, true)
    }

    fn begin_operation(
        self: &Arc<Self>,
        operation: ObservedOperation,
        parent: &ExecutionContext,
        fields: OperationFields<'_>,
        links: &[super::TraceContextV1],
        truncated: bool,
        trace: bool,
    ) -> Result<OperationObservation, TelemetryError> {
        if operation == ObservedOperation::ServiceWork {
            self.worker_owned
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let eligible = trace
            && !(operation == ObservedOperation::Evaluate && fields.change_count == Some(0))
            && operation
                .span_name()
                .is_some_and(|name| self.config.policy().allows_span(name));
        let eligible = eligible
            && self
                .state
                .lock()
                .map_err(|_| TelemetryError::Poisoned)?
                .active;
        let sampled = !eligible
            || self.config.context_adapter().is_some()
            || self.config.policy().sampling().decide(
                parent.trace().map(|trace| trace.sampled()),
                |policy| {
                    let super::RootSampling::Ratio(ratio) = policy else {
                        unreachable!("only ratio sampling needs a local draw");
                    };
                    let random = uuid::Uuid::new_v4();
                    let value =
                        u64::from_be_bytes(random.as_bytes()[..8].try_into().expect("UUID prefix"));
                    ratio.value() == 1.0 || (value as f64) / (u64::MAX as f64) < ratio.value()
                },
            );
        let span = if eligible && sampled {
            operation_span(
                operation,
                self.config.scope(),
                self.generation,
                &fields,
                truncated,
            )
        } else {
            Span::none()
        };
        let mut context = parent.clone();
        if !span.is_disabled() {
            if let Some(adapter) = self.config.context_adapter() {
                adapter.prepare_operation(&span, parent, links, self.config.policy().sampling())?;
                let operation_context = adapter.span_context(&span);
                // Another layer can enable a span filtered out of the trace layer.
                if !operation_context.is_empty() {
                    context = operation_context;
                }
            }
        }
        let record_metrics = !matches!(
            operation,
            ObservedOperation::Reaction | ObservedOperation::ReactionBatch
        );
        if let Some(metrics) = self.metrics.get(&operation).filter(|_| record_metrics) {
            self.update(|| metrics.calls.increment(1));
        }
        let timed = record_metrics
            && self.metrics.contains_key(&operation)
            && self
                .config
                .policy()
                .allows_metric("drasi.component.operation.duration");
        let mut scope = ExecutionScope::capture()
            .with_context(context)
            .with_adapter(self.config.context_adapter().cloned());
        if ExecutionScope::current_component().is_none() {
            if let Some(identity) = self.config.scope() {
                scope = scope.with_component(identity.clone(), self.generation);
            }
        }
        Ok(OperationObservation {
            owner: self.clone(),
            operation,
            span,
            scope,
            started: timed.then(Instant::now),
            finished: false,
            input_count: fields.input_count.unwrap_or(0),
            completion: fields.completion,
            record_metrics,
            query_timing: None,
        })
    }
}

pub(crate) struct OperationObservation {
    owner: Arc<ComponentObservations>,
    operation: ObservedOperation,
    span: Span,
    scope: ExecutionScope,
    started: Option<Instant>,
    finished: bool,
    input_count: usize,
    completion: Option<&'static str>,
    record_metrics: bool,
    query_timing: Option<(QueryPhase, bool, Instant)>,
}

impl OperationObservation {
    pub(crate) fn context(&self) -> &ExecutionContext {
        self.scope.context()
    }

    pub(crate) fn span(&self) -> Span {
        self.span.clone()
    }
}

pub(crate) struct QueryOperationObserver {
    owner: Arc<ComponentObservations>,
    phase: QueryPhase,
    detail: bool,
}

impl drasi_core::computation::QueryObserver for QueryOperationObserver {
    fn start(
        &self,
        operation: drasi_core::computation::QueryOperation,
        change_count: Option<usize>,
    ) -> Result<Box<dyn drasi_core::computation::QueryObservation>, drasi_core::interface::IndexError>
    {
        let operation = match operation {
            drasi_core::computation::QueryOperation::Evaluate => ObservedOperation::Evaluate,
            drasi_core::computation::QueryOperation::Commit => ObservedOperation::Commit,
        };
        let fields = OperationFields {
            change_count,
            ..Default::default()
        };
        let context = ExecutionContext::current();
        let observation = if self.detail {
            self.owner.begin(operation, &context, fields)
        } else {
            self.owner.begin_untraced(operation, &context, fields)
        };
        observation
            .map(|mut observation| {
                let evaluate = operation == ObservedOperation::Evaluate;
                let name = if evaluate {
                    "drasi.query.engine.duration"
                } else {
                    "drasi.query.commit.duration"
                };
                if self.owner.query.get().is_some()
                    && self.owner.config.policy().allows_metric(name)
                {
                    observation.query_timing = Some((self.phase, evaluate, Instant::now()));
                }
                Box::new(observation) as Box<dyn drasi_core::computation::QueryObservation>
            })
            .map_err(drasi_core::interface::IndexError::other)
    }
}

impl drasi_core::computation::QueryObservation for OperationObservation {
    fn in_scope(&self, work: &mut dyn FnMut()) {
        self.scope.in_scope(|| {
            let _entered = self.span.enter();
            work();
        });
    }

    fn finish(&mut self, success: bool, result_count: Option<usize>) {
        if let Some(count) = result_count {
            self.record("result_change_count", trace_count(count));
        }
        self.finish(if success { 0 } else { 1 });
    }
}

impl OperationObservation {
    // Control payloads can be large; keep their instrumentation out of caller state machines.
    pub(crate) fn run_control<T, E>(
        self,
        future: impl Future<Output = Result<T, E>>,
    ) -> std::pin::Pin<Box<impl Future<Output = Result<T, E>>>> {
        Box::pin(self.run(future))
    }

    pub(crate) fn record(&self, field: &str, value: impl tracing::Value) {
        self.span.record(field, value);
    }

    pub(crate) async fn run<T, E>(
        mut self,
        future: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let result = self
            .owner
            .scope(self.scope.scope(future.instrument(self.span.clone())))
            .await;
        self.finish(if result.is_ok() { 0 } else { 1 });
        result
    }

    pub(crate) async fn run_outputs<E>(
        mut self,
        future: impl Future<Output = Result<Vec<crate::computation::v1::OutputEnvelope>, E>>,
    ) -> Result<Vec<crate::computation::v1::OutputEnvelope>, E> {
        let mut result = self
            .owner
            .scope(self.scope.scope(future.instrument(self.span.clone())))
            .await;
        if let Ok(outputs) = &mut result {
            self.record("output_count", trace_count(outputs.len()));
            for output in outputs {
                if output.execution_context.is_empty() {
                    output.execution_context = self.context().clone();
                }
            }
        }
        self.finish(if result.is_ok() { 0 } else { 1 });
        result
    }

    pub(crate) async fn run_send(
        mut self,
        future: impl Future<
            Output = Result<
                crate::computation::v1::EnqueueReceipt,
                crate::computation::v1::SendFailure,
            >,
        >,
    ) -> Result<crate::computation::v1::EnqueueReceipt, crate::computation::v1::SendFailure> {
        self.record("acceptance", "unknown");
        let result = self
            .owner
            .scope(self.scope.scope(future.instrument(self.span.clone())))
            .await;
        self.record(
            "acceptance",
            match &result {
                Ok(_) => "accepted",
                Err(error)
                    if error.acceptance()
                        == crate::computation::v1::AcceptanceState::NotAccepted =>
                {
                    "not_accepted"
                }
                Err(_) => "unknown",
            },
        );
        self.finish(if result.is_ok() { 0 } else { 1 });
        result
    }

    pub(crate) async fn run_acceptance<T>(
        mut self,
        future: impl Future<Output = Result<T, crate::computation::v1::PipeError>>,
    ) -> Result<T, crate::computation::v1::PipeError> {
        use crate::computation::v1::PipeError;
        self.record("acceptance", "unknown");
        let result = self
            .owner
            .scope(self.scope.scope(future.instrument(self.span.clone())))
            .await;
        self.record(
            "acceptance",
            match &result {
                Ok(_) => "accepted",
                Err(PipeError::AcceptanceUnknown { .. }) => "unknown",
                Err(_) => "not_accepted",
            },
        );
        self.finish(if result.is_ok() { 0 } else { 1 });
        result
    }

    pub(crate) fn finish(&mut self, outcome: usize) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.span
            .record("outcome", ["success", "error", "cancelled"][outcome]);
        if outcome == 1 {
            self.span.record("error_kind", self.operation.error_kind());
        }
        self.owner.update(|| {
            if let Some((phase, evaluate, started)) = self.query_timing {
                if let Some(query) = self.owner.query.get() {
                    let histogram = if evaluate {
                        &query.engine[phase as usize]
                    } else {
                        &query.commit[phase as usize][outcome]
                    };
                    histogram.record(started.elapsed().as_secs_f64());
                }
            }
            if let Some(metrics) = self
                .owner
                .metrics
                .get(&self.operation)
                .filter(|_| self.record_metrics)
            {
                if let Some(started) = self.started {
                    metrics.durations[outcome].record(started.elapsed().as_secs_f64());
                }
                if outcome == 1 && self.owner.owns_operation_errors(self.operation) {
                    metrics.errors.increment(1);
                }
            }
            if outcome == 0
                && matches!(
                    self.operation,
                    ObservedOperation::Transform
                        | ObservedOperation::Handle
                        | ObservedOperation::HandleBatch
                )
            {
                if let Some(index) = self.completion.and_then(|completion| {
                    ["transformed", "accepted", "handled"]
                        .iter()
                        .position(|value| *value == completion)
                }) {
                    self.owner.inputs[index].increment(self.input_count as u64);
                }
            }
        });
    }
}

impl Drop for OperationObservation {
    fn drop(&mut self) {
        self.finish(2);
    }
}

// OpenTelemetry has signed integer attributes; u64 fields fall back to strings.
fn trace_count(count: usize) -> i64 {
    i64::try_from(count).expect("envelope/change collection lengths fit signed trace counts")
}

fn operation_span(
    operation: ObservedOperation,
    scope: Option<&TelemetryScope>,
    generation: u64,
    fields: &OperationFields<'_>,
    truncated: bool,
) -> Span {
    macro_rules! span {
        ($name:literal) => {
            tracing::info_span!(target: TELEMETRY_TARGET, parent: None, $name,
                instance_id = scope.map(TelemetryScope::instance_id),
                graph_id = scope.map(TelemetryScope::graph_id),
                component_id = scope.and_then(TelemetryScope::component_id).map(|id| id.as_str()),
                component_kind = scope.and_then(TelemetryScope::semantic_kind).map(|kind| kind.as_str()),
                role = scope.and_then(TelemetryScope::role).map(|role| role.as_str()), generation,
                operation = operation.label(), outcome = Empty, error_kind = Empty,
                input_port = fields.input_port, output_port = fields.output_port,
                input_count = fields.input_count.map(trace_count),
                output_count = fields.output_count.map(trace_count),
                change_count = fields.change_count.map(trace_count),
                result_change_count = Empty,
                completion = fields.completion, publication_mode = fields.publication_mode,
                queue_id = fields.queue_id, handling_outcome = fields.handling_outcome,
                query_id = scope.filter(|scope| scope.semantic_kind() == Some(drasi_computation_contracts::ComponentSemanticKind::Query)).and_then(TelemetryScope::component_id).map(|id| id.as_str()),
                target_count = fields.target_count.map(trace_count),
                work_kind = fields.work_kind,
                source_count = fields.source_count.map(trace_count),
                links_truncated = truncated, acceptance = Empty)
        };
    }
    match operation {
        ObservedOperation::Publish => span!("source.publish"),
        ObservedOperation::Send => span!("pipe.send"),
        ObservedOperation::Transform | ObservedOperation::Continue => span!("component.process"),
        ObservedOperation::Handle => span!("sink.handle"),
        ObservedOperation::HandleBatch => span!("sink.handle_batch"),
        ObservedOperation::Confirm => span!("component.confirm_outputs"),
        ObservedOperation::Acknowledge => span!("pipe.acknowledge"),
        ObservedOperation::Start => span!("component.start"),
        ObservedOperation::Stop => span!("component.stop"),
        ObservedOperation::Reconfigure => span!("component.reconfigure"),
        ObservedOperation::Evaluate => span!("query.evaluate"),
        ObservedOperation::Commit => span!("query.commit"),
        ObservedOperation::Control => span!("control.apply"),
        ObservedOperation::Bootstrap => span!("query.bootstrap"),
        ObservedOperation::Snapshot => span!("bootstrap.snapshot"),
        ObservedOperation::Scheduled => span!("component.scheduled"),
        ObservedOperation::Reaction => span!("reaction.process"),
        ObservedOperation::ReactionBatch => span!("reaction.process_batch"),
        ObservedOperation::ServiceWork => span!("component.process"),
        ObservedOperation::Next | ObservedOperation::Run => Span::none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::{TelemetryOptions, TelemetryProfile, TelemetrySettings};
    use drasi_computation_contracts::{ComponentId, ComponentSemanticKind};

    #[test]
    fn query_hooks_stay_unbound_when_detail_collection_is_disabled() {
        for profile in [
            TelemetryProfile::Off,
            TelemetryProfile::Basic,
            TelemetryProfile::Persistence,
            TelemetryProfile::Debug,
        ] {
            for tracing in [false, true] {
                let graph = TelemetryOptions::new(
                    TelemetrySettings::new(profile)
                        .with_metrics(false)
                        .with_tracing(tracing),
                )
                .with_instance_id("instance")
                .resolve("graph", None)
                .unwrap();
                let owner = graph
                    .observations(
                        ComponentId::try_new("query").unwrap(),
                        ComponentRole::Query,
                        ComponentSemanticKind::Query,
                        1,
                    )
                    .unwrap();
                assert_eq!(
                    owner.query_observer().is_some(),
                    profile == TelemetryProfile::Debug && tracing
                );
            }
        }
    }
}
