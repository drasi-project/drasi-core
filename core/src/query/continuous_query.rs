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

use crate::evaluation::QueryExecutionError;

use crate::interface::SessionError;

use std::{
    collections::HashMap,
    fmt::Debug,
    future::Future,
    hash::{Hash, Hasher},
    sync::Arc,
    time::Duration,
};

use drasi_query_ast::ast::Query;
use hashers::jenkins::spooky_hash::SpookyHasher;
use tokio::{
    select,
    sync::{Mutex, Notify},
    task::JoinHandle,
};

use crate::{
    evaluation::{
        context::{ChangeContext, QueryPartEvaluationContext, QueryVariables},
        EvaluationError, ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock,
        QueryPartEvaluator,
    },
    interface::{
        ElementIndex, FutureQueue, FutureQueueConsumer, IndexError, MiddlewareError, Provisional,
        QueryClock, RootOutcome, SessionControl, SessionGuard,
    },
    middleware::SourceMiddlewarePipelineCollection,
    models::{Element, SourceChange, SourceInput},
    path_solver::{
        match_path::{MatchPath, SlotElementSpec},
        solution::{MatchPathSolution, SolutionSignature},
        variable_length::VariableLengthMatchPlan,
        MatchPathSolver, MatchSolveContext,
    },
};

/// Result of processing a due future item.
/// Contains the evaluation results and the source_id from the popped future's element_ref,
/// needed by the lib crate to record provenance in QueryResult metadata.
#[derive(Debug)]
pub struct DueFutureResult {
    pub results: Vec<QueryPartEvaluationContext>,
    /// The source_id from the popped future's element_ref.
    pub source_id: Arc<str>,
}

pub(super) struct FixedMatcher {
    pub match_path: Arc<MatchPath>,
    pub path_solver: Arc<MatchPathSolver>,
}

pub(super) enum Matcher {
    Fixed(FixedMatcher),
    VariableLength(VariableLengthMatchPlan),
}

