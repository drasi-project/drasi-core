#[cfg(test)]
mod tests;

use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

use crate::{
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{ResultIndex, ResultKey, ResultOwner},
};

use super::{
    context::{self, ChangeContext, SideEffects},
    expressions::*,
    EvaluationError, ExpressionEvaluationContext,
};
use drasi_query_ast::ast::{ProjectionClause, QueryPart};
use hashers::jenkins::spooky_hash::SpookyHasher;

use super::context::{QueryPartEvaluationContext, QueryVariables};

pub struct QueryPartEvaluator {
    expression_evaluator: Arc<ExpressionEvaluator>,
    result_index: Arc<dyn ResultIndex>,
}

impl QueryPartEvaluator {
    pub fn new(
        expression_evaluator: Arc<ExpressionEvaluator>,
        result_index: Arc<dyn ResultIndex>,
    ) -> QueryPartEvaluator {
        QueryPartEvaluator {
            expression_evaluator,
            result_index,
        }
    }

    pub async fn evaluate(
        &self,
        context: QueryPartEvaluationContext,
        part_num: usize,
        part: &QueryPart,
        change_context: &ChangeContext,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        // println!("Evaluating : {:#?}", context);

        let is_return_aggreating = match &part.return_clause {
            ProjectionClause::GroupBy {
                grouping: _,
                aggregates: _,
            } => true,
            _ => false,
        };

        match context {
            QueryPartEvaluationContext::Adding { after } => {
                let agg_snapshot = match &part.return_clause {
                    ProjectionClause::GroupBy {
                        grouping: _,
                        aggregates: _,
                    } => {
                        let snapshot_context = ExpressionEvaluationContext::from_before_change(
                            &after,
                            SideEffects::Snapshot,
                            change_context,
                        );
                        let mut grouping_keys = Vec::new();
                        Some(
                            self.project(
                                &snapshot_context,
                                &part.return_clause,
                                &mut grouping_keys,
                            )
                            .await?,
                        )
                    }
                    _ => None,
                };

                let eval_context =
                    ExpressionEvaluationContext::from_after_change(&after, change_context);

                let mut grouping_keys = Vec::new();

                for filter in &part.where_clauses {
                    if !self
                        .expression_evaluator
                        .evaluate_predicate(&eval_context, filter)
                        .await?
                    {
                        return Ok(vec![QueryPartEvaluationContext::Noop]);
                    }
                }

                let data = self
                    .project(&eval_context, &part.return_clause, &mut grouping_keys)
                    .await?;

                match &part.return_clause {
                    ProjectionClause::GroupBy {
                        grouping: _,
                        aggregates: _,
                    } => Ok(vec![QueryPartEvaluationContext::Aggregation {
                        before: agg_snapshot,
                        after: data,
                        grouping_keys,
                        default_before: true,
                        default_after: false,
                    }]),
                    _ => Ok(vec![QueryPartEvaluationContext::Adding { after: data }]),
                }
            }
            QueryPartEvaluationContext::Updating { before, after } => {
                if &before == &after && !change_context.is_future_reprocess {
                    return Ok(vec![QueryPartEvaluationContext::Noop]);
                };

                let mut grouping_keys = Vec::new();

                let agg_snapshot = match &part.return_clause {
                    ProjectionClause::GroupBy {
                        grouping: _,
                        aggregates: _,
                    } => {
                        let snapshot_context = ExpressionEvaluationContext::from_before_change(
                            &after,
                            SideEffects::Snapshot,
                            change_context,
                        );
                        Some(
                            self.project(
                                &snapshot_context,
                                &part.return_clause,
                                &mut grouping_keys,
                            )
                            .await?,
                        )
                    }
                    _ => None,
                };

                let mut before_out: Option<QueryVariables> = None;
                let before_context = ExpressionEvaluationContext::from_before_change(
                    &before,
                    SideEffects::Snapshot,
                    change_context,
                );
                let mut before_filtered = false;

                for filter in &part.where_clauses {
                    before_filtered = before_filtered
                        || !self
                            .expression_evaluator
                            .evaluate_predicate(&before_context, filter)
                            .await?;
                }

                let mut agg_after: Option<QueryVariables> = None;
                let mut agg_after_grouping_keys = Vec::new();

                if !before_filtered {
                    before_out = Some(
                        self.project(&before_context, &part.return_clause, &mut grouping_keys)
                            .await?,
                    );

                    let revert_context = ExpressionEvaluationContext::from_before_change(
                        &before,
                        SideEffects::RevertForUpdate,
                        change_context,
                    );

                    agg_after = Some(
                        self.project(
                            &revert_context,
                            &part.return_clause,
                            &mut agg_after_grouping_keys,
                        )
                        .await?,
                    );
                }

                let mut after_context =
                    ExpressionEvaluationContext::from_after_change(&after, change_context);
                after_context.set_side_effects(context::SideEffects::Apply);

                for filter in &part.where_clauses {
                    if !self
                        .expression_evaluator
                        .evaluate_predicate(&after_context, filter)
                        .await?
                    {
                        if let Some(agg_after) = agg_after {
                            if is_return_aggreating {
                                return self
                                    .reconile_crossing_agregate(
                                        part,
                                        grouping_keys,
                                        Some(before),
                                        after,
                                        before_out,
                                        agg_after,
                                        agg_snapshot,
                                        change_context,
                                    )
                                    .await;
                            }
                        }

                        match before_out {
                            Some(before_out) => {
                                return Ok(vec![QueryPartEvaluationContext::Removing {
                                    before: before_out,
                                }])
                            }
                            None => return Ok(vec![QueryPartEvaluationContext::Noop]),
                        };
                    }
                }

                let after_out = self
                    .project(&after_context, &part.return_clause, &mut grouping_keys)
                    .await?;

                match &part.return_clause {
                    ProjectionClause::GroupBy {
                        grouping: _,
                        aggregates: _,
                    } => {
                        self.reconile_crossing_agregate(
                            part,
                            grouping_keys,
                            Some(before),
                            after,
                            before_out,
                            after_out,
                            agg_snapshot,
                            change_context,
                        )
                        .await
                    }
                    _ => match before_out {
                        Some(before_out) => Ok(vec![QueryPartEvaluationContext::Updating {
                            before: before_out,
                            after: after_out,
                        }]),
                        None => Ok(vec![QueryPartEvaluationContext::Adding { after: after_out }]),
                    },
                }
            }
            QueryPartEvaluationContext::Removing { before } => {
                let agg_before = match &part.return_clause {
                    ProjectionClause::GroupBy {
                        grouping: _,
                        aggregates: _,
                    } => {
                        let prev_context = ExpressionEvaluationContext::from_before_change(
                            &before,
                            SideEffects::Snapshot,
                            change_context,
                        );
                        let mut grouping_keys = Vec::new();
                        Some(
                            self.project(&prev_context, &part.return_clause, &mut grouping_keys)
                                .await?,
                        )
                    }
                    _ => None,
                };

                let eval_context = ExpressionEvaluationContext::from_before_change(
                    &before,
                    SideEffects::RevertForDelete,
                    change_context,
                );
                let mut grouping_keys = Vec::new();

                for filter in &part.where_clauses {
                    if !self
                        .expression_evaluator
                        .evaluate_predicate(&eval_context, filter)
                        .await?
                    {
                        return Ok(vec![QueryPartEvaluationContext::Noop]);
                    }
                }

                let data = self
                    .project(&eval_context, &part.return_clause, &mut grouping_keys)
                    .await?;

                match &part.return_clause {
                    ProjectionClause::GroupBy {
                        grouping: _,
                        aggregates: _,
                    } => Ok(vec![QueryPartEvaluationContext::Aggregation {
                        before: agg_before,
                        after: data,
                        grouping_keys,
                        default_before: false,
                        default_after: true,
                    }]),
                    _ => Ok(vec![QueryPartEvaluationContext::Removing { before: data }]),
                }
            }
            QueryPartEvaluationContext::Aggregation {
                before,
                after,
                grouping_keys,
                default_before,
                default_after,
            } => {
                if let Some(before) = &before {
                    if before == &after && !change_context.is_future_reprocess && !default_before {
                        return Ok(vec![QueryPartEvaluationContext::Noop]);
                    }
                };

                let result_key = ResultKey::groupby_from_variables(&grouping_keys, &after);

                let should_revert = match &before {
                    Some(before) => {
                        if !default_before {
                            true
                        } else {
                            let mut before_hash = SpookyHasher::default();
                            before.hash(&mut before_hash);
                            let before_hash = before_hash.finish();
                            match self
                                .result_index
                                .get(&result_key, &ResultOwner::PartCurrent(part_num))
                                .await?
                            {
                                Some(ValueAccumulator::Signature(sig)) => sig == before_hash,
                                None => {
                                    self.result_index
                                        .set(
                                            result_key.clone(),
                                            ResultOwner::PartDefault(part_num),
                                            Some(ValueAccumulator::Signature(before_hash)),
                                        )
                                        .await?;
                                    false
                                }
                                _ => false,
                            }
                        }
                    }
                    None => false,
                };

                let mut after_hash = SpookyHasher::default();
                after.hash(&mut after_hash);
                let after_hash = after_hash.finish();

                let should_apply = {
                    if !default_after {
                        true
                    } else {
                        match self
                            .result_index
                            .get(&result_key, &ResultOwner::PartDefault(part_num))
                            .await?
                        {
                            Some(ValueAccumulator::Signature(sig)) => {
                                if sig == after_hash {
                                    self.result_index
                                        .set(
                                            result_key.clone(),
                                            ResultOwner::PartCurrent(part_num),
                                            None,
                                        )
                                        .await?;
                                }
                                sig != after_hash
                            }
                            None => true,
                            _ => true,
                        }
                    }
                };

                if should_apply {
                    self.result_index
                        .set(
                            result_key,
                            ResultOwner::PartCurrent(part_num),
                            Some(ValueAccumulator::Signature(after_hash)),
                        )
                        .await?;
                }

                let agg_snapshot = match &part.return_clause {
                    ProjectionClause::GroupBy {
                        grouping: _,
                        aggregates: _,
                    } => {
                        let snapshot_context = ExpressionEvaluationContext::from_before_change(
                            &after,
                            SideEffects::Snapshot,
                            change_context,
                        );
                        let mut grouping_keys = Vec::new();
                        Some(
                            self.project(
                                &snapshot_context,
                                &part.return_clause,
                                &mut grouping_keys,
                            )
                            .await?,
                        )
                    }
                    _ => None,
                };

                let mut next_before_grouping_keys = Vec::new();
                let next_before = match &before {
                    Some(before) => {
                        let prev_context = ExpressionEvaluationContext::from_before_change(
                            before,
                            SideEffects::Snapshot,
                            change_context,
                        );
                        Some(
                            self.project(
                                &prev_context,
                                &part.return_clause,
                                &mut next_before_grouping_keys,
                            )
                            .await?,
                        )
                    }
                    None => None,
                };

                let mut before_filtered = false;
                let mut next_after: Option<QueryVariables> = None;
                let mut next_after_grouping_keys = Vec::new();

                if let Some(before) = &before {
                    let before_context = ExpressionEvaluationContext::from_before_change(
                        before,
                        SideEffects::Snapshot,
                        change_context,
                    );

                    for filter in &part.where_clauses {
                        before_filtered = before_filtered
                            || !self
                                .expression_evaluator
                                .evaluate_predicate(&before_context, filter)
                                .await?;
                    }

                    if !before_filtered && should_revert {
                        let mut revert_context = ExpressionEvaluationContext::from_before_change(
                            before,
                            SideEffects::RevertForUpdate,
                            change_context,
                        );
                        revert_context.replace_variables(before);
                        next_after = Some(
                            self.project(
                                &revert_context,
                                &part.return_clause,
                                &mut next_after_grouping_keys,
                            )
                            .await?,
                        );
                    }
                }

                let mut after_context =
                    ExpressionEvaluationContext::from_after_change(&after, change_context);

                if !should_apply {
                    after_context.set_side_effects(SideEffects::Snapshot);
                }

                for filter in &part.where_clauses {
                    if !self
                        .expression_evaluator
                        .evaluate_predicate(&after_context, filter)
                        .await?
                    {
                        if let Some(next_after) = next_after {
                            if is_return_aggreating {
                                return self
                                    .reconile_crossing_agregate(
                                        part,
                                        next_after_grouping_keys,
                                        before,
                                        after,
                                        next_before,
                                        next_after,
                                        agg_snapshot,
                                        change_context,
                                    )
                                    .await;
                            }
                        }

                        if before.is_some() && !before_filtered {
                            return Ok(vec![QueryPartEvaluationContext::Removing {
                                before: next_before.unwrap_or_default(),
                            }]);
                        } else {
                            return Ok(vec![QueryPartEvaluationContext::Noop]);
                        }
                    }
                }

                let next_after = self
                    .project(
                        &after_context,
                        &part.return_clause,
                        &mut next_after_grouping_keys,
                    )
                    .await?;

                match &part.return_clause {
                    ProjectionClause::GroupBy {
                        grouping: _,
                        aggregates: _,
                    } => Ok(self
                        .reconile_crossing_agregate(
                            part,
                            next_after_grouping_keys,
                            before,
                            after,
                            next_before,
                            next_after,
                            agg_snapshot,
                            change_context,
                        )
                        .await?),
                    _ => {
                        if before_filtered || !should_revert {
                            Ok(vec![QueryPartEvaluationContext::Adding { after: next_after }])
                        } else {
                            Ok(vec![QueryPartEvaluationContext::Updating {
                                before: next_before.unwrap_or_default(),
                                after: next_after,
                            }])
                        }
                    }
                }
            }
            QueryPartEvaluationContext::Noop => Ok(vec![context]),
        }
    }

