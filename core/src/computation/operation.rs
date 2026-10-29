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

use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};

use super::{ComputationIndexes, ComputationQueryError, Result};

struct PendingOperation<'a> {
    resources: &'a ComputationIndexes,
    recovery_required: &'a AtomicBool,
    completed: bool,
}

impl Drop for PendingOperation<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.recovery_required.store(true, Ordering::Release);
            if let Some(cleanup) = self.resources.cleanup() {
                cleanup.cancel();
            } else if let Err(error) = self.resources.indexes().session_control.rollback() {
                log::error!("Cancelled computation session rollback failed: {error}");
            }
        }
    }
}

fn rollback_failure(
    resources: &ComputationIndexes,
    recovery_required: &AtomicBool,
    failure: ComputationQueryError,
) -> ComputationQueryError {
    match resources.indexes().session_control.rollback() {
        Ok(()) => failure,
        Err(rollback) => {
            recovery_required.store(true, Ordering::Release);
            ComputationQueryError::Rollback {
                failure: Box::new(failure),
                rollback,
            }
        }
    }
}

/// The caller must serialize access to the bundle, including shutdown.
pub(crate) async fn in_operation<T>(
    resources: &ComputationIndexes,
    recovery_required: &AtomicBool,
    atomic: bool,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    if recovery_required.load(Ordering::Acquire)
        || resources
            .group_session
            .as_ref()
            .is_some_and(|group| group.recovery_required())
    {
        return Err(ComputationQueryError::RecoveryRequired);
    }
    let mut pending = PendingOperation {
        resources,
        recovery_required,
        completed: false,
    };
    let session = &resources.indexes().session_control;
    let result = match session.begin().await {
        Ok(()) => match operation.await {
            Ok(value) => match session.commit().await {
                Ok(()) => Ok(value),
                Err(error) => {
                    recovery_required.store(true, Ordering::Release);
                    Err(rollback_failure(resources, recovery_required, error.into()))
                }
            },
            Err(error) => {
                if !atomic {
                    recovery_required.store(true, Ordering::Release);
                }
                Err(rollback_failure(resources, recovery_required, error))
            }
        },
        Err(error) => {
            recovery_required.store(true, Ordering::Release);
            Err(rollback_failure(resources, recovery_required, error.into()))
        }
    };
    pending.completed = true;
    result
}

/// Serialized, cancellation-safe index transactions without a query evaluator.
///
/// Requires a complete atomic bundle. An interrupted operation or uncertain
/// commit fences this owner. After dropping/awaiting the operation, call
/// [`Self::shutdown`] to join provider I/O before opening a replacement.
pub struct ComputationTransaction {
    resources: ComputationIndexes,
    lock: Arc<tokio::sync::Mutex<()>>,
    retirement: Mutex<Weak<RetirementGate>>,
    recovery_required: AtomicBool,
    atomic: bool,
}

impl ComputationTransaction {
    pub fn try_new(resources: ComputationIndexes) -> Result<Self> {
        resources.atomic_result_transaction()?;
        Ok(Self::for_query(resources))
    }

    pub(crate) fn for_query(resources: ComputationIndexes) -> Self {
        Self {
            atomic: resources.atomic_result_transaction().is_ok(),
            resources,
            lock: Arc::new(tokio::sync::Mutex::new(())),
            retirement: Mutex::new(Weak::new()),
            recovery_required: AtomicBool::new(false),
        }
    }

    pub fn resources(&self) -> &ComputationIndexes {
        &self.resources
    }

    /// Hold this actual standalone owner's operation gate through a decision.
    /// Shared members require their group's gate instead.
    pub fn freeze_for_retirement(self: &Arc<Self>) -> Result<ComputationTransactionRetirement> {
        if !self.atomic || self.resources.group_session.is_some() {
            return Err(ComputationQueryError::TransactionMismatch);
        }
        let gate = self
            .lock
            .clone()
            .try_lock_owned()
            .map_err(|_| ComputationQueryError::OperationInProgress)?;
        if self.recovery_required() {
            return Err(ComputationQueryError::RecoveryRequired);
        }
        let gate = Arc::new(Mutex::new(Some(gate)));
        *self.retirement.lock().map_err(|_| retirement_poisoned())? = Arc::downgrade(&gate);
        if self.recovery_required() {
            return Err(ComputationQueryError::RecoveryRequired);
        }
        Ok(ComputationTransactionRetirement {
            owner: self.clone(),
            _gate: gate,
            resolved: false,
        })
    }

    /// Revoke this owner's retirement lease before terminal resource cleanup.
    /// A revoked owner stays fenced even if the decision later resumes its guard.
    pub fn cancel_retirement(&self) -> Result<()> {
        let retirement = self
            .retirement
            .lock()
            .map_err(|_| retirement_poisoned())?
            .upgrade();
        if let Some(retirement) = retirement {
            self.recovery_required.store(true, Ordering::Release);
            retirement.lock().map_err(|_| retirement_poisoned())?.take();
        }
        Ok(())
    }

