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
    sync::atomic::{AtomicBool, Ordering},
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
    if recovery_required.load(Ordering::Acquire) {
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
    lock: tokio::sync::Mutex<()>,
    recovery_required: AtomicBool,
}

impl ComputationTransaction {
    pub fn try_new(resources: ComputationIndexes) -> Result<Self> {
        resources.atomic_result_transaction()?;
        Ok(Self {
            resources,
            lock: tokio::sync::Mutex::new(()),
            recovery_required: AtomicBool::new(false),
        })
    }

    pub fn resources(&self) -> &ComputationIndexes {
        &self.resources
    }

    pub fn recovery_required(&self) -> bool {
        self.recovery_required.load(Ordering::Acquire)
    }

    pub async fn run<T>(&self, operation: impl Future<Output = Result<T>>) -> Result<T> {
        let _lock = self.lock.lock().await;
        in_operation(&self.resources, &self.recovery_required, true, operation).await
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
