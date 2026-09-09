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
        Arc,
    },
};

use crate::{
    computation::{
        AtomicResultTransaction, ComputationIndexes, ComputationQueryError,
        ComputationResourceCleanup, Result,
    },
    evaluation::{context::QueryPartEvaluationContext, EvaluationError},
    interface::{FutureQueue, IndexError, SessionControl},
    models::SourceChange,
    query::QueryBuilder,
};

use super::{ContinuousQuery, DueFutureResult};

/// Query-local execution for the parallel graph, using the unchanged evaluator.
///
/// Construction consumes a graph resource bundle and binds every evaluator index
/// through the existing builder. An arbitrary prebuilt legacy query cannot be
/// paired with unrelated indexes or a descriptive transaction token.
pub struct ComputationQuery {
    inner: ContinuousQuery,
    resources: ComputationIndexes,
    recovery_required: AtomicBool,
    atomic_output: bool,
}

struct PendingEvaluation<'a> {
    recovery_required: &'a AtomicBool,
    session: Arc<dyn SessionControl>,
    cleanup: Option<&'a Arc<dyn ComputationResourceCleanup>>,
    completed: bool,
}

impl Drop for PendingEvaluation<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.recovery_required.store(true, Ordering::Release);
            if let Some(cleanup) = self.cleanup {
                cleanup.cancel();
            } else if let Err(error) = self.session.rollback() {
                log::error!("Cancelled computation session rollback failed: {error}");
            }
        }
    }
}

impl ComputationQuery {
    pub async fn try_build(builder: QueryBuilder, resources: ComputationIndexes) -> Result<Self> {
        let set = resources.indexes();
        let inner = builder
            .with_element_index(set.element_index.clone())
            .with_archive_index(set.archive_index.clone())
            .with_result_index(set.result_index.clone())
            .with_future_queue(set.future_queue.clone())
            .with_session_control(set.session_control.clone())
            .try_build()
            .await?;
        if !Arc::ptr_eq(&inner.session_control, &set.session_control)
            || !Arc::ptr_eq(&inner.element_index, &set.element_index)
        {
            return Err(ComputationQueryError::TransactionMismatch);
        }
        Ok(Self {
            inner,
            atomic_output: resources.atomic_result_transaction().is_ok(),
            resources,
            recovery_required: AtomicBool::new(false),
        })
    }

    pub fn resources(&self) -> &ComputationIndexes {
        &self.resources
    }

    /// Includes the builder's existing shadow queue; no spawned timer/consumer.
    pub fn future_queue(&self) -> Arc<dyn FutureQueue> {
        self.inner.future_queue.clone()
    }

    /// Dropped evaluation, failed commit/rollback, or failed non-atomic processing
    /// can leave uncertain state. The owner must recover a replacement rather
    /// than reuse this constructed query.
    pub fn recovery_required(&self) -> bool {
        self.recovery_required.load(Ordering::Acquire)
    }

    /// Await provider-registered work before retiring this query's resources.
    /// This seals the constructed query; it is not a restart or a rollback claim.
    pub async fn shutdown(&self) -> Result<()> {
        let _lock = self
            .inner
            .change_lock
            .try_lock()
            .map_err(|_| ComputationQueryError::OperationInProgress)?;
        self.recovery_required.store(true, Ordering::Release);
        if let Some(cleanup) = self.resources.cleanup() {
            cleanup.cancel();
            cleanup.shutdown().await?;
        }
        self.inner.session_control.rollback()?;
        Ok(())
    }

    async fn in_operation<T>(&self, operation: impl Future<Output = Result<T>>) -> Result<T> {
        let _lock = self.inner.change_lock.lock().await;
        if self.recovery_required() {
            return Err(ComputationQueryError::RecoveryRequired);
        }
        let mut pending = PendingEvaluation {
            recovery_required: &self.recovery_required,
            session: self.inner.session_control.clone(),
            cleanup: self.resources.cleanup(),
            completed: false,
        };
        let result = match self.inner.session_control.begin().await {
            Ok(()) => match operation.await {
                Ok(value) => match self.inner.session_control.commit().await {
                    Ok(()) => Ok(value),
                    Err(error) => {
                        self.recovery_required.store(true, Ordering::Release);
                        Err(self.rollback_failure(error.into()))
                    }
                },
                Err(error) => {
                    if !self.atomic_output {
                        self.recovery_required.store(true, Ordering::Release);
                    }
                    Err(self.rollback_failure(error))
                }
            },
            Err(error) => {
                self.recovery_required.store(true, Ordering::Release);
                Err(self.rollback_failure(error.into()))
            }
        };
        pending.completed = true;
        result
    }

