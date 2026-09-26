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

use async_trait::async_trait;
use std::{
    future::Future,
    sync::{Arc, Weak},
};

use crate::{
    computation::{
        AtomicResultTransaction, ComputationFutureResult, ComputationIndexes,
        ComputationQueryError, ComputationTransaction, Result,
    },
    evaluation::context::QueryPartEvaluationContext,
    interface::{FutureElementRef, FutureQueue, IndexError, PushType},
    models::SourceChange,
    query::{QueryBuilder, QueryEvaluator},
};

/// Query-local execution for the parallel graph, using the unchanged evaluator.
///
/// Construction consumes a graph resource bundle and binds every evaluator index
/// through the existing builder. An arbitrary prebuilt legacy query cannot be
/// paired with unrelated indexes or a descriptive transaction token.
pub struct ComputationQuery {
    inner: QueryEvaluator,
    transaction: Arc<ComputationTransaction>,
    scheduling: Arc<dyn FutureQueue>,
}

struct ScheduledQueue {
    transaction: Weak<ComputationTransaction>,
    queue: Arc<dyn FutureQueue>,
}

#[async_trait]
impl FutureQueue for ScheduledQueue {
    async fn peek_due_time(
        &self,
    ) -> std::result::Result<Option<crate::models::ElementTimestamp>, IndexError> {
        let transaction = self.transaction.upgrade().ok_or(IndexError::NotSupported)?;
        transaction
            .inspect(async { Ok(self.queue.peek_due_time().await?) })
            .await
            .map_err(IndexError::other)
    }
    async fn push(
        &self,
        _: PushType,
        _: usize,
        _: u64,
        _: &crate::models::ElementReference,
        _: crate::models::ElementTimestamp,
        _: crate::models::ElementTimestamp,
    ) -> std::result::Result<bool, IndexError> {
        Err(IndexError::NotSupported)
    }
    async fn pop(&self) -> std::result::Result<Option<FutureElementRef>, IndexError> {
        Err(IndexError::NotSupported)
    }
    async fn remove(&self, _: usize, _: u64) -> std::result::Result<(), IndexError> {
        Err(IndexError::NotSupported)
    }
    async fn clear(&self) -> std::result::Result<(), IndexError> {
        Err(IndexError::NotSupported)
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
            .try_build_evaluator()
            .await?;
        if !Arc::ptr_eq(inner.element_index(), &set.element_index) {
            return Err(ComputationQueryError::TransactionMismatch);
        }
        let transaction = Arc::new(ComputationTransaction::for_query(resources));
        let scheduling = Arc::new(ScheduledQueue {
            transaction: Arc::downgrade(&transaction),
            queue: inner.future_queue(),
        });
        Ok(Self {
            inner,
            transaction,
            scheduling,
        })
    }

    pub fn resources(&self) -> &ComputationIndexes {
        self.transaction.resources()
    }

    /// Run graph-owned metadata/reset staging under the same serialized resource
    /// transaction and cancellation/recovery rules as evaluation.
    pub async fn resource_transaction<F, Fut>(&self, operation: F) -> Result<()>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = std::result::Result<(), IndexError>> + Send,
    {
        self.in_operation(async {
            operation().await?;
            Ok(())
        })
        .await
    }

    /// Includes the builder's existing shadow queue; no spawned timer/consumer.
    pub fn future_queue(&self) -> Arc<dyn FutureQueue> {
        self.inner.future_queue()
    }

    /// Read-only committed scheduling view; it cannot pop or mutate due work.
    pub fn scheduling_queue(&self) -> Arc<dyn FutureQueue> {
        self.scheduling.clone()
    }

    /// Dropped evaluation, failed commit/rollback, or failed non-atomic processing
    /// can leave uncertain state. The owner must recover a replacement rather
    /// than reuse this constructed query.
    pub fn recovery_required(&self) -> bool {
        self.transaction.recovery_required()
    }

    /// Await provider-registered work before retiring this query's resources.
    /// This seals the constructed query; it is not a restart or a rollback claim.
    pub async fn shutdown(&self) -> Result<()> {
        self.transaction.shutdown().await
    }

