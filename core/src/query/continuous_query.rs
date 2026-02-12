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

use std::{
    collections::HashMap,
    fmt::Debug,
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
    interface::{ElementIndex, FutureQueue, FutureQueueConsumer, MiddlewareError, QueryClock},
    middleware::SourceMiddlewarePipelineCollection,
    models::{Element, SourceChange},
    path_solver::{
        match_path::{MatchPath, SlotElementSpec},
        solution::{MatchPathSolution, SolutionSignature},
        MatchPathSolver, MatchSolveContext,
    },
};

pub struct ContinuousQuery {
    expression_evaluator: Arc<ExpressionEvaluator>,
    part_evaluator: Arc<QueryPartEvaluator>,
    element_index: Arc<dyn ElementIndex>,
    path_solver: Arc<MatchPathSolver>,
    match_path: Arc<MatchPath>,
    query: Arc<Query>,
    future_consumer_shutdown_request: Arc<Notify>,
    future_queue: Arc<dyn FutureQueue>,
    future_queue_task: Mutex<Option<JoinHandle<()>>>,
    change_lock: Mutex<()>,
    source_pipelines: SourceMiddlewarePipelineCollection,
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
    ) -> Self {
        Self {
            expression_evaluator,
            element_index,
            path_solver,
            match_path,
            part_evaluator,
            query,
            future_consumer_shutdown_request: Arc::new(Notify::new()),
            future_queue,
            future_queue_task: Mutex::new(None),
            change_lock: Mutex::new(()),
            source_pipelines,
        }
    }

    #[tracing::instrument(skip_all, err, level = "debug")]
    pub async fn process_source_change(
        &self,
        change: SourceChange,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        //println!("-> process_source_change {:?}", change);
        let _lock = self.change_lock.lock().await;
        let mut result = Vec::new();

        let changes = self.execute_source_middleware(change).await?;

        for change in changes {
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

            for (solution_signature, part_context) in solution_changes.changes {
                let change_results = self
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
                        },
                    )
                    .await?;
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
        //println!("--> process_source_change result {:?}", result);
        Ok(result)
    }

    #[tracing::instrument(skip_all, err, level = "debug")]
    async fn build_solution_changes(
        &self,
        base_variables: &QueryVariables,
        change: SourceChange,
        clock: Arc<dyn QueryClock>,
    ) -> Result<SolutionChangesResult, EvaluationError> {
        let mut result = SolutionChangesResult::new();
        let mut before_change_solutions = HashMap::new();
        let mut after_change_solutions = HashMap::new();

        match change {
            SourceChange::Insert { element } => {
                let element = Arc::new(element);
                let affinity_slots = self
                    .get_slots_with_affinity(base_variables, element.clone(), clock.clone())
                    .await?;
                let solutions = self
                    .resolve_solutions(element.clone(), affinity_slots, true)
                    .await?;

                for (signature, solution) in solutions {
                    if let Some(blank_optional_solution) =
                        solution.get_empty_optional_solution(&self.match_path)
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
                            base_variables,
                            prev_version.clone(),
                            before_clock.clone(),
                        )
                        .await?;
                    let solutions = self
                        .resolve_solutions(prev_version.clone(), affinity_slots, false)
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
                    .get_slots_with_affinity(base_variables, element.clone(), clock.clone())
                    .await?;
                let solutions = self
                    .resolve_solutions(element.clone(), affinity_slots, true)
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
                            base_variables,
                            element.clone(),
                            before_clock.clone(),
                        )
                        .await?;
                    let solutions = self
                        .resolve_solutions(element.clone(), affinity_slots, false)
                        .await?;
                    for (signature, solution) in solutions {
                        if let Some(blank_optional_solution) =
                            solution.get_empty_optional_solution(&self.match_path)
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
                    let prev_timestamp = element.get_effective_from();
                    if prev_timestamp >= future_ref.due_time {
                        // element already processed with due time expired, don't duplicate
                        return Ok(result);
                    }

                    let before_clock =
                        Arc::new(InstantQueryClock::new(prev_timestamp, prev_timestamp));

                    let affinity_slots = self
                        .get_slots_with_affinity(
                            base_variables,
                            element.clone(),
                            before_clock.clone(),
                        )
                        .await?;

                    let before_solutions = self
                        .resolve_solutions(element.clone(), affinity_slots, false)
                        .await?;
                    for (signature, solution) in before_solutions {
                        before_change_solutions.insert(signature, solution);
                    }

                    result.before_clock = Some(before_clock);
                    result.before_anchor_element = Some(element.clone());

                    let affinity_slots = self
                        .get_slots_with_affinity(base_variables, element.clone(), clock.clone())
                        .await?;

                    let after_solutions = self
                        .resolve_solutions(element.clone(), affinity_slots, false)
                        .await?;
                    for (signature, solution) in after_solutions {
                        after_change_solutions.insert(signature, solution);
                    }

                    result.anchor_element = Some(element);
                }
            }
        }

        for (sig, before_sol) in &before_change_solutions {
            match after_change_solutions.get(sig) {
                Some(after_sol) => result.changes.push((
                    *sig,
                    QueryPartEvaluationContext::Updating {
                        before: before_sol.into_query_variables(&self.match_path, base_variables),
                        after: after_sol.into_query_variables(&self.match_path, base_variables),
                        row_signature: 0,
                    },
                )),
                None => result.changes.push((
                    *sig,
                    QueryPartEvaluationContext::Removing {
                        before: before_sol.into_query_variables(&self.match_path, base_variables),
                        row_signature: 0,
                    },
                )),
            }
        }

        for (sig, after_sol) in &after_change_solutions {
            if !before_change_solutions.contains_key(sig) {
                result.changes.push((
                    *sig,
                    QueryPartEvaluationContext::Adding {
                        after: after_sol.into_query_variables(&self.match_path, base_variables),
                        row_signature: 0,
                    },
                ))
            }
        }

        Ok(result)
    }

    async fn resolve_solutions(
        &self,
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
            let solution = self
                .path_solver
                .solve(self.match_path.clone(), anchor_element.clone(), slot_num)
                .await?;
            result.extend(solution.into_iter());
        }

        Ok(result)
    }

    async fn get_slots_with_affinity(
        &self,
        variables: &QueryVariables,
        anchor_element: Arc<Element>,
        clock: Arc<dyn QueryClock>,
    ) -> Result<Vec<usize>, EvaluationError> {
        let context = MatchSolveContext::new(variables, clock);

        let mut affinity_slots = Vec::new();

        for (slot_num, slot) in self.match_path.slots.iter().enumerate() {
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
    async fn execute_source_middleware(
        &self,
        change: SourceChange,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        let source_id = change.get_reference().source_id.clone();
        let mut source_changes = vec![change];

        let pipeline = match self.source_pipelines.get(source_id) {
            Some(pipeline) => pipeline,
            None => return Ok(source_changes),
        };

        let mut new_source_changes = Vec::new();
        for source_change in source_changes {
            new_source_changes.append(
                &mut pipeline
                    .process(source_change, self.element_index.clone())
                    .await?,
            );
        }

        source_changes = new_source_changes;

        Ok(source_changes)
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
                        let fut_ref = match peek {
                            Ok(Some(due_time)) => {
                                if due_time > consumer.now() {
                                    tokio::time::sleep(idle_interval).await;
                                    continue;
                                }
                                match queue.pop().await {
                                    Ok(Some(element)) => element,
                                    Ok(None) => {
                                        tokio::time::sleep(idle_interval).await;
                                        continue;
                                    }
                                    Err(e) => {
                                        log::error!("Future queue consumer error: {e:?}");
                                        tokio::time::sleep(error_interval).await;
                                        continue;
                                    }
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

                        log::info!("Future queue consumer processing {}", &fut_ref.element_ref);

                        match consumer.on_due(&fut_ref).await {
                            Ok(_) => log::info!("Future queue consumer processed {}", &fut_ref.element_ref),
                            Err(e) => {
                                log::error!("Future queue consumer error: {e:?}");
                                consumer.on_error(&fut_ref, e).await;
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

struct SolutionChangesResult {
    pub changes: Vec<(SolutionSignature, QueryPartEvaluationContext)>,
    pub anchor_element: Option<Arc<Element>>,
    pub before_clock: Option<Arc<dyn QueryClock>>,
    pub before_anchor_element: Option<Arc<Element>>,
    pub is_future_reprocess: bool,
}

impl SolutionChangesResult {
    fn new() -> Self {
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
