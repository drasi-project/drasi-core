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
    variable_value::VariableValue,
    EvaluationError, ExpressionEvaluationContext,
};
use drasi_query_ast::ast::{Expression, ProjectionClause, QueryPart};
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
                        grouping,
                        aggregates,
                    } => {
                        let mut id_context = eval_context.clone();
                        id_context.set_output_grouping_key(grouping);
                        let grouping_values =
                            self.evaluate_grouping_values(&id_context, grouping).await?;
                        let at_identity = self
                            .expression_evaluator
                            .projection_is_at_identity(&id_context, aggregates, &grouping_values)
                            .await?;

                        if at_identity {
                            if let Some(before) = agg_before {
                                return Ok(vec![QueryPartEvaluationContext::Removing {
                                    before,
                                    row_signature: 0,
                                }]);
                            }
                        }

                        Ok(vec![QueryPartEvaluationContext::Aggregation {
                            before: agg_before,
                            after: data,
                            grouping_keys,
                            default_before: false,
                            row_signature: 0,
                        }])
                    }
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
                ..
            } => {
                if let Some(before) = &before {
                    if before == &after && !change_context.is_future_reprocess && !default_before {
                        return Ok(vec![QueryPartEvaluationContext::Noop]);
                    }
                };

                let result_key = ResultKey::groupby_from_variables(&grouping_keys, &after);

                // When the upstream tags `default_before: true`, the `before`
                // *might* be the synthetic identity row for a fresh group;
                // the PartCurrent ledger below disambiguates "first-time
                // default" vs "continuation that happens to be tagged
                // default" by comparing `before` against the part's last
                // emitted `after`.
                let should_revert = match &before {
                    Some(before) => {
                        if !default_before {
                            true
                        } else {
                            let mut before_hash = SpookyHasher::default();
                            before.hash(&mut before_hash);
                            let before_hash = before_hash.finish();
                            matches!(
                                self.result_index
                                    .get(&result_key, &ResultOwner::PartCurrent(part_num))
                                    .await?,
                                Some(ValueAccumulator::Signature(sig)) if sig == before_hash,
                            )
                        }
                    }
                    None => false,
                };

                let mut after_hash = SpookyHasher::default();
                after.hash(&mut after_hash);
                let after_hash = after_hash.finish();

                self.result_index
                    .set(
                        result_key,
                        ResultOwner::PartCurrent(part_num),
                        Some(ValueAccumulator::Signature(after_hash)),
                    )
                    .await?;

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

                if let Some(before_inner) = &before {
                    let before_context = ExpressionEvaluationContext::from_before_change(
                        before_inner,
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
                            before_inner,
                            SideEffects::RevertForUpdate,
                            change_context,
                            part,
                        );
                        revert_context.replace_variables(before_inner);
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

                let after_context =
                    ExpressionEvaluationContext::from_after_change(&after, change_context, part);

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
                                        change_context,
                                    )
                                    .await;
                            }
                        }

                        if before.is_some() && !before_filtered {
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
                            change_context,
                        )
                        .await?),
                    _ => {
                        if before_filtered || !should_revert {
                            Ok(vec![QueryPartEvaluationContext::Adding {
                                after: next_after,
                                row_signature: 0,
                            }])
                        } else {
                            Ok(vec![QueryPartEvaluationContext::Updating {
                                before: next_before.unwrap_or_default(),
                                after: next_after,
                                row_signature: 0,
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

    async fn evaluate_grouping_values(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        grouping: &[Expression],
    ) -> Result<Arc<Vec<VariableValue>>, EvaluationError> {
        let mut values = Vec::with_capacity(grouping.len());
        for expr in grouping {
            values.push(
                self.expression_evaluator
                    .evaluate_expression(context, expr)
                    .await?,
            );
        }
        Ok(Arc::new(values))
    }

    /// Reconciles values crossing from one group to another. When a row moves
    /// out of its source group and the source group's aggregates all report
    /// identity (no remaining contributors), the source-side emission is a
    /// `Removing` rather than an `Aggregation` so the consumer drops the row
    /// instead of retaining it at its identity values.
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
        change_context: &ChangeContext,
    ) -> Result<Vec<QueryPartEvaluationContext>, EvaluationError> {
        if before_in.is_none() || before_out.is_none() {
            return Ok(vec![QueryPartEvaluationContext::Aggregation {
                before: before_out,
                after: after_out,
                grouping_keys,
                default_before: false,
                row_signature: 0,
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
            // Same-group case. The row stayed in (or was filtered out of) its
            // original group; `after_out` already reflects the post-revert /
            // post-apply state. If the group is now empty, emit `Removing`
            // to drop the row, mirroring the cross-group branch below.
            let same_group_emission = match &part.return_clause {
                ProjectionClause::GroupBy {
                    grouping,
                    aggregates,
                } => {
                    let prev_context = ExpressionEvaluationContext::from_before_change(
                        &before_in,
                        SideEffects::Snapshot,
                        change_context,
                        part,
                    );
                    let grouping_values = self
                        .evaluate_grouping_values(&prev_context, grouping)
                        .await?;
                    let at_identity = self
                        .expression_evaluator
                        .projection_is_at_identity(&prev_context, aggregates, &grouping_values)
                        .await?;

                    if at_identity {
                        QueryPartEvaluationContext::Removing {
                            before: before_out,
                            row_signature: 0,
                        }
                    } else {
                        QueryPartEvaluationContext::Aggregation {
                            before: Some(before_out),
                            after: after_out,
                            grouping_keys,
                            default_before: false,
                            row_signature: 0,
                        }
                    }
                }
                _ => QueryPartEvaluationContext::Aggregation {
                    before: Some(before_out),
                    after: after_out,
                    grouping_keys,
                    default_before: false,
                    row_signature: 0,
                },
            };
            return Ok(vec![same_group_emission]);
        }

        // Cross-group: the row left `before_out`'s group and entered
        // `after_out`'s. Recompute the source group's post-revert state and
        // determine whether it has emptied. Use `from_before_change` so the
        // context carries the change's anchor, solution signature, and input
        // grouping hash, matching how every other branch of `evaluate`
        // constructs its contexts.
        let prev_context = ExpressionEvaluationContext::from_before_change(
            &before_in,
            SideEffects::Snapshot,
            change_context,
            part,
        );

        let mut source_after_grouping_keys = Vec::new();
        let source_after = self
            .project(
                &prev_context,
                &part.return_clause,
                &mut source_after_grouping_keys,
            )
            .await?;

        let source_emission = match &part.return_clause {
            ProjectionClause::GroupBy {
                grouping,
                aggregates,
            } => {
                let grouping_values = self
                    .evaluate_grouping_values(&prev_context, grouping)
                    .await?;
                let at_identity = self
                    .expression_evaluator
                    .projection_is_at_identity(&prev_context, aggregates, &grouping_values)
                    .await?;

                if at_identity {
                    QueryPartEvaluationContext::Removing {
                        before: before_out,
                        row_signature: 0,
                    }
                } else {
                    QueryPartEvaluationContext::Aggregation {
                        before: Some(before_out),
                        after: source_after,
                        grouping_keys: grouping_keys.clone(),
                        default_before: false,
                        row_signature: 0,
                    }
                }
            }
            _ => QueryPartEvaluationContext::Aggregation {
                before: Some(before_out),
                after: source_after,
                grouping_keys: grouping_keys.clone(),
                default_before: false,
                row_signature: 0,
            },
        };

        Ok(vec![
            QueryPartEvaluationContext::Aggregation {
                before: snapshot,
                after: after_out,
                grouping_keys: grouping_keys.clone(),
                default_before: false,
                row_signature: 0,
            },
            source_emission,
        ])
    }
}