    pub fn recovery_required(&self) -> bool {
        self.recovery_required.load(Ordering::Acquire)
            || self
                .resources
                .group_session
                .as_ref()
                .is_some_and(|group| group.recovery_required())
    }

    pub fn shares_transaction_group(&self, other: &Self) -> bool {
        match (
            &self.resources.group_session,
            &other.resources.group_session,
        ) {
            (Some(left), Some(right)) => {
                left.same_group(right) && !self.recovery_required() && !other.recovery_required()
            }
            _ => false,
        }
    }

    /// Wake shared-service waiters when their member or group is fenced.
    /// Ordinary transactions reject this service rather than creating a watcher.
    pub async fn wait_for_transaction_group_failure(&self) -> Result<()> {
        let group = self
            .resources
            .group_session
            .as_ref()
            .ok_or(ComputationQueryError::TransactionMismatch)?;
        group.wait_for_failure().await;
        Ok(())
    }

    async fn lock_operation(&self) -> Result<tokio::sync::MutexGuard<'_, ()>> {
        let lock = self.lock.lock();
        match &self.resources.group_session {
            Some(group) => {
                // Shared failure observation must not enlarge standalone operations.
                Box::pin(async {
                    tokio::select! {
                        biased;
                        _ = group.wait_for_failure() => Err(ComputationQueryError::RecoveryRequired),
                        lock = lock => Ok(lock),
                    }
                })
                .await
            }
            None => Ok(lock.await),
        }
    }

    pub async fn run<T>(&self, operation: impl Future<Output = Result<T>>) -> Result<T> {
        let _lock = self.lock_operation().await?;
        in_operation(
            &self.resources,
            &self.recovery_required,
            self.atomic,
            operation,
        )
        .await
    }

    pub async fn run_with_mutations<T>(
        &self,
        operation: impl Future<Output = Result<(T, Vec<Box<dyn super::TransactionGroupMutation>>)>>,
    ) -> Result<T> {
        let group = self
            .resources
            .group_session
            .as_ref()
            .ok_or(ComputationQueryError::TransactionMismatch)?;
        self.run(async {
            let (value, mutations) = operation.await?;
            group.stage_mutations(mutations).await?;
            Ok(value)
        })
        .await
    }

    pub(crate) async fn inspect<T>(&self, operation: impl Future<Output = Result<T>>) -> Result<T> {
        let _lock = self.lock_operation().await?;
        if self.recovery_required() {
            return Err(ComputationQueryError::RecoveryRequired);
        }
        operation.await
    }

    pub async fn quiesce(&self) -> Result<()> {
        let _lock = self
            .lock
            .try_lock()
            .map_err(|_| ComputationQueryError::OperationInProgress)?;
        if self.recovery_required() {
            return Err(ComputationQueryError::RecoveryRequired);
        }
        if let Some(cleanup) = self.resources.cleanup() {
            if let Err(error) = cleanup.quiesce().await {
                self.recovery_required.store(true, Ordering::Release);
                return Err(error.into());
            }
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.cancel_retirement()?;
        let _lock = self
            .lock
            .try_lock()
            .map_err(|_| ComputationQueryError::OperationInProgress)?;
        self.recovery_required.store(true, Ordering::Release);
        if let Some(cleanup) = self.resources.cleanup() {
            cleanup.cancel();
            cleanup.shutdown().await?;
        }
        self.resources.indexes().session_control.rollback()?;
        Ok(())
    }
}

impl Drop for ComputationTransaction {
    fn drop(&mut self) {
        if let Some(cleanup) = self.resources.cleanup() {
            cleanup.cancel();
        }
    }
}

type RetirementGate = Mutex<Option<tokio::sync::OwnedMutexGuard<()>>>;

fn retirement_poisoned() -> ComputationQueryError {
    crate::interface::IndexError::other(std::io::Error::other(
        "standalone transaction retirement ownership is poisoned",
    ))
    .into()
}

/// A live operation gate, not an empty-state sample or a persisted migration.
#[must_use]
pub struct ComputationTransactionRetirement {
    owner: Arc<ComputationTransaction>,
    _gate: Arc<RetirementGate>,
    resolved: bool,
}

impl ComputationTransactionRetirement {
    pub async fn has_scheduled_work(&self) -> Result<bool> {
        if self.owner.recovery_required() {
            return Err(ComputationQueryError::RecoveryRequired);
        }
        let pending = self
            .owner
            .resources
            .indexes()
            .future_queue
            .peek_due_time()
            .await?;
        if self.owner.recovery_required() {
            return Err(ComputationQueryError::RecoveryRequired);
        }
        Ok(pending.is_some())
    }

    /// Resume only after definite rejection of the proposed configuration.
    pub fn resume(mut self) {
        self.resolved = true;
    }

    /// Fence the old owner after authoritative acceptance.
    pub fn retire(mut self) {
        self.owner.recovery_required.store(true, Ordering::Release);
        self.resolved = true;
    }
}

impl Drop for ComputationTransactionRetirement {
    fn drop(&mut self) {
        if !self.resolved {
            self.owner.recovery_required.store(true, Ordering::Release);
        }
    }
}
