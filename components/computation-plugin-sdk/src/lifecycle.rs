// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native lifecycle contexts and authoring helpers.
//!
//! Every [`crate::NativeComponent`] implements six required hooks: Initialize,
//! Start, Pause, Resume, Stop and Dispose. Each receives a typed context with
//! the operation identity, deadline and cancellation token. The exporter
//! rejects invalid sequences before plugin code runs.

use std::{future::Future, sync::Arc};

pub use drasi_computation_contracts::lifecycle::{
    nothing_to_do, unsupported, ApplyError, Assessment, CancellationToken, DisposeReason,
    LifecycleError, LifecycleErrorKind, LifecycleResult, OperationScope, StateIntent, StopMode,
    UpdateBoundary,
};
use tokio::{sync::watch, task::JoinSet};

use crate::ActivationCredentials;

macro_rules! scoped_context {
    ($name:ident) => {
        impl $name {
            /// Operation identity, deadline and cancellation.
            pub fn scope(&self) -> &OperationScope {
                &self.scope
            }

            /// Signalled before the host stops waiting for or drops this hook.
            pub fn cancellation(&self) -> &CancellationToken {
                self.scope.cancellation()
            }

            /// When the host stops waiting. Expiry is neither completion nor rollback.
            pub fn deadline(&self) -> Option<tokio::time::Instant> {
                self.scope.deadline()
            }
        }
    };
}

/// Context for [`crate::NativeComponent::initialize`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InitializeContext {
    scope: OperationScope,
}

/// Context for [`crate::NativeComponent::start`].
#[non_exhaustive]
pub struct StartContext {
    scope: OperationScope,
    credentials: Option<ActivationCredentials>,
    fresh_start: Option<bool>,
}

/// Context for [`crate::NativeComponent::pause`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PauseContext {
    scope: OperationScope,
}

/// Context for [`crate::NativeComponent::resume`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ResumeContext {
    scope: OperationScope,
}

/// Context for [`crate::NativeComponent::stop`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StopContext {
    scope: OperationScope,
    mode: StopMode,
}

/// Context for [`crate::NativeComponent::dispose`].
#[derive(Clone)]
#[non_exhaustive]
pub struct DisposeContext {
    scope: OperationScope,
    reason: DisposeReason,
    intent: StateIntent,
    attempt: u32,
    credentials: Option<ActivationCredentials>,
}

/// Context for [`crate::NativeManagement::apply`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ApplyContext {
    scope: OperationScope,
    boundary: UpdateBoundary,
}

scoped_context!(InitializeContext);
scoped_context!(StartContext);
scoped_context!(PauseContext);
scoped_context!(ResumeContext);
scoped_context!(StopContext);
scoped_context!(DisposeContext);
scoped_context!(ApplyContext);

impl InitializeContext {
    pub fn new(scope: OperationScope) -> Self {
        Self { scope }
    }
}

impl StartContext {
    pub fn new(scope: OperationScope, credentials: Option<ActivationCredentials>) -> Self {
        Self {
            scope,
            credentials,
            fresh_start: None,
        }
    }

    /// Report whether this is a source's very first start.
    pub fn with_fresh_start(mut self, fresh_start: Option<bool>) -> Self {
        self.fresh_start = fresh_start;
        self
    }

    /// For a source: `Some(true)` on its very first start, when no consumer
    /// has a saved live position or a completed bootstrap. The host then holds
    /// live delivery until the first bootstrap boundary, as a legacy source
    /// waits for its first bootstrap; later consumers bootstrap while live
    /// changes continue. `None` when the host does not know.
    pub fn fresh_start(&self) -> Option<bool> {
        self.fresh_start
    }

    /// Resolved activation credentials, present only when the factory declares
    /// activation credentials. Never persist them or copy them into configuration.
    pub fn credentials(&self) -> Option<&ActivationCredentials> {
        self.credentials.as_ref()
    }

    /// Takes ownership of the credentials, leaving none in the context.
    pub fn take_credentials(&mut self) -> Option<ActivationCredentials> {
        self.credentials.take()
    }

    /// A fresh scope for this activation's workers.
    pub fn activation_scope(&self) -> ActivationScope {
        ActivationScope::new()
    }
}

impl std::fmt::Debug for StartContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartContext")
            .field("scope", &self.scope)
            .field(
                "credentials",
                &self.credentials.as_ref().map(|_| "<redacted>"),
            )
            .field("fresh_start", &self.fresh_start)
            .finish()
    }
}

impl PauseContext {
    pub fn new(scope: OperationScope) -> Self {
        Self { scope }
    }
}

impl ResumeContext {
    pub fn new(scope: OperationScope) -> Self {
        Self { scope }
    }
}

impl StopContext {
    pub fn new(scope: OperationScope, mode: StopMode) -> Self {
        Self { scope, mode }
    }

