// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{ExecutionContext, Result, TelemetryScope, TraceContextV1};
use pin_project_lite::pin_project;
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// A local backend attachment. It is dropped before the next future is polled.
pub trait TraceContextGuard {}

/// Host-selected context integration; no exporter, recorder or global installation.
pub trait TraceContextAdapter: std::fmt::Debug + Send + Sync {
    fn enter(&self, context: &ExecutionContext) -> Box<dyn TraceContextGuard>;
    fn current_context(&self) -> ExecutionContext;
    fn span_context(&self, span: &tracing::Span) -> ExecutionContext;
    fn set_parent(&self, span: &tracing::Span, parent: &ExecutionContext) -> Result<()>;
    fn add_link(&self, span: &tracing::Span, context: &TraceContextV1) -> Result<()>;

    /// Called before context extraction or the first span entry.
    fn prepare_operation(
        &self,
        span: &tracing::Span,
        parent: &ExecutionContext,
        links: &[TraceContextV1],
        sampling: super::RootSampling,
    ) -> Result<()> {
        if sampling != super::RootSampling::Always {
            return Err(super::TelemetryError::Unsupported(
                "configured operation root sampling",
            ));
        }
        self.set_parent(span, parent)?;
        for link in links {
            self.add_link(span, link)?;
        }
        Ok(())
    }
}

/// The receiver's host-assigned identity, independent of the sending trace.
#[derive(Debug, Clone)]
pub struct ComponentExecutionScope {
    pub identity: TelemetryScope,
    pub generation: u64,
}

/// Local polling state. Only its `ExecutionContext` belongs on a worker queue.
#[derive(Debug, Clone)]
pub struct ExecutionScope {
    context: ExecutionContext,
    component: Option<Arc<ComponentExecutionScope>>,
    adapter: Option<Arc<dyn TraceContextAdapter>>,
    dispatch: tracing::Dispatch,
    transport_required: bool,
    timing_transport: Option<Arc<dyn super::HandledBranchTransport>>,
    timing_lifetime: Option<super::TelemetryLifetime>,
    #[cfg(feature = "metrics-facade")]
    plugin_telemetry: Option<super::PluginTelemetry>,
    #[cfg(feature = "metrics-facade")]
    disabled_plugin_metrics: bool,
}

thread_local! {
    static CURRENT: RefCell<Option<ExecutionScope>> = const { RefCell::new(None) };
}

impl ExecutionScope {
    pub fn new(context: ExecutionContext) -> Self {
        Self {
            context,
            component: None,
            adapter: None,
            dispatch: tracing::dispatcher::get_default(Clone::clone),
            transport_required: false,
            timing_transport: None,
            timing_lifetime: None,
            #[cfg(feature = "metrics-facade")]
            plugin_telemetry: None,
            #[cfg(feature = "metrics-facade")]
            disabled_plugin_metrics: false,
        }
    }

    pub fn with_component(mut self, identity: TelemetryScope, generation: u64) -> Self {
        self.component = Some(Arc::new(ComponentExecutionScope {
            identity,
            generation,
        }));
        self
    }

    pub fn with_timing_transport(
        mut self,
        transport: Option<Arc<dyn super::HandledBranchTransport>>,
    ) -> Self {
        self.timing_transport = transport;
        self
    }