    async fn project(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        projection: &ProjectionClause,
        grouping_keys: &mut Vec<String>,
    ) -> Result<QueryVariables, EvaluationError> {
        grouping_keys.clear();
        match projection {
            ProjectionClause::Item(expressions) => {
                let mut result = QueryVariables::new();

                for expr in expressions {
                    let (name, value) = self
                        .expression_evaluator
                        .evaluate_projection_field(context, expr)
                        .await?;
                    result.insert(name.into_boxed_str(), value);
                }

                Ok(result)
            }
            ProjectionClause::GroupBy {
                grouping,
                aggregates,
            } => {
                let mut result = QueryVariables::new();

                for expr in grouping {
                    let (name, value) = self
                        .expression_evaluator
                        .evaluate_projection_field(context, expr)
                        .await?;
                    result.insert(name.clone().into_boxed_str(), value);
                    grouping_keys.push(name);
                }

                let mut agg_context = context.clone();
                agg_context.set_output_grouping_key(grouping);

                for expr in aggregates {
                    let (name, value) = self
                        .expression_evaluator
                        .evaluate_projection_field(&agg_context, expr)
                        .await?;
                    result.insert(name.into_boxed_str(), value);
                }

                Ok(result)
            }
        }
    }

    /// Reconciles values crossing from one group to another
    async fn reconile_crossing_agregate(
        &self,
        part: &QueryPart,
        grouping_keys: Vec<String>,
        before_in: Option<QueryVariables>,
        _after_in: QueryVariables,
        before_out: Option<QueryVariables>,
        after_out: QueryVariables,
        snapshot: Option<QueryVariables>,
        change_context: &ChangeContext,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        if before_in.is_none() || before_out.is_none() {
            return Ok(vec![QueryPartEvaluationContext::Aggregation {
                before: before_out,
                after: after_out,
                grouping_keys,
                default_before: false,
                default_after: false,
            }]);
        }
        let before_in = before_in.unwrap();
        let before_out = before_out.unwrap();

        let mut grouping_match = true;
        for gk in &grouping_keys {
            if before_out.get(gk.as_str()) != after_out.get(gk.as_str()) {
                grouping_match = false;
                break;
            }
        }

        if grouping_match {
            return Ok(vec![QueryPartEvaluationContext::Aggregation {
                before: Some(before_out),
                after: after_out,
                grouping_keys,
                default_before: false,
                default_after: false,
            }]);
        }

        Ok(vec![
            QueryPartEvaluationContext::Aggregation {
                before: snapshot,
                after: after_out,
                grouping_keys: grouping_keys.clone(),
                default_before: false,
                default_after: false,
            },
            QueryPartEvaluationContext::Aggregation {
                before: Some(before_out),
                after: {
                    let mut prev_context = ExpressionEvaluationContext::new(
                        &before_in,
                        change_context.before_clock.clone(),
                    );
                    prev_context.set_side_effects(context::SideEffects::Snapshot);
                    let mut grouping_keys = Vec::new();
                    self.project(&prev_context, &part.return_clause, &mut grouping_keys)
                        .await?
                },
                grouping_keys: grouping_keys.clone(),
                default_before: false,
                default_after: false,
            },
        ])
    }
}
