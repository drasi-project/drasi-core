// Copyright 2024 The Drasi Authors.
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

use super::QueryEvaluator;
use crate::{
    evaluation::{
        context::QueryPartEvaluationContext, EvaluationError, ExpressionEvaluator,
        QueryPartEvaluator,
    },
    interface::{
        ElementIndex, FutureQueue, FutureQueueConsumer, IndexError, SessionControl, SessionGuard,
    },
    middleware::SourceMiddlewarePipelineCollection,
    models::SourceChange,
    path_solver::{match_path::MatchPath, MatchPathSolver},
};
use drasi_query_ast::ast::Query;
use std::{fmt::Debug, future::Future, sync::Arc, time::Duration};
use tokio::{
    select,
    sync::{Mutex, Notify},
    task::JoinHandle,
};

pub struct DueFutureResult {
    pub results: Vec<QueryPartEvaluationContext>,
    pub source_id: Arc<str>,
}

/// Standalone execution and scheduling around the shared evaluator.
pub struct ContinuousQuery {
    evaluator: QueryEvaluator,
    future_consumer_shutdown_request: Arc<Notify>,
    future_queue_task: Mutex<Option<JoinHandle<()>>>,
    change_lock: Mutex<()>,
    session_control: Arc<dyn SessionControl>,
}