    /// `Drain` completes in-flight work; `Abort` cancels promptly.
    pub fn mode(&self) -> StopMode {
        self.mode
    }
}

impl DisposeContext {
    /// Fails when the reason forbids the intent.
    pub fn new(
        scope: OperationScope,
        reason: DisposeReason,
        intent: StateIntent,
        attempt: u32,
    ) -> LifecycleResult<Self> {
        reason.validate(intent)?;
        Ok(Self {
            scope,
            reason,
            intent,
            attempt,
            credentials: None,
        })
    }

    /// Attach freshly resolved activation credentials for an `Erase` dispose.
    pub fn with_credentials(mut self, credentials: Option<ActivationCredentials>) -> Self {
        self.credentials = credentials;
        self
    }

    /// Activation credentials, resolved anew for this dispose: present only
    /// for an `Erase` dispose of a component whose factory declares
    /// activation credentials. Erasure may need to reach an external system
    /// even when the component never started in this process, so the host
    /// supplies the same credentials a start would receive. Never persist
    /// them or copy them into configuration.
    pub fn credentials(&self) -> Option<&ActivationCredentials> {
        self.credentials.as_ref()
    }

    /// Takes ownership of the credentials, leaving none in the context.
    pub fn take_credentials(&mut self) -> Option<ActivationCredentials> {
        self.credentials.take()
    }

    pub fn reason(&self) -> DisposeReason {
        self.reason
    }

    /// `Erase` permits erasing only state this component owns, using the
    /// processing and state services it was constructed with. The host keeps
    /// those services open for the duration of an `Erase` Dispose.
    pub fn intent(&self) -> StateIntent {
        self.intent
    }

    pub fn erases_owned_state(&self) -> bool {
        self.intent == StateIntent::Erase
    }

    /// One for the first attempt; retries keep the same reason and intent.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

impl std::fmt::Debug for DisposeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DisposeContext")
            .field("scope", &self.scope)
            .field("reason", &self.reason)
            .field("intent", &self.intent)
            .field("attempt", &self.attempt)
            .field(
                "credentials",
                &self.credentials.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl ApplyContext {
    pub fn new(scope: OperationScope, boundary: UpdateBoundary) -> Self {
        Self { scope, boundary }
    }

    /// The boundary that the host established before calling `apply`.
    pub fn boundary(&self) -> UpdateBoundary {
        self.boundary
    }
}

/// Owns one activation's workers.
///
/// Pause gates workers through [`PauseGate`]. `stop(Drain)` signals the drain
/// token and joins; `stop(Abort)` also aborts the tasks before joining.
pub struct ActivationScope {
    token: CancellationToken,
    paused: Arc<watch::Sender<bool>>,
    workers: JoinSet<anyhow::Result<()>>,
}

impl Default for ActivationScope {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivationScope {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            paused: Arc::new(watch::channel(false).0),
            workers: JoinSet::new(),
        }
    }

    /// Cancelled when the activation stops.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Lets a worker wait while the activation is paused.
    pub fn gate(&self) -> PauseGate {
        PauseGate(self.paused.subscribe())
    }

    pub fn spawn<F>(&mut self, worker: F)
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.workers.spawn(worker);
    }

    pub fn pause(&self) {
        self.paused.send_replace(true);
    }

    pub fn resume(&self) {
        self.paused.send_replace(false);
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.borrow()
    }

    /// Signals the token, then joins every worker. `Abort` also aborts them.
    /// Returns the first worker error; a worker aborted by `Abort` is not an error.
    pub async fn stop(&mut self, mode: StopMode) -> anyhow::Result<()> {
        self.token.cancel();
        self.paused.send_replace(false);
        if mode == StopMode::Abort {
            self.workers.abort_all();
        }
        let mut first = None;
        while let Some(result) = self.workers.join_next().await {
            let error = match result {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => error,
                Err(join) if join.is_cancelled() => continue,
                Err(join) => anyhow::anyhow!("activation worker panicked: {join}"),
            };
            first.get_or_insert(error);
        }
        first.map_or(Ok(()), Err)
    }
}

/// Observes an [`ActivationScope`]'s pause state.
#[derive(Clone)]
pub struct PauseGate(watch::Receiver<bool>);

impl PauseGate {
    /// Returns immediately while running; otherwise waits for Resume or Stop.
    pub async fn wait_running(&mut self) {
        let _ = self.0.wait_for(|paused| !*paused).await;
    }
}