    fn rollback_failure(&self, failure: ComputationQueryError) -> ComputationQueryError {
        match self.inner.session_control.rollback() {
            Ok(()) => failure,
            Err(rollback) => {
                self.recovery_required.store(true, Ordering::Release);
                ComputationQueryError::Rollback {
                    failure: Box::new(failure),
                    rollback,
                }
            }
        }
    }

    async fn evaluate_source_change(
        &self,
        change: SourceChange,
    ) -> Result<Vec<QueryPartEvaluationContext>> {
        let changes = self
            .inner
            .execute_source_middleware(change)
            .await
            .map_err(EvaluationError::from)?;
        Ok(self.inner.process_changes_inner(changes).await?)
    }

    pub async fn process_source_change(
        &self,
        change: SourceChange,
    ) -> Result<Vec<QueryPartEvaluationContext>> {
        self.in_operation(self.evaluate_source_change(change)).await
    }

    /// Result-aware staging without a complete output atomicity claim.
    /// Rollback and cancellation safety remain properties of the supplied resources.
    pub async fn process_source_change_with_non_atomic_result_hook<F, Fut>(
        &self,
        change: SourceChange,
        pre_commit_hook: F,
    ) -> Result<Arc<[QueryPartEvaluationContext]>>
    where
        F: FnOnce(Arc<[QueryPartEvaluationContext]>) -> Fut + Send,
        Fut: Future<Output = std::result::Result<(), IndexError>> + Send,
    {
        self.in_operation(async {
            let results: Arc<[QueryPartEvaluationContext]> =
                self.evaluate_source_change(change).await?.into();
            pre_commit_hook(results.clone()).await?;
            Ok(results)
        })
        .await
    }

    /// Stage results while the same query/index session is still uncommitted.
    /// Capability mismatch is rejected before middleware, evaluation, or the hook.
    pub async fn process_source_change_with_result_hook<F, Fut>(
        &self,
        change: SourceChange,
        transaction: &AtomicResultTransaction,
        pre_commit_hook: F,
    ) -> Result<Arc<[QueryPartEvaluationContext]>>
    where
        F: FnOnce(Arc<[QueryPartEvaluationContext]>) -> Fut + Send,
        Fut: Future<Output = std::result::Result<(), IndexError>> + Send,
    {
        self.validate_transaction(transaction)?;
        self.process_source_change_with_non_atomic_result_hook(change, pre_commit_hook)
            .await
    }

    pub async fn process_due_futures(&self) -> Result<Option<Arc<DueFutureResult>>> {
        self.process_due_futures_with_non_atomic_result_hook(|_| async { Ok(()) })
            .await
    }

    /// Process one due item using its original source provenance. An empty queue
    /// commits an empty session without invoking the hook.
    pub async fn process_due_futures_with_non_atomic_result_hook<F, Fut>(
        &self,
        pre_commit_hook: F,
    ) -> Result<Option<Arc<DueFutureResult>>>
    where
        F: FnOnce(Arc<DueFutureResult>) -> Fut + Send,
        Fut: Future<Output = std::result::Result<(), IndexError>> + Send,
    {
        self.in_operation(async {
            let Some(future_ref) = self.inner.future_queue.pop().await? else {
                return Ok(None);
            };
            let source_id = future_ref.element_ref.source_id.clone();
            let results = Arc::new(DueFutureResult {
                results: self
                    .evaluate_source_change(SourceChange::Future { future_ref })
                    .await?,
                source_id,
            });
            pre_commit_hook(results.clone()).await?;
            Ok(Some(results))
        })
        .await
    }

    pub async fn process_due_futures_with_result_hook<F, Fut>(
        &self,
        transaction: &AtomicResultTransaction,
        pre_commit_hook: F,
    ) -> Result<Option<Arc<DueFutureResult>>>
    where
        F: FnOnce(Arc<DueFutureResult>) -> Fut + Send,
        Fut: Future<Output = std::result::Result<(), IndexError>> + Send,
    {
        self.validate_transaction(transaction)?;
        self.process_due_futures_with_non_atomic_result_hook(pre_commit_hook)
            .await
    }

    fn validate_transaction(&self, transaction: &AtomicResultTransaction) -> Result<()> {
        let expected = self.resources.atomic_result_transaction()?;
        if transaction.matches(&expected, &self.inner.session_control) {
            Ok(())
        } else {
            Err(ComputationQueryError::TransactionMismatch)
        }
    }
}