    pub async fn quiesce(&self) -> Result<()> {
        self.transaction.quiesce().await
    }

    async fn in_operation<T>(&self, operation: impl Future<Output = Result<T>>) -> Result<T> {
        self.transaction.run(operation).await
    }

    async fn evaluate_source_change(
        &self,
        change: SourceChange,
    ) -> Result<Vec<QueryPartEvaluationContext>> {
        self.evaluate_source_changes(vec![change]).await
    }

    async fn evaluate_source_changes(
        &self,
        input: Vec<SourceChange>,
    ) -> Result<Vec<QueryPartEvaluationContext>> {
        Ok(self.inner.evaluate_changes(input).await?)
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
        self.process_source_changes_with_non_atomic_result_hook(vec![change], pre_commit_hook)
            .await
    }

    pub async fn process_source_changes_with_non_atomic_result_hook<F, Fut>(
        &self,
        changes: Vec<SourceChange>,
        pre_commit_hook: F,
    ) -> Result<Arc<[QueryPartEvaluationContext]>>
    where
        F: FnOnce(Arc<[QueryPartEvaluationContext]>) -> Fut + Send,
        Fut: Future<Output = std::result::Result<(), IndexError>> + Send,
    {
        self.in_operation(async {
            let results: Arc<[QueryPartEvaluationContext]> =
                self.evaluate_source_changes(changes).await?.into();
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
        self.process_source_changes_with_result_hook(vec![change], transaction, pre_commit_hook)
            .await
    }

    pub async fn process_source_changes_with_result_hook<F, Fut>(
        &self,
        changes: Vec<SourceChange>,
        transaction: &AtomicResultTransaction,
        pre_commit_hook: F,
    ) -> Result<Arc<[QueryPartEvaluationContext]>>
    where
        F: FnOnce(Arc<[QueryPartEvaluationContext]>) -> Fut + Send,
        Fut: Future<Output = std::result::Result<(), IndexError>> + Send,
    {
        self.validate_transaction(transaction)?;
        self.process_source_changes_with_non_atomic_result_hook(changes, pre_commit_hook)
            .await
    }

    pub async fn process_due_futures(&self) -> Result<Option<Arc<ComputationFutureResult>>> {
        self.process_due_futures_with_non_atomic_result_hook(|_| async { Ok(()) })
            .await
    }

    /// Process one due item using its original source provenance. An empty queue
    /// commits an empty session without invoking the hook.
    pub async fn process_due_futures_with_non_atomic_result_hook<F, Fut>(
        &self,
        pre_commit_hook: F,
    ) -> Result<Option<Arc<ComputationFutureResult>>>
    where
        F: FnOnce(Arc<ComputationFutureResult>) -> Fut + Send,
        Fut: Future<Output = std::result::Result<(), IndexError>> + Send,
    {
        self.in_operation(async {
            let Some(future_ref) = self.inner.future_queue().pop().await? else {
                return Ok(None);
            };
            let source_id = future_ref.element_ref.source_id.clone();
            let results = Arc::new(ComputationFutureResult {
                results: self
                    .evaluate_source_change(SourceChange::Future {
                        future_ref: future_ref.clone(),
                    })
                    .await?,
                source_id,
                future: future_ref,
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
    ) -> Result<Option<Arc<ComputationFutureResult>>>
    where
        F: FnOnce(Arc<ComputationFutureResult>) -> Fut + Send,
        Fut: Future<Output = std::result::Result<(), IndexError>> + Send,
    {
        self.validate_transaction(transaction)?;
        self.process_due_futures_with_non_atomic_result_hook(pre_commit_hook)
            .await
    }

    fn validate_transaction(&self, transaction: &AtomicResultTransaction) -> Result<()> {
        let expected = self.resources().atomic_result_transaction()?;
        if transaction.matches(&expected, &self.resources().indexes().session_control) {
            Ok(())
        } else {
            Err(ComputationQueryError::TransactionMismatch)
        }
    }
}