    pub fn current_timing_transport() -> Option<Arc<dyn super::HandledBranchTransport>> {
        CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .and_then(|scope| scope.timing_transport.clone())
        })
    }

    pub fn with_timing_lifetime(mut self, lifetime: Option<super::TelemetryLifetime>) -> Self {
        self.timing_lifetime = lifetime;
        self
    }

    pub fn current_timing_lifetime() -> Option<super::TelemetryLifetime> {
        CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .and_then(|scope| scope.timing_lifetime.clone())
        })
    }

    #[cfg(feature = "metrics-facade")]
    pub fn with_plugin_telemetry(mut self, binding: Option<super::PluginTelemetry>) -> Self {
        self.plugin_telemetry = binding;
        self
    }
    #[cfg(feature = "metrics-facade")]
    pub fn has_plugin_telemetry(&self) -> bool {
        self.plugin_telemetry.is_some()
    }
    #[cfg(feature = "metrics-facade")]
    pub fn with_disabled_plugin_metrics(mut self, disabled: bool) -> Self {
        self.disabled_plugin_metrics = disabled;
        self
    }
    #[cfg(feature = "metrics-facade")]
    pub fn plugin_metrics_disabled() -> bool {
        CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .is_some_and(|scope| scope.disabled_plugin_metrics)
        })
    }

    #[cfg(feature = "metrics-facade")]
    pub fn current_plugin_telemetry() -> Option<super::PluginTelemetry> {
        CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .and_then(|scope| scope.plugin_telemetry.clone())
        })
    }

    /// The supplied carrier, without consulting an adapter or recursing through
    /// the current native subscriber.
    pub fn supplied_context() -> ExecutionContext {
        CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .map(|scope| scope.context.clone())
                .unwrap_or_default()
        })
    }

    pub fn with_adapter(mut self, adapter: Option<Arc<dyn TraceContextAdapter>>) -> Self {
        self.adapter = adapter;
        self
    }

    pub fn with_context(&self, context: ExecutionContext) -> Self {
        Self {
            context,
            ..self.clone()
        }
    }

    pub fn context(&self) -> &ExecutionContext {
        &self.context
    }

    pub fn has_adapter(&self) -> bool {
        self.adapter.is_some()
    }

    pub fn with_required_transport(mut self, required: bool) -> Self {
        self.transport_required = required;
        self
    }

    pub fn transport_required(&self) -> bool {
        self.transport_required
    }

    /// Capture the local binding and current portable context before spawning.
    pub fn capture() -> Self {
        let current = CURRENT.with(|current| current.borrow().clone());
        match current {
            Some(scope) => {
                let context = scope.capture_context();
                scope.with_context(context)
            }
            None => Self::new(ExecutionContext::default()),
        }
    }

    /// Direct component calls have no graph scope. An existing operation scope,
    /// including an explicitly empty one, remains authoritative.
    pub fn for_input(context: &ExecutionContext) -> Self {
        if CURRENT.with(|current| current.borrow().is_some()) {
            Self::capture()
        } else {
            Self::new(context.clone())
        }
    }

    pub fn current_component() -> Option<Arc<ComponentExecutionScope>> {
        CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .and_then(|scope| scope.component.clone())
        })
    }

    pub fn capture_context(&self) -> ExecutionContext {
        self.adapter
            .as_ref()
            .map(|adapter| adapter.current_context())
            .filter(|context| context.trace().is_some())
            .map(|context| self.context.clone().with_trace_from(&context))
            .unwrap_or_else(|| self.context.clone())
    }

    /// Also suitable for `spawn_owned_blocking_worker`; the guard never escapes.
    pub fn in_scope<T>(&self, work: impl FnOnce() -> T) -> T {
        let _entered = self.enter();
        work()
    }

    pub fn scope<F: Future>(&self, future: F) -> ScopedExecution<F> {
        ScopedExecution {
            scope: self.clone(),
            future: Some(future),
        }
    }

    fn enter(&self) -> EnteredScope {
        let dispatch = tracing::dispatcher::set_default(&self.dispatch);
        let previous = CURRENT.with(|current| current.replace(Some(self.clone())));
        let mut entered = EnteredScope {
            previous,
            backend: None,
            _dispatch: dispatch,
        };
        entered.backend = self
            .adapter
            .as_ref()
            .map(|adapter| adapter.enter(&self.context));
        entered
    }
}

impl ExecutionContext {
    pub fn current() -> Self {
        ExecutionScope::capture().context
    }
}

struct EnteredScope {
    previous: Option<ExecutionScope>,
    backend: Option<Box<dyn TraceContextGuard>>,
    _dispatch: tracing::dispatcher::DefaultGuard,
}

impl Drop for EnteredScope {
    fn drop(&mut self) {
        let backend = self.backend.take();
        CURRENT.with(|current| {
            current.replace(self.previous.take());
        });
        drop(backend);
    }
}

pin_project! {
    pub struct ScopedExecution<F> {
        scope: ExecutionScope,
        #[pin]
        future: Option<F>,
    }

    impl<F> PinnedDrop for ScopedExecution<F> {
        fn drop(this: Pin<&mut Self>) {
            let mut this = this.project();
            let _entered = this.scope.enter();
            this.future.set(None);
        }
    }
}