impl ContinuousQuery {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        query: Arc<Query>,
        match_path: Arc<MatchPath>,
        expression_evaluator: Arc<ExpressionEvaluator>,
        element_index: Arc<dyn ElementIndex>,
        path_solver: Arc<MatchPathSolver>,
        part_evaluator: Arc<QueryPartEvaluator>,
        future_queue: Arc<dyn FutureQueue>,
        source_pipelines: SourceMiddlewarePipelineCollection,
        session_control: Arc<dyn SessionControl>,
    ) -> Self {
        Self::from_evaluator(
            QueryEvaluator::new(
                query,
                match_path,
                expression_evaluator,
                element_index,
                path_solver,
                part_evaluator,
                future_queue,
                source_pipelines,
            ),
            session_control,
        )
    }

    pub(super) fn from_evaluator(
        evaluator: QueryEvaluator,
        session_control: Arc<dyn SessionControl>,
    ) -> Self {
        Self {
            evaluator,
            session_control,
            future_consumer_shutdown_request: Arc::new(Notify::new()),
            future_queue_task: Mutex::new(None),
            change_lock: Mutex::new(()),
        }
    }

    #[tracing::instrument(skip_all, err, level = "debug")]
    pub async fn process_source_change(
        &self,
        change: SourceChange,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        let _lock = self.change_lock.lock().await;
        let guard = SessionGuard::begin(self.session_control.clone()).await?;

        let result = self.evaluator.evaluate_changes(vec![change]).await?;

        guard.commit().await?;
        Ok(result)
    }

    /// Process a source change with a pre-commit hook that runs inside the session.
    ///
    /// The hook executes after index updates but before the session commits,
    /// allowing callers to stage additional writes (e.g. checkpoint data, outbox,
    /// live results, result sequence) into the same atomic transaction. The hook
    /// receives the evaluation results so output can be staged before commit.
    /// The change_lock is held for the entire duration, preserving serialization.
    #[tracing::instrument(skip_all, err, level = "debug")]
    pub async fn process_source_change_with_hook<F, Fut>(
        &self,
        change: SourceChange,
        pre_commit_hook: F,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError>
    where
        F: FnOnce(&[QueryPartEvaluationContext]) -> Fut + Send,
        Fut: Future<Output = Result<(), IndexError>> + Send,
    {
        let _lock = self.change_lock.lock().await;
        let guard = SessionGuard::begin(self.session_control.clone()).await?;

        let result = self.evaluator.evaluate_changes(vec![change]).await?;

        pre_commit_hook(&result).await?;
        guard.commit().await?;
        Ok(result)
    }

    /// Atomically pop a due future from the queue and process it within a single session.
    ///
    /// Returns `Ok(None)` when the queue is empty (stale peek).
    /// Returns `Ok(Some(DueFutureResult))` with results and the original source_id.
    ///
    /// Pop happens inside the session → atomic with all downstream index writes.
    /// If a crash occurs before commit, the pop rolls back and the item stays in the queue.
    #[tracing::instrument(skip_all, err, level = "debug")]
    pub async fn process_due_futures(&self) -> Result<Option<DueFutureResult>, EvaluationError> {
        self.process_due_futures_with_hook(|_results, _source_id| async { Ok(()) })
            .await
    }

    /// Process a due future with a pre-commit hook that runs inside the session.
    ///
    /// Same atomic pop-and-process semantics as [`process_due_futures`], but the
    /// hook can stage output (result sequence, outbox, live results) before commit.
    /// There is no source checkpoint — futures are not source events.
    #[tracing::instrument(skip_all, err, level = "debug")]
    pub async fn process_due_futures_with_hook<F, Fut>(
        &self,
        pre_commit_hook: F,
    ) -> Result<Option<DueFutureResult>, EvaluationError>
    where
        F: FnOnce(&[QueryPartEvaluationContext], &str) -> Fut + Send,
        Fut: Future<Output = Result<(), IndexError>> + Send,
    {
        let _lock = self.change_lock.lock().await;
        let guard = SessionGuard::begin(self.session_control.clone()).await?;

        let future_ref = match self.evaluator.future_queue().pop().await {
            Ok(Some(fr)) => fr,
            Ok(None) => {
                guard.commit().await?;
                return Ok(None);
            }
            Err(e) => return Err(EvaluationError::from(e)),
        };

        let source_id = future_ref.element_ref.source_id.clone();
        let change = SourceChange::Future { future_ref };
        let results = self.evaluator.evaluate_changes(vec![change]).await?;
        pre_commit_hook(&results, source_id.as_ref()).await?;
        guard.commit().await?;
        Ok(Some(DueFutureResult { results, source_id }))
    }

    /// Expose the ContinuousQuery's future queue for external polling.
    pub fn future_queue(&self) -> Arc<dyn FutureQueue> {
        self.evaluator.future_queue()
    }

    pub async fn set_future_consumer(&self, consumer: Arc<dyn FutureQueueConsumer>) {
        let mut future_queue_task = self.future_queue_task.lock().await;
        if let Some(c) = future_queue_task.take() {
            c.abort();
        }

        let queue = self.evaluator.future_queue();
        let shutdown_request = self.future_consumer_shutdown_request.clone();

        let task = tokio::spawn(async move {
            let idle_interval = Duration::from_secs(1);
            let error_interval = Duration::from_secs(5);
            loop {
                select! {
                    _ = shutdown_request.notified() => {
                        log::info!("Future queue consumer shutting down");
                        break;
                    }
                    peek = queue.peek_due_time() => {
                        match peek {
                            Ok(Some(due_time)) => {
                                if due_time > consumer.now() {
                                    tokio::time::sleep(idle_interval).await;
                                    continue;
                                }
                            }
                            Ok(None) => {
                                tokio::time::sleep(idle_interval).await;
                                continue;
                            }
                            Err(e) => {
                                log::error!("Future queue consumer error: {e:?}");
                                tokio::time::sleep(error_interval).await;
                                continue;
                            }
                        };

                        // Items are due — delegate to consumer which calls process_due_futures()
                        match consumer.on_items_due().await {
                            Ok(_) => {}
                            Err(e) => {
                                log::error!("Future queue consumer error: {e:?}");
                                consumer.on_error(e).await;
                                tokio::time::sleep(error_interval).await;
                            }
                        }
                    }
                }
            }
        });

        _ = future_queue_task.insert(task);
    }

    pub async fn terminate_future_consumer(&self) {
        let mut future_queue_task = self.future_queue_task.lock().await;
        if let Some(task) = future_queue_task.take() {
            self.future_consumer_shutdown_request.notify_one();
            select! {
                _ = task => {
                    log::info!("Future queue consumer terminated");
                }
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    log::error!("Future queue consumer termination timeout");
                }
            }
        }
    }

    pub fn get_query(&self) -> Arc<Query> {
        self.evaluator.get_query()
    }
}

impl Drop for ContinuousQuery {
    fn drop(&mut self) {
        self.future_consumer_shutdown_request.notify_one();
    }
}

impl Debug for ContinuousQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContinuousQuery")
            .field("query", &self.evaluator.get_query())
            .finish()
    }
}
