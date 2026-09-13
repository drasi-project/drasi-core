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

        let is_return_aggregating = matches!(
            &part.return_clause,
            ProjectionClause::GroupBy {
                grouping: _,
                aggregates: _
            }
        );

        match context {
            QueryPartEvaluationContext::Adding { after, .. } => {
                let agg_snapshot = match &part.return_clause {
                    ProjectionClause::GroupBy {
                        grouping: _,
                        aggregates: _,
                    } => {
                        let snapshot_context = ExpressionEvaluationContext::from_before_change(
                            &after,
                            SideEffects::Snapshot,
                            change_context,
                            part,
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
                    ExpressionEvaluationContext::from_after_change(&after, change_context, part);

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
                        row_signature: 0,
                    }]),
                    _ => Ok(vec![QueryPartEvaluationContext::Adding {
                        after: data,
                        row_signature: 0,
                    }]),
                }
            }
            QueryPartEvaluationContext::Updating { before, after, .. } => {
                if before == after && !change_context.is_future_reprocess {
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
                            part,
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
                    part,
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
                        part,
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
                    ExpressionEvaluationContext::from_after_change(&after, change_context, part);
                after_context.set_side_effects(context::SideEffects::Apply);

                for filter in &part.where_clauses {
                    if !self
                        .expression_evaluator
                        .evaluate_predicate(&after_context, filter)
                        .await?
                    {
                        if let Some(agg_after) = agg_after {
                            if is_return_aggregating {
                                return self
                                    .reconcile_crossing_aggregate(
                                        part,
                                        grouping_keys,
                                        Some(before),
                                        after,
                                        before_out,
                                        agg_after,
                                        agg_snapshot,
                                        part_num,
                                        None,
                                        change_context,
                                    )
                                    .await;
                            }
                        }

                        match before_out {
                            Some(before_out) => {
                                return Ok(vec![QueryPartEvaluationContext::Removing {
                                    before: before_out,
                                    row_signature: 0,
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
                        self.reconcile_crossing_aggregate(
                            part,
                            grouping_keys,
                            Some(before),
                            after,
                            before_out,
                            after_out,
                            agg_snapshot,
                            part_num,
                            None,
                            change_context,
                        )
                        .await
                    }
                    _ => match before_out {
                        Some(before_out) => Ok(vec![QueryPartEvaluationContext::Updating {
                            before: before_out,
                            after: after_out,
                            row_signature: 0,
                        }]),
                        None => Ok(vec![QueryPartEvaluationContext::Adding {
                            after: after_out,
                            row_signature: 0,
                        }]),
                    },
                }
            }
            QueryPartEvaluationContext::Removing { before, .. } => {
                let agg_before = match &part.return_clause {
                    ProjectionClause::GroupBy {
                        grouping: _,
                        aggregates: _,
                    } => {
                        let prev_context = ExpressionEvaluationContext::from_before_change(
                            &before,
                            SideEffects::Snapshot,
                            change_context,
                            part,
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
                    part,
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
                        row_signature: 0,
                    }]),
                    _ => Ok(vec![QueryPartEvaluationContext::Removing {
                        before: data,
                        row_signature: 0,
                    }]),
                }
            }
            QueryPartEvaluationContext::Aggregation {
                before,
                after,
                grouping_keys,
                default_before,
                default_after,
                ..
            } => {
                if let Some(before) = &before {
                    if before == &after
                        && !change_context.is_future_reprocess
                        && !default_before
                        && !default_after
                    {
                        return Ok(vec![QueryPartEvaluationContext::Noop]);
                    }
                };

                let result_key = ResultKey::groupby_from_variables(&grouping_keys, &after);

                let should_revert = match &before {
                    Some(before) => {
                        if !default_before {
                            true
                        } else {
                            let before_hash = hash_variables_for_groupby(before);
                            match self
                                .read_signature_state(
                                    &result_key,
                                    &ResultOwner::PartCurrent(part_num),
                                )
                                .await?
                            {
                                Some(sig) => sig == before_hash,
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
                            }
                        }
                    }
                    None => false,
                };

                let after_hash = hash_variables_for_groupby(&after);

                let should_apply = {
                    if !default_after {
                        true
                    } else {
                        match self
                            .read_signature_state(&result_key, &ResultOwner::PartDefault(part_num))
                            .await?
                        {
                            Some(sig) => {
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
                            part,
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
                            part,
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
                        part,
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
                            part,
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
                    ExpressionEvaluationContext::from_after_change(&after, change_context, part);

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
                            if is_return_aggregating {
                                return self
                                    .reconcile_crossing_aggregate(
                                        part,
                                        next_after_grouping_keys,
                                        before,
                                        after,
                                        next_before,
                                        next_after,
                                        agg_snapshot,
                                        part_num,
                                        Some(OuterContributionTransition {
                                            before: !before_filtered && should_revert,
                                            after: false,
                                        }),
                                        change_context,
                                    )
                                    .await;
                            }
                        }

                        if !before_filtered && should_revert {
                            return Ok(vec![QueryPartEvaluationContext::Removing {
                                before: next_before.unwrap_or_default(),
                                row_signature: 0,
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
                        .reconcile_crossing_aggregate(
                            part,
                            next_after_grouping_keys,
                            before,
                            after,
                            next_before,
                            next_after,
                            agg_snapshot,
                            part_num,
                            Some(OuterContributionTransition {
                                before: !before_filtered && should_revert,
                                after: should_apply,
                            }),
                            change_context,
                        )
                        .await?),
                    _ => match (!before_filtered && should_revert, should_apply) {
                        (true, true) => Ok(vec![QueryPartEvaluationContext::Updating {
                            before: next_before.unwrap_or_default(),
                            after: next_after,
                            row_signature: 0,
                        }]),
                        (true, false) => Ok(vec![QueryPartEvaluationContext::Removing {
                            before: next_before.unwrap_or_default(),
                            row_signature: 0,
                        }]),
                        (false, true) => Ok(vec![QueryPartEvaluationContext::Adding {
                            after: next_after,
                            row_signature: 0,
                        }]),
                        (false, false) => Ok(vec![QueryPartEvaluationContext::Noop]),
                    },
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
    #[allow(clippy::too_many_arguments, clippy::unwrap_used)]
    async fn reconcile_crossing_aggregate(
        &self,
        part: &QueryPart,
        grouping_keys: Vec<String>,
        before_in: Option<QueryVariables>,
        _after_in: QueryVariables,
        before_out: Option<QueryVariables>,
        after_out: QueryVariables,
        snapshot: Option<QueryVariables>,
        part_num: usize,
        contribution: Option<OuterContributionTransition>,
        change_context: &ChangeContext,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        if let Some(contribution) = contribution {
            return self
                .reconcile_tracked_aggregate(
                    part,
                    grouping_keys,
                    before_in,
                    before_out,
                    after_out,
                    snapshot,
                    part_num,
                    contribution,
                    change_context,
                )
                .await;
        }

        if before_in.is_none() || before_out.is_none() {
            return Ok(vec![QueryPartEvaluationContext::Aggregation {
                before: before_out,
                after: after_out,
                grouping_keys,
                default_before: false,
                default_after: false,
                row_signature: 0,
            }]);
        }
        let before_in = before_in.unwrap();
        let before_out = before_out.unwrap();

        let mut grouping_match = true;
        for gk in &grouping_keys {
            let values_match = match (before_out.get(gk.as_str()), after_out.get(gk.as_str())) {
                (Some(before), Some(after)) => before.eq_for_groupby(after),
                (None, None) => true,
                _ => false,
            };
            if !values_match {
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
                row_signature: 0,
            }]);
        }

        Ok(vec![
            QueryPartEvaluationContext::Aggregation {
                before: snapshot,
                after: after_out,
                grouping_keys: grouping_keys.clone(),
                default_before: true,
                default_after: false,
                row_signature: 0,
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
                default_after: true,
                row_signature: 0,
            },
        ])
    }

    #[allow(clippy::too_many_arguments)]
    async fn reconcile_tracked_aggregate(
        &self,
        part: &QueryPart,
        grouping_keys: Vec<String>,
        before_in: Option<QueryVariables>,
        before_out: Option<QueryVariables>,
        after_out: QueryVariables,
        snapshot: Option<QueryVariables>,
        part_num: usize,
        contribution: OuterContributionTransition,
        change_context: &ChangeContext,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        if !contribution.before && !contribution.after {
            return Ok(vec![QueryPartEvaluationContext::Noop]);
        }

        if contribution.before && contribution.after {
            let before_out = before_out.ok_or(EvaluationError::InvalidContext)?;
            if grouping_values_match(&grouping_keys, &before_out, &after_out) {
                let key = ResultKey::groupby_from_variables(&grouping_keys, &after_out);
                let cardinality = self
                    .transition_group_cardinality(part_num, key, true, true)
                    .await?;
                return Ok(vec![QueryPartEvaluationContext::Aggregation {
                    before: Some(before_out),
                    after: after_out,
                    grouping_keys,
                    default_before: cardinality.before == 0,
                    default_after: cardinality.after == 0,
                    row_signature: 0,
                }]);
            }

            let before_key = ResultKey::groupby_from_variables(&grouping_keys, &before_out);
            let after_key = ResultKey::groupby_from_variables(&grouping_keys, &after_out);
            let before_cardinality = self.read_group_cardinality(part_num, &before_key).await?;
            let after_cardinality = self.read_group_cardinality(part_num, &after_key).await?;
            if before_cardinality == 0 {
                return Err(EvaluationError::InvalidGroupCardinality {
                    part_num,
                    count: before_cardinality,
                    before_contributes: true,
                    after_contributes: false,
                });
            }
            let new_before_cardinality = before_cardinality - 1;
            let new_after_cardinality = after_cardinality.checked_add(1).ok_or(
                EvaluationError::InvalidGroupCardinality {
                    part_num,
                    count: after_cardinality,
                    before_contributes: false,
                    after_contributes: true,
                },
            )?;
            self.write_group_cardinality(part_num, before_key, new_before_cardinality)
                .await?;
            self.write_group_cardinality(part_num, after_key, new_after_cardinality)
                .await?;

            let before_in = before_in.ok_or(EvaluationError::InvalidContext)?;
            let mut source_context =
                ExpressionEvaluationContext::new(&before_in, change_context.before_clock.clone());
            source_context.set_side_effects(context::SideEffects::Snapshot);
            let mut source_grouping_keys = Vec::new();
            let source_after = self
                .project(
                    &source_context,
                    &part.return_clause,
                    &mut source_grouping_keys,
                )
                .await?;

            return Ok(vec![
                QueryPartEvaluationContext::Aggregation {
                    before: snapshot,
                    after: after_out,
                    grouping_keys: grouping_keys.clone(),
                    default_before: after_cardinality == 0,
                    default_after: new_after_cardinality == 0,
                    row_signature: 0,
                },
                QueryPartEvaluationContext::Aggregation {
                    before: Some(before_out),
                    after: source_after,
                    grouping_keys,
                    default_before: before_cardinality == 0,
                    default_after: new_before_cardinality == 0,
                    row_signature: 0,
                },
            ]);
        }

        if contribution.before {
            let before_out = before_out.ok_or(EvaluationError::InvalidContext)?;
            let key = ResultKey::groupby_from_variables(&grouping_keys, &before_out);
            let cardinality = self
                .transition_group_cardinality(part_num, key, true, false)
                .await?;
            return Ok(vec![QueryPartEvaluationContext::Aggregation {
                before: Some(before_out),
                after: after_out,
                grouping_keys,
                default_before: cardinality.before == 0,
                default_after: cardinality.after == 0,
                row_signature: 0,
            }]);
        }

        let key = ResultKey::groupby_from_variables(&grouping_keys, &after_out);
        let cardinality = self
            .transition_group_cardinality(part_num, key, false, true)
            .await?;
        Ok(vec![QueryPartEvaluationContext::Aggregation {
            before: snapshot.or(before_out),
            after: after_out,
            grouping_keys,
            default_before: cardinality.before == 0,
            default_after: cardinality.after == 0,
            row_signature: 0,
        }])
    }

    async fn transition_group_cardinality(
        &self,
        part_num: usize,
        key: ResultKey,
        before_contributes: bool,
        after_contributes: bool,
    ) -> Result<GroupCardinalityTransition, EvaluationError> {
        let before = self.read_group_cardinality(part_num, &key).await?;
        if before_contributes && before == 0 {
            return Err(EvaluationError::InvalidGroupCardinality {
                part_num,
                count: before,
                before_contributes,
                after_contributes,
            });
        }

        let delta = i64::from(after_contributes) - i64::from(before_contributes);
        let after = before
            .checked_add(delta)
            .ok_or(EvaluationError::InvalidGroupCardinality {
                part_num,
                count: before,
                before_contributes,
                after_contributes,
            })?;
        if after < 0 {
            return Err(EvaluationError::InvalidGroupCardinality {
                part_num,
                count: before,
                before_contributes,
                after_contributes,
            });
        }

        self.write_group_cardinality(part_num, key, after).await?;
        Ok(GroupCardinalityTransition { before, after })
    }

    async fn read_group_cardinality(
        &self,
        part_num: usize,
        key: &ResultKey,
    ) -> Result<i64, EvaluationError> {
        match self
            .result_index
            .get(key, &ResultOwner::PartGroupCardinality(part_num))
            .await?
        {
            Some(ValueAccumulator::Count { value }) if value >= 0 => Ok(value),
            Some(_) => Err(EvaluationError::CorruptData),
            None => Ok(0),
        }
    }

    async fn read_signature_state(
        &self,
        key: &ResultKey,
        owner: &ResultOwner,
    ) -> Result<Option<u64>, EvaluationError> {
        match self.result_index.get(key, owner).await? {
            Some(ValueAccumulator::Signature(signature)) => Ok(Some(signature)),
            Some(_) => Err(EvaluationError::CorruptData),
            None => Ok(None),
        }
    }

    async fn write_group_cardinality(
        &self,
        part_num: usize,
        key: ResultKey,
        count: i64,
    ) -> Result<(), EvaluationError> {
        let value = (count > 0).then_some(ValueAccumulator::Count { value: count });
        self.result_index
            .set(key, ResultOwner::PartGroupCardinality(part_num), value)
            .await?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct OuterContributionTransition {
    before: bool,
    after: bool,
}

struct GroupCardinalityTransition {
    before: i64,
    after: i64,
}

fn grouping_values_match(
    grouping_keys: &[String],
    before: &QueryVariables,
    after: &QueryVariables,
) -> bool {
    grouping_keys.iter().all(
        |key| match (before.get(key.as_str()), after.get(key.as_str())) {
            (Some(before), Some(after)) => before.eq_for_groupby(after),
            (None, None) => true,
            _ => false,
        },
    )
}

fn hash_variables_for_groupby(variables: &QueryVariables) -> u64 {
    let mut hasher = SpookyHasher::default();
    for (name, value) in variables {
        name.hash(&mut hasher);
        value.hash_for_groupby(&mut hasher);
    }
    hasher.finish()
}