impl<F: Future> Future for ScopedExecution<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let _entered = this.scope.enter();
        this.future.as_pin_mut().expect("scoped future").poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn context(id: u8) -> ExecutionContext {
        ExecutionContext::new(TraceContextV1::new([id; 16], [id; 8], 1, None).unwrap())
    }

    async fn interleave() {
        let one = context(1);
        let two = context(2);
        let first = ExecutionScope::new(one.clone());
        let second = ExecutionScope::new(two.clone());
        let work = |expected: ExecutionContext| async move {
            assert_eq!(ExecutionContext::current(), expected);
            tokio::task::yield_now().await;
            assert_eq!(ExecutionContext::current(), expected);
            let worker_scope = ExecutionScope::capture();
            let worker = tokio::spawn(worker_scope.scope(async {
                tokio::task::yield_now().await;
                ExecutionContext::current()
            }));
            assert_eq!(worker.await.unwrap(), expected);
            let blocking_scope = ExecutionScope::capture();
            assert_eq!(
                tokio::task::spawn_blocking(move || {
                    blocking_scope.in_scope(ExecutionContext::current)
                })
                .await
                .unwrap(),
                expected
            );
            ExecutionScope::new(ExecutionContext::default())
                .scope(async {
                    assert!(ExecutionContext::current().is_empty());
                    tokio::task::yield_now().await;
                    assert!(ExecutionContext::current().is_empty());
                })
                .await;
            assert_eq!(ExecutionContext::current(), expected);
        };
        tokio::join!(first.scope(work(one)), second.scope(work(two)));
        assert!(ExecutionContext::current().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scoped_context_current_thread() {
        interleave().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_context_multi_thread() {
        interleave().await;
    }

    #[tokio::test]
    async fn cancelled_future_is_dropped_in_its_scope_and_restores_the_caller() {
        struct ObserveDrop(Arc<Mutex<Option<ExecutionContext>>>);
        impl Drop for ObserveDrop {
            fn drop(&mut self) {
                *self.0.lock().unwrap() = Some(ExecutionContext::current());
            }
        }
        let observed = Arc::new(Mutex::new(None));
        let scope = ExecutionScope::new(context(3));
        let dropped = ObserveDrop(observed.clone());
        let mut future = Box::pin(scope.scope(async move {
            let _dropped = dropped;
            std::future::pending::<()>().await;
        }));
        assert!(futures::poll!(future.as_mut()).is_pending());
        assert!(ExecutionContext::current().is_empty());
        drop(future);
        assert_eq!(*observed.lock().unwrap(), Some(context(3)));
        assert!(ExecutionContext::current().is_empty());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scope.in_scope(|| panic!("test unwind"));
        }));
        assert!(panic.is_err());
        assert!(ExecutionContext::current().is_empty());
    }

    #[test]
    fn adapter_panics_restore_the_previous_scope() {
        #[derive(Debug)]
        struct PanickingAdapter(bool);
        struct PanickingGuard;
        impl TraceContextGuard for PanickingGuard {}
        impl Drop for PanickingGuard {
            fn drop(&mut self) {
                panic!("test adapter detach");
            }
        }
        impl TraceContextAdapter for PanickingAdapter {
            fn enter(&self, _: &ExecutionContext) -> Box<dyn TraceContextGuard> {
                assert!(!self.0, "test adapter attach");
                Box::new(PanickingGuard)
            }
            fn current_context(&self) -> ExecutionContext {
                ExecutionContext::default()
            }
            fn span_context(&self, _: &tracing::Span) -> ExecutionContext {
                ExecutionContext::default()
            }
            fn set_parent(&self, _: &tracing::Span, _: &ExecutionContext) -> Result<()> {
                unreachable!()
            }
            fn add_link(&self, _: &tracing::Span, _: &TraceContextV1) -> Result<()> {
                unreachable!()
            }
        }
        ExecutionScope::new(context(1)).in_scope(|| {
            for attaching in [true, false] {
                let scope = ExecutionScope::new(context(2))
                    .with_adapter(Some(Arc::new(PanickingAdapter(attaching))));
                assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    scope.in_scope(|| ());
                }))
                .is_err());
                assert_eq!(ExecutionContext::current(), context(1));
            }
        });
        assert!(ExecutionContext::current().is_empty());
    }
}
