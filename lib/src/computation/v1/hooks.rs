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

#![deny(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! Lifecycle hook contexts for in-process graph components.
//!
//! Every [`super::ComputationComponent`] implements six required hooks:
//! Initialize, Start, Pause, Resume, Stop and Dispose. Each receives a typed
//! context carrying the operation identity, deadline and cancellation token.
//! These contexts are DrasiLib's own; the native host proxy translates them
//! into native plugin operations.

pub use drasi_computation_contracts::lifecycle::{
    lifecycle_error_kind, nothing_to_do, unsupported, ApplyError, Assessment, CancellationToken,
    DisposeReason, LifecycleError, LifecycleErrorKind, LifecycleResult, OperationScope,
    StateIntent, StopMode, UpdateBoundary,
};

macro_rules! scoped_context {
    ($name:ident) => {
        impl $name {
            /// Operation identity, deadline and cancellation.
            pub fn scope(&self) -> &OperationScope {
                &self.scope
            }

            /// Signalled when the graph stops waiting for this hook. The token
            /// belongs to this operation only; do not bind activation work to it.
            pub fn cancellation(&self) -> &CancellationToken {
                self.scope.cancellation()
            }

            /// When the graph stops waiting. Expiry is neither completion nor rollback.
            pub fn deadline(&self) -> Option<tokio::time::Instant> {
                self.scope.deadline()
            }
        }
    };
}

/// Context for [`super::ComputationComponent::initialize`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InitializeContext {
    scope: OperationScope,
}

/// Context for [`super::ComputationComponent::start`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StartContext {
    scope: OperationScope,
}

/// Context for [`super::ComputationComponent::pause`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PauseContext {
    scope: OperationScope,
}

/// Context for [`super::ComputationComponent::resume`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ResumeContext {
    scope: OperationScope,
}

/// Context for [`super::ComputationComponent::stop`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StopContext {
    scope: OperationScope,
    mode: StopMode,
}

/// Context for [`super::ComputationComponent::dispose`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DisposeContext {
    scope: OperationScope,
    reason: DisposeReason,
    intent: StateIntent,
    attempt: u32,
}

/// Context for [`super::ComponentManagement::apply`].
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
    /// Context for one Initialize operation.
    pub fn new(scope: OperationScope) -> Self {
        Self { scope }
    }
}

impl StartContext {
    /// Context for one Start operation.
    pub fn new(scope: OperationScope) -> Self {
        Self { scope }
    }
}

impl PauseContext {
    /// Context for one Pause operation.
    pub fn new(scope: OperationScope) -> Self {
        Self { scope }
    }
}

impl ResumeContext {
    /// Context for one Resume operation.
    pub fn new(scope: OperationScope) -> Self {
        Self { scope }
    }
}

impl StopContext {
    /// Context for one Stop operation.
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
        })
    }

    /// Why the object is being retired.
    pub fn reason(&self) -> DisposeReason {
        self.reason
    }

    /// `Erase` permits erasing only state this component generation owns.
    pub fn intent(&self) -> StateIntent {
        self.intent
    }

    /// Whether owned state must be erased before releasing the object.
    pub fn erases_owned_state(&self) -> bool {
        self.intent == StateIntent::Erase
    }

    /// One for the first attempt; retries keep the same reason and intent, so
    /// erasure must tolerate being repeated.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

impl ApplyContext {
    /// Context for one accepted update at an established boundary.
    pub fn new(scope: OperationScope, boundary: UpdateBoundary) -> Self {
        Self { scope, boundary }
    }

    /// The boundary that the graph established before calling `apply`.
    pub fn boundary(&self) -> UpdateBoundary {
        self.boundary
    }
}

/// Implements every [`super::ComputationComponent`] lifecycle hook as a
/// successful no-op, except that Dispose refuses to erase owned state.
///
/// An explicit opt-in for components that own no instance or activation work.
/// Pause succeeds immediately, so a component using this macro must not run
/// independent workers that produce output while paused.
///
/// Use it inside an `#[async_trait]` impl; it emits the boxed signatures that
/// `async_trait` produces, because attribute macros run before it expands.
#[macro_export]
macro_rules! passive_component_lifecycle {
    () => {
        $crate::passive_component_lifecycle!(@hook initialize, InitializeContext);
        $crate::passive_component_lifecycle!(@hook start, StartContext);
        $crate::passive_component_lifecycle!(@hook pause, PauseContext);
        $crate::passive_component_lifecycle!(@hook resume, ResumeContext);
        $crate::passive_component_lifecycle!(@hook stop, StopContext);
        fn dispose<'this, 'ctx, 'async_trait>(
            &'this mut self,
            context: &'ctx $crate::computation::v1::DisposeContext,
        ) -> ::std::pin::Pin<
            ::std::boxed::Box<
                dyn ::std::future::Future<Output = ::anyhow::Result<()>>
                    + ::std::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'this: 'async_trait,
            'ctx: 'async_trait,
            Self: 'async_trait,
        {
            let erase = context.erases_owned_state();
            ::std::boxed::Box::pin(async move {
                if erase {
                    ::anyhow::bail!("this component does not support permanent state removal");
                }
                ::std::result::Result::Ok(())
            })
        }
    };
    (@hook $hook:ident, $context:ident) => {
        fn $hook<'this, 'ctx, 'async_trait>(
            &'this mut self,
            _: &'ctx $crate::computation::v1::$context,
        ) -> ::std::pin::Pin<
            ::std::boxed::Box<
                dyn ::std::future::Future<Output = ::anyhow::Result<()>>
                    + ::std::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'this: 'async_trait,
            'ctx: 'async_trait,
            Self: 'async_trait,
        {
            ::std::boxed::Box::pin(async { ::std::result::Result::Ok(()) })
        }
    };
}