pub struct ContinuousQuery {
    expression_evaluator: Arc<ExpressionEvaluator>,
    part_evaluator: Arc<QueryPartEvaluator>,
    element_index: Arc<dyn ElementIndex>,
    matcher: Matcher,
    query: Arc<Query>,
    future_consumer_shutdown_request: Arc<Notify>,
    future_queue: Arc<dyn FutureQueue>,
    future_queue_task: Mutex<Option<JoinHandle<()>>>,
    change_lock: Mutex<()>,
    source_pipelines: SourceMiddlewarePipelineCollection,
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
        Self::new_with_matcher(
            query,
            Matcher::Fixed(FixedMatcher {
                match_path,
                path_solver,
            }),
            expression_evaluator,
            element_index,
            part_evaluator,
            future_queue,
            source_pipelines,
            session_control,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_with_matcher(
        query: Arc<Query>,
        matcher: Matcher,
        expression_evaluator: Arc<ExpressionEvaluator>,
        element_index: Arc<dyn ElementIndex>,
        part_evaluator: Arc<QueryPartEvaluator>,
        future_queue: Arc<dyn FutureQueue>,
        source_pipelines: SourceMiddlewarePipelineCollection,
        session_control: Arc<dyn SessionControl>,
    ) -> Self {
        Self {
            expression_evaluator,
            element_index,
            matcher,
            part_evaluator,
            query,
            future_consumer_shutdown_request: Arc::new(Notify::new()),
            future_queue,
            future_queue_task: Mutex::new(None),
            change_lock: Mutex::new(()),
            source_pipelines,
            session_control,
        }
    }

    /// Check whether the last root permits processing or needs recovery.
    pub fn check_health(&self) -> Result<(), EvaluationError> {
        if let Some(root) =
            crate::interface::session_tracker(&self.session_control)?.current_root()?
        {
            match root.outcome() {
                RootOutcome::RequiresRebuild | RootOutcome::Indeterminate => {
                    return Err(EvaluationError::from(
                        QueryExecutionError::QueryRequiresRebuild,
                    ));
                }
                RootOutcome::Committing => {
                    return Err(IndexError::other(SessionError::SessionBusy).into())
                }
                RootOutcome::Active | RootOutcome::Committed | RootOutcome::RolledBack => {}
            }
        }
        Ok(())
    }

    /// Apply one source change and return its result changes.
    ///
    /// Variable-length MATCH prepares both snapshots before writing. Resource
    /// failures during pure preparation can be retried. A complete transactional
    /// rollback permits retry; dirty nontransactional or uncertain roots require recovery.
    #[tracing::instrument(skip_all, err, level = "debug")]
    pub async fn process_source_change(
        &self,
        change: SourceChange,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        self.process_source_change_with_hook(change, || async { Ok(()) })
            .await
    }

    /// Process a source change with a pre-commit hook that runs inside the session.
    ///
    /// The hook executes after index updates but before the session commits,
    /// allowing callers to stage additional writes (e.g. checkpoint data) into
    /// the same atomic transaction. The change_lock is held for the entire
    /// duration, preserving serialization.
    #[tracing::instrument(skip_all, err, level = "debug")]
    pub async fn process_source_change_with_hook<F, Fut>(
        &self,
        change: SourceChange,
        pre_commit_hook: F,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError>
    where
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<(), IndexError>> + Send,
    {
        self.process_source_change_with_result_hook(change, |_| pre_commit_hook())
            .await
    }

    /// Process a source change with a hook that can serialize tentative results
    /// into the transactional outbox. Publish only the returned committed results.
    pub async fn process_source_change_with_result_hook<F, Fut>(
        &self,
        change: impl Into<SourceInput> + Send,
        pre_commit_hook: F,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError>
    where
        F: FnOnce(&[QueryPartEvaluationContext]) -> Fut + Send,
        Fut: Future<Output = Result<(), IndexError>> + Send,
    {
        let _lock = self.change_lock.lock().await;
        self.check_health()?;
        let mut root = SessionGuard::begin(self.session_control.clone()).await?;
        let child = root.child(&self.session_control)?;
        let result = self
            .process_source_input_with_hook(change.into(), pre_commit_hook)
            .await?;
        let provisional = child.complete(result)?;
        let receipt = root.commit_with_receipt().await?;
        Ok(provisional.into_committed(&receipt)?)
    }

    /// Stage a change in an explicit parent root without committing that root.
    pub async fn process_source_change_in(
        &self,
        parent: &mut SessionGuard,
        change: impl Into<SourceInput> + Send,
    ) -> Result<Provisional<Vec<QueryPartEvaluationContext>>, EvaluationError> {
        self.process_source_change_in_with_hook(parent, change, |_| async { Ok(()) })
            .await
    }

    /// Stage a source change and checkpoint/outbox writes in the same parent root.
    ///
    /// The hook sees tentative results only for staging. Do not publish them.
    /// It must produce an owned future, for example by serializing results before
    /// entering its async block. Results become publishable with the root's receipt.
    pub async fn process_source_change_in_with_hook<F, Fut>(
        &self,
        parent: &mut SessionGuard,
        change: impl Into<SourceInput> + Send,
        pre_commit_hook: F,
    ) -> Result<Provisional<Vec<QueryPartEvaluationContext>>, EvaluationError>
    where
        F: FnOnce(&[QueryPartEvaluationContext]) -> Fut + Send,
        Fut: Future<Output = Result<(), IndexError>> + Send,
    {
        let _lock = self.change_lock.lock().await;
        self.check_health()?;
        let child = parent.child(&self.session_control)?;
        let result = self
            .process_source_input_with_hook(change.into(), pre_commit_hook)
            .await?;
        Ok(child.complete(result)?)
    }

    async fn process_source_input_with_hook<F, Fut>(
        &self,
        input: SourceInput,
        pre_commit_hook: F,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError>
    where
        F: FnOnce(&[QueryPartEvaluationContext]) -> Fut + Send,
        Fut: Future<Output = Result<(), IndexError>> + Send,
    {
        let changes = self.prepare_source_input(input).await?;
        let result = self.process_changes_inner(changes).await?;
        crate::interface::session_tracker(&self.session_control)?.mark_dirty()?;
        pre_commit_hook(&result).await?;
        Ok(result)
    }

    /// Atomically pop a due future from the queue and process it within a single session.
    ///
    /// Returns `Ok(None)` when the queue is empty (stale peek).
    /// Returns `Ok(Some(DueFutureResult))` with results and the original source_id.
    ///
    /// Pop is atomic with downstream writes when the session supports rollback.
    /// Dirty nontransactional and indeterminate roots require recovery; a complete
    /// rollback restores the popped item along with the downstream writes.
    #[tracing::instrument(skip_all, err, level = "debug")]
    pub async fn process_due_futures(&self) -> Result<Option<DueFutureResult>, EvaluationError> {
        self.process_due_futures_with_hook(|_| async { Ok(()) })
            .await
    }

    /// Pop and evaluate a future, staging hook writes before the root commit.
    pub async fn process_due_futures_with_hook<F, Fut>(
        &self,
        pre_commit_hook: F,
    ) -> Result<Option<DueFutureResult>, EvaluationError>
    where
        F: FnOnce(&Option<DueFutureResult>) -> Fut + Send,
        Fut: Future<Output = Result<(), IndexError>> + Send,
    {
        let _lock = self.change_lock.lock().await;
        self.check_health()?;
        let mut root = SessionGuard::begin(self.session_control.clone()).await?;
        let child = root.child(&self.session_control)?;
        let result = self.process_due_futures_inner().await?;
        crate::interface::session_tracker(&self.session_control)?.mark_dirty()?;
        pre_commit_hook(&result).await?;
        let provisional = child.complete(result)?;
        let receipt = root.commit_with_receipt().await?;
        Ok(provisional.into_committed(&receipt)?)
    }

    pub async fn process_due_futures_in(
        &self,
        parent: &mut SessionGuard,
    ) -> Result<Provisional<Option<DueFutureResult>>, EvaluationError> {
        self.process_due_futures_in_with_hook(parent, |_| async { Ok(()) })
            .await
    }

    /// Stage a future pop, evaluation, and hook in an explicit parent root.
    pub async fn process_due_futures_in_with_hook<F, Fut>(
        &self,
        parent: &mut SessionGuard,
        pre_commit_hook: F,
    ) -> Result<Provisional<Option<DueFutureResult>>, EvaluationError>
    where
        F: FnOnce(&Option<DueFutureResult>) -> Fut + Send,
        Fut: Future<Output = Result<(), IndexError>> + Send,
    {
        let _lock = self.change_lock.lock().await;
        self.check_health()?;
        let child = parent.child(&self.session_control)?;
        let result = self.process_due_futures_inner().await?;
        crate::interface::session_tracker(&self.session_control)?.mark_dirty()?;
        pre_commit_hook(&result).await?;
        Ok(child.complete(result)?)
    }

    async fn process_due_futures_inner(&self) -> Result<Option<DueFutureResult>, EvaluationError> {
        crate::interface::session_tracker(&self.session_control)?.mark_dirty()?;
        let Some(entry) = self.future_queue.pop().await? else {
            return Ok(None);
        };
        let source_id = entry.element_ref.source_id.clone();
        let results = self
            .process_changes_inner(vec![SourceChange::Future { future_ref: entry }])
            .await?;
        Ok(Some(DueFutureResult { results, source_id }))
    }

    /// Expose the ContinuousQuery's future queue for external polling.
    pub fn future_queue(&self) -> Arc<dyn FutureQueue> {
        self.future_queue.clone()
    }

    /// Inner processing logic shared by `process_source_change` and `process_due_futures`.
    /// Must be called within an active session and while holding `change_lock`.
    #[tracing::instrument(skip_all, err, level = "debug")]
    async fn process_changes_inner(
        &self,
        changes: Vec<SourceChange>,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        let mut result = Vec::new();

        for change in changes {
            let future_group_signature = match &change {
                SourceChange::Future { future_ref } => Some(future_ref.group_signature),
                _ => None,
            };
            let base_variables = QueryVariables::new(); //todo: get query parameters
            let after_clock = Arc::new(InstantQueryClock::from_source_change(&change));

            let solution_changes = self
                .build_solution_changes(&base_variables, change, after_clock.clone())
                .await?;
            let before_clock = match solution_changes.before_clock {
                Some(before_clock) => before_clock,
                None => after_clock.clone(),
            };

            let mut aggregation_results = CollapsedAggregationResults::new();

            for change in solution_changes.changes {
                let solution_signature = change.signature;
                let part_context = change.context;
                crate::interface::session_tracker(&self.session_control)?.mark_dirty()?;
                let change_results = match self
                    .project_solution(
                        part_context,
                        &ChangeContext {
                            solution_signature,
                            before_clock: before_clock.clone(),
                            after_clock: after_clock.clone(),
                            before_anchor_element: solution_changes.before_anchor_element.clone(),
                            after_anchor_element: solution_changes.anchor_element.clone(),
                            is_future_reprocess: solution_changes.is_future_reprocess,
                            before_grouping_hash: solution_signature,
                            after_grouping_hash: solution_signature,
                            future_group_signature,
                        },
                    )
                    .await
                {
                    Ok(results) => results,
                    Err(EvaluationError::DivideByZero)
                        if matches!(&self.matcher, Matcher::Fixed(_)) =>
                    {
                        log::debug!("Skipping solution due to DivideByZero in projection");
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                change_results.into_iter().for_each(|ctx| {
                    match &ctx {
                        QueryPartEvaluationContext::Aggregation {
                            before,
                            after,
                            default_before,
                            ..
                        } => {
                            if let Some(before) = before {
                                if before == after && !default_before {
                                    return;
                                }
                            }

                            aggregation_results.insert(ctx);
                        }
                        QueryPartEvaluationContext::Updating { before, after, .. } => {
                            if before == after {
                                return;
                            }
                            result.push(ctx);
                        }
                        _ => result.push(ctx),
                    };
                });
            }

            for ctx in aggregation_results.into_result_vec() {
                result.push(ctx);
            }
        }

        Ok(result)
    }

    #[tracing::instrument(skip_all, err, level = "debug")]
    async fn build_solution_changes(
        &self,
        base_variables: &QueryVariables,
        change: SourceChange,
        clock: Arc<dyn QueryClock>,
    ) -> Result<SolutionChangesResult, EvaluationError> {
        let fixed = match &self.matcher {
            Matcher::VariableLength(plan) => {
                let prepared = super::variable_length::prepare(
                    plan,
                    self.element_index.as_ref(),
                    self.expression_evaluator.as_ref(),
                    change,
                    clock,
                    base_variables,
                )
                .await?;
                match prepared.write {
                    super::variable_length::IndexWrite::Set(element, slots) => {
                        crate::interface::session_tracker(&self.session_control)?.mark_dirty()?;
                        self.element_index.set_element(&element, &slots).await?;
                    }
                    super::variable_length::IndexWrite::Delete(reference) => {
                        crate::interface::session_tracker(&self.session_control)?.mark_dirty()?;
                        self.element_index.delete_element(&reference).await?;
                    }
                    super::variable_length::IndexWrite::None => {}
                }
                return Ok(prepared.solutions);
            }
            Matcher::Fixed(fixed) => {
                // Fixed-pattern expressions can call stateful functions before graph writes.
                crate::interface::session_tracker(&self.session_control)?.mark_dirty()?;
                fixed
            }
        };
        let mut result = SolutionChangesResult::new();
        let mut before_change_solutions = HashMap::new();
        let mut after_change_solutions = HashMap::new();

        match change {
            SourceChange::Insert { element } => {
                let element = Arc::new(element);
                let affinity_slots = self
                    .get_slots_with_affinity(fixed, base_variables, element.clone(), clock.clone())
                    .await?;
                let solutions = self
                    .resolve_solutions(fixed, element.clone(), affinity_slots, true)
                    .await?;

                for (signature, solution) in solutions {
                    if let Some(blank_optional_solution) =
                        solution.get_empty_optional_solution(&fixed.match_path)
                    {
                        before_change_solutions.insert(signature, blank_optional_solution);
                    }
                    after_change_solutions.insert(signature, solution);
                }

                result.anchor_element = Some(element);
            }
            SourceChange::Update { mut element } => {
                if let Some(prev_version) = self
                    .element_index
                    .get_element(element.get_reference())
                    .await?
                {
                    let prev_timestamp = prev_version.get_effective_from();
                    let before_clock =
                        Arc::new(InstantQueryClock::new(prev_timestamp, clock.get_realtime()));
                    let affinity_slots = self
                        .get_slots_with_affinity(
                            fixed,
                            base_variables,
                            prev_version.clone(),
                            before_clock.clone(),
                        )
                        .await?;
                    let solutions = self
                        .resolve_solutions(fixed, prev_version.clone(), affinity_slots, false)
                        .await?;
                    for (signature, solution) in solutions {
                        before_change_solutions.insert(signature, solution);
                    }
                    element.merge_missing_properties(prev_version.as_ref());
                    result.before_clock = Some(before_clock);
                    result.before_anchor_element = Some(prev_version);
                }

                let element = Arc::new(element);
                let affinity_slots = self
                    .get_slots_with_affinity(fixed, base_variables, element.clone(), clock.clone())
                    .await?;
                let solutions = self
                    .resolve_solutions(fixed, element.clone(), affinity_slots, true)
                    .await?;

                for (signature, solution) in solutions {
                    after_change_solutions.insert(signature, solution);
                }

                result.anchor_element = Some(element);
            }
            SourceChange::Delete { metadata } => {
                if let Some(element) = self.element_index.get_element(&metadata.reference).await? {
                    let prev_timestamp = element.get_effective_from();
                    let before_clock =
                        Arc::new(InstantQueryClock::new(prev_timestamp, clock.get_realtime()));
                    let affinity_slots = self
                        .get_slots_with_affinity(
                            fixed,
                            base_variables,
                            element.clone(),
                            before_clock.clone(),
                        )
                        .await?;
                    let solutions = self
                        .resolve_solutions(fixed, element.clone(), affinity_slots, false)
                        .await?;
                    for (signature, solution) in solutions {
                        if let Some(blank_optional_solution) =
                            solution.get_empty_optional_solution(&fixed.match_path)
                        {
                            after_change_solutions.insert(signature, blank_optional_solution);
                        }

                        before_change_solutions.insert(signature, solution);
                    }
                    result.before_clock = Some(before_clock);
                    result.before_anchor_element = Some(element);

                    match self.element_index.delete_element(&metadata.reference).await {
                        Ok(_) => {}
                        Err(e) => return Err(EvaluationError::from(e)),
                    }
                }
            }
            SourceChange::Future { future_ref } => {
                result.is_future_reprocess = true;
                if let Some(element) = self
                    .element_index
                    .get_element(&future_ref.element_ref)
                    .await?
                {
                    let timestamp = element.get_effective_from();
                    if timestamp >= future_ref.due_time {
                        return Ok(result);
                    }
                    let before_clock = Arc::new(InstantQueryClock::new(timestamp, timestamp));
                    let slots = self
                        .get_slots_with_affinity(
                            fixed,
                            base_variables,
                            element.clone(),
                            before_clock.clone(),
                        )
                        .await?;
                    before_change_solutions.extend(
                        self.resolve_solutions(fixed, element.clone(), slots, false)
                            .await?,
                    );
                    result.before_clock = Some(before_clock);
                    result.before_anchor_element = Some(element.clone());
                    let slots = self
                        .get_slots_with_affinity(
                            fixed,
                            base_variables,
                            element.clone(),
                            clock.clone(),
                        )
                        .await?;
                    after_change_solutions.extend(
                        self.resolve_solutions(fixed, element.clone(), slots, false)
                            .await?,
                    );
                    result.anchor_element = Some(element);
                }
            }
        }

        for (sig, before_sol) in &before_change_solutions {
            match after_change_solutions.get(sig) {
                Some(after_sol) => result.changes.push(SolutionChange {
                    signature: *sig,
                    context: QueryPartEvaluationContext::Updating {
                        before: before_sol.into_query_variables(&fixed.match_path, base_variables),
                        after: after_sol.into_query_variables(&fixed.match_path, base_variables),
                        row_signature: 0,
                    },
                }),
                None => result.changes.push(SolutionChange {
                    signature: *sig,
                    context: QueryPartEvaluationContext::Removing {
                        before: before_sol.into_query_variables(&fixed.match_path, base_variables),
                        row_signature: 0,
                    },
                }),
            }
        }

        for (sig, after_sol) in &after_change_solutions {
            if !before_change_solutions.contains_key(sig) {
                result.changes.push(SolutionChange {
                    signature: *sig,
                    context: QueryPartEvaluationContext::Adding {
                        after: after_sol.into_query_variables(&fixed.match_path, base_variables),
                        row_signature: 0,
                    },
                })
            }
        }

        Ok(result)
    }

    async fn resolve_solutions(
        &self,
        fixed: &FixedMatcher,
        anchor_element: Arc<Element>,
        affinity_slots: Vec<usize>,
        update_index: bool,
    ) -> Result<HashMap<u64, MatchPathSolution>, EvaluationError> {
        if update_index {
            self.element_index
                .set_element(anchor_element.as_ref(), &affinity_slots)
                .await?;
        }

        let mut result = HashMap::new();

        for slot_num in affinity_slots {
            let solution = fixed
                .path_solver
                .solve(fixed.match_path.clone(), anchor_element.clone(), slot_num)
                .await?;
            result.extend(solution);
        }

        Ok(result)
    }

    async fn get_slots_with_affinity(
        &self,
        fixed: &FixedMatcher,
        variables: &QueryVariables,
        anchor_element: Arc<Element>,
        clock: Arc<dyn QueryClock>,
    ) -> Result<Vec<usize>, EvaluationError> {
        let context = MatchSolveContext::new(variables, clock);

        let mut affinity_slots = Vec::new();

        for (slot_num, slot) in fixed.match_path.slots.iter().enumerate() {
            if self
                .match_element_to_slot(&context, &slot.spec, anchor_element.clone())
                .await?
            {
                affinity_slots.push(slot_num);
            }
        }

        Ok(affinity_slots)
    }

    async fn match_element_to_slot(
        &self,
        context: &MatchSolveContext<'_>,
        element_spec: &SlotElementSpec,
        element: Arc<Element>,
    ) -> Result<bool, EvaluationError> {
        let metadata = element.get_metadata();
        let mut label_match = element_spec.labels.is_empty();

        for label in &element_spec.labels {
            if metadata.labels.contains(label) {
                label_match = true;
                break;
            }
        }

        if !label_match {
            return Ok(false);
        }

        let mut variables = context.variables.clone();

        let element_variable = element.to_expression_variable();

        if element_spec.annotation.is_some() {
            variables.insert(
                element_spec
                    .annotation
                    .clone()
                    .unwrap()
                    .to_string()
                    .into_boxed_str(),
                element_variable.clone(),
            );
        }

        variables.insert("".into(), element_variable);

        let eval_context = ExpressionEvaluationContext::from_slot(
            &variables,
            context.clock.clone(),
            &metadata.reference,
        );

        for predicate in &element_spec.predicates {
            let result = self
                .expression_evaluator
                .evaluate_predicate(&eval_context, predicate)
                .await?;
            if !result {
                return Ok(false);
            }
        }

        Ok(true)
    }

    #[tracing::instrument(skip_all, err, level = "debug")]
    async fn project_solution(
        &self,
        part_context: QueryPartEvaluationContext,
        change_context: &ChangeContext,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        let mut result = Vec::new();
        let mut contexts = vec![(part_context, change_context.clone())];

        let mut part_num = 0;

        for part in &self.query.parts {
            part_num += 1;
            result.clear();

            for (part_context, change_context) in &contexts {
                let new_contexts = self
                    .part_evaluator
                    .evaluate(part_context.clone(), part_num, part, change_context)
                    .await?;

                let mut aggregation_results = CollapsedAggregationResults::new();

                new_contexts.into_iter().for_each(|ctx| match &ctx {
                    QueryPartEvaluationContext::Aggregation { .. } => {
                        aggregation_results.insert(ctx)
                    }
                    QueryPartEvaluationContext::Noop => (),
                    _ => result.push((ctx, change_context.clone())),
                });

                for actx in aggregation_results.into_vec_with_context(change_context) {
                    result.push(actx);
                }
            }
            contexts = result.clone();
        }

        Ok(result
            .into_iter()
            .map(|(ctx, cc)| match ctx {
                QueryPartEvaluationContext::Adding { after, .. } => {
                    QueryPartEvaluationContext::Adding {
                        after,
                        row_signature: cc.solution_signature,
                    }
                }
                QueryPartEvaluationContext::Updating { before, after, .. } => {
                    QueryPartEvaluationContext::Updating {
                        before,
                        after,
                        row_signature: cc.solution_signature,
                    }
                }
                QueryPartEvaluationContext::Removing { before, .. } => {
                    QueryPartEvaluationContext::Removing {
                        before,
                        row_signature: cc.solution_signature,
                    }
                }
                QueryPartEvaluationContext::Aggregation {
                    before,
                    after,
                    grouping_keys,
                    default_before,
                    default_after,
                    ..
                } => QueryPartEvaluationContext::Aggregation {
                    before,
                    after,
                    grouping_keys,
                    default_before,
                    default_after,
                    row_signature: cc.after_grouping_hash,
                },
                QueryPartEvaluationContext::Noop => QueryPartEvaluationContext::Noop,
            })
            .collect())
    }

    #[tracing::instrument(skip_all, err, level = "debug")]
    async fn prepare_source_input(
        &self,
        input: SourceInput,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        let source_id = input.change.get_reference().source_id.clone();
        let changes = match self.source_pipelines.get(source_id) {
            Some(pipeline) => {
                let index = Arc::new(crate::middleware::TrackedElementIndex {
                    inner: self.element_index.clone(),
                    tracker: crate::interface::session_tracker(&self.session_control)?,
                });
                pipeline.process(input.change, index).await?
            }
            None => vec![input.change],
        };
        match input.normalizer {
            Some(normalizer) => Ok(changes
                .into_iter()
                .map(|change| normalizer.normalize(change))
                .collect()),
            None => Ok(changes),
        }
    }

    pub async fn set_future_consumer(&self, consumer: Arc<dyn FutureQueueConsumer>) {
        let mut future_queue_task = self.future_queue_task.lock().await;
        if let Some(c) = future_queue_task.take() {
            c.abort();
        }

        let queue = self.future_queue.clone();
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
                            Err(error) if error.session_error() == Some(SessionError::SessionBusy) => {
                                tokio::time::sleep(idle_interval).await;
                                continue;
                            }
                            Err(error) if error.session_error() == Some(SessionError::SessionFenced) => {
                                log::error!("Future queue requires recovery: {error}");
                                consumer.on_error(Box::new(error)).await;
                                break;
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
        self.query.clone()
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
            .field("query", &self.query)
            .finish()
    }
}

pub(super) struct SolutionChange {
    pub signature: SolutionSignature,
    pub context: QueryPartEvaluationContext,
}

pub(super) struct SolutionChangesResult {
    pub changes: Vec<SolutionChange>,
    pub anchor_element: Option<Arc<Element>>,
    pub before_clock: Option<Arc<dyn QueryClock>>,
    pub before_anchor_element: Option<Arc<Element>>,
    pub is_future_reprocess: bool,
}

impl SolutionChangesResult {
    pub(super) fn new() -> Self {
        Self {
            changes: Vec::new(),
            before_clock: None,
            anchor_element: None,
            before_anchor_element: None,
            is_future_reprocess: false,
        }
    }
}

struct CollapsedAggregationResults {
    // [hash of after change grouping keys] -> (context, hash of before change grouping keys)
    data: HashMap<u64, (QueryPartEvaluationContext, u64)>,
}

impl CollapsedAggregationResults {
    fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    fn insert(&mut self, context: QueryPartEvaluationContext) {
        if let QueryPartEvaluationContext::Aggregation {
            before,
            after,
            grouping_keys,
            default_before,
            default_after,
            ..
        } = context
        {
            let after_key = extract_grouping_value_hash(&grouping_keys, &after);
            let before_key = match &before {
                Some(before) => extract_grouping_value_hash(&grouping_keys, before),
                None => after_key,
            };

            match self.data.remove(&after_key) {
                Some((existing, before_key)) => {
                    if let QueryPartEvaluationContext::Aggregation {
                        before: existing_before,
                        ..
                    } = existing
                    {
                        self.data.insert(
                            after_key,
                            (
                                QueryPartEvaluationContext::Aggregation {
                                    before: existing_before,
                                    default_before,
                                    default_after,
                                    after,
                                    grouping_keys,
                                    row_signature: after_key,
                                },
                                before_key,
                            ),
                        );
                    }
                }
                None => {
                    self.data.insert(
                        after_key,
                        (
                            QueryPartEvaluationContext::Aggregation {
                                before,
                                after,
                                grouping_keys,
                                default_before,
                                default_after,
                                row_signature: after_key,
                            },
                            before_key,
                        ),
                    );
                }
            }
        }
    }

    fn into_vec_with_context(
        self,
        change_context: &ChangeContext,
    ) -> Vec<(QueryPartEvaluationContext, ChangeContext)> {
        self.data
            .into_iter()
            .map(|(after_key, (v, before_key))| {
                let mut change_context = change_context.clone();
                change_context.before_grouping_hash = before_key;
                change_context.after_grouping_hash = after_key;
                (v, change_context)
            })
            .collect()
    }

    fn into_result_vec(self) -> Vec<QueryPartEvaluationContext> {
        self.data
            .into_iter()
            .map(|(after_key, (ctx, _))| match ctx {
                QueryPartEvaluationContext::Aggregation {
                    before,
                    after,
                    grouping_keys,
                    default_before,
                    default_after,
                    ..
                } => QueryPartEvaluationContext::Aggregation {
                    before,
                    after,
                    grouping_keys,
                    default_before,
                    default_after,
                    row_signature: after_key,
                },
                other => other,
            })
            .collect()
    }
}

fn extract_grouping_value_hash(grouping_keys: &Vec<String>, variables: &QueryVariables) -> u64 {
    let mut hasher = SpookyHasher::default();

    for key in grouping_keys {
        match variables.get(key.as_str()) {
            Some(v) => v.hash_for_groupby(&mut hasher),
            None => 0.hash(&mut hasher),
        };
    }
    hasher.finish()
}