/// Implements every lifecycle hook as [`nothing_to_do`].
///
/// An explicit opt-in for components that own no instance or activation work.
/// Pause succeeds immediately, so a component using this macro must not run
/// independent workers that produce output while paused.
///
/// Use it inside an `#[async_trait]` impl; it emits the boxed signatures that
/// `async_trait` produces, because attribute macros run before it expands.
#[macro_export]
macro_rules! passive_lifecycle {
    () => {
        fn initialize<'this, 'ctx, 'async_trait>(
            &'this mut self,
            _: &'ctx $crate::lifecycle::InitializeContext,
        ) -> ::std::pin::Pin<
            ::std::boxed::Box<
                dyn ::std::future::Future<Output = $crate::lifecycle::LifecycleResult>
                    + ::std::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'this: 'async_trait,
            'ctx: 'async_trait,
            Self: 'async_trait,
        {
            ::std::boxed::Box::pin(async { $crate::lifecycle::nothing_to_do() })
        }
        fn start<'this, 'ctx, 'async_trait>(
            &'this mut self,
            _: &'ctx mut $crate::lifecycle::StartContext,
        ) -> ::std::pin::Pin<
            ::std::boxed::Box<
                dyn ::std::future::Future<Output = $crate::lifecycle::LifecycleResult>
                    + ::std::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'this: 'async_trait,
            'ctx: 'async_trait,
            Self: 'async_trait,
        {
            ::std::boxed::Box::pin(async { $crate::lifecycle::nothing_to_do() })
        }
        fn pause<'this, 'ctx, 'async_trait>(
            &'this mut self,
            _: &'ctx $crate::lifecycle::PauseContext,
        ) -> ::std::pin::Pin<
            ::std::boxed::Box<
                dyn ::std::future::Future<Output = $crate::lifecycle::LifecycleResult>
                    + ::std::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'this: 'async_trait,
            'ctx: 'async_trait,
            Self: 'async_trait,
        {
            ::std::boxed::Box::pin(async { $crate::lifecycle::nothing_to_do() })
        }
        fn resume<'this, 'ctx, 'async_trait>(
            &'this mut self,
            _: &'ctx $crate::lifecycle::ResumeContext,
        ) -> ::std::pin::Pin<
            ::std::boxed::Box<
                dyn ::std::future::Future<Output = $crate::lifecycle::LifecycleResult>
                    + ::std::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'this: 'async_trait,
            'ctx: 'async_trait,
            Self: 'async_trait,
        {
            ::std::boxed::Box::pin(async { $crate::lifecycle::nothing_to_do() })
        }
        fn stop<'this, 'ctx, 'async_trait>(
            &'this mut self,
            _: &'ctx $crate::lifecycle::StopContext,
        ) -> ::std::pin::Pin<
            ::std::boxed::Box<
                dyn ::std::future::Future<Output = $crate::lifecycle::LifecycleResult>
                    + ::std::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'this: 'async_trait,
            'ctx: 'async_trait,
            Self: 'async_trait,
        {
            ::std::boxed::Box::pin(async { $crate::lifecycle::nothing_to_do() })
        }
        fn dispose<'this, 'ctx, 'async_trait>(
            &'this mut self,
            _: &'ctx $crate::lifecycle::DisposeContext,
        ) -> ::std::pin::Pin<
            ::std::boxed::Box<
                dyn ::std::future::Future<Output = $crate::lifecycle::LifecycleResult>
                    + ::std::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'this: 'async_trait,
            'ctx: 'async_trait,
            Self: 'async_trait,
        {
            ::std::boxed::Box::pin(async { $crate::lifecycle::nothing_to_do() })
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Passive(drasi_computation_contracts::ComponentDescriptor);

    #[async_trait::async_trait]
    impl crate::NativeComponent for Passive {
        fn descriptor(&self) -> &drasi_computation_contracts::ComponentDescriptor {
            &self.0
        }
        crate::passive_lifecycle!();
    }

    #[allow(dead_code)]
    fn passive_lifecycle_implements_the_trait(
        component: Passive,
    ) -> Box<dyn crate::NativeComponent> {
        Box::new(component)
    }

    #[tokio::test]
    async fn activation_scope_gates_drains_and_aborts() -> anyhow::Result<()> {
        let mut scope = ActivationScope::new();
        let token = scope.token();
        let mut gate = scope.gate();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        scope.pause();
        scope.spawn(async move {
            gate.wait_running().await;
            let _ = tx.send(());
            token.cancelled().await;
            Ok(())
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(rx.try_recv().is_err());
        scope.resume();
        rx.recv().await;
        scope.stop(StopMode::Drain).await?;

        let mut aborted = ActivationScope::new();
        aborted.spawn(std::future::pending());
        aborted.stop(StopMode::Abort).await?;
        Ok(())
    }

    #[test]
    fn dispose_context_rejects_forbidden_intent() {
        let scope = OperationScope::new("op", "c", 1, None, CancellationToken::new());
        assert!(DisposeContext::new(
            scope.clone(),
            DisposeReason::Shutdown,
            StateIntent::Erase,
            1
        )
        .is_err());
        assert!(DisposeContext::new(scope, DisposeReason::Removal, StateIntent::Erase, 1).is_ok());
    }
}
