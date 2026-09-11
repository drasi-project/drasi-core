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

use std::{collections::HashMap, sync::Arc};

use crate::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        EvaluationError, ExpressionEvaluator, InstantQueryClock,
    },
    interface::{ElementIndex, QueryClock},
    models::{Element, ElementReference, SourceChange},
    path_solver::variable_length::{
        GraphView, VariableLengthMatchPlan, VariableLengthSolver, WorkBudget,
    },
};

use super::continuous_query::{SolutionChange, SolutionChangesResult};

pub(super) enum IndexWrite {
    Set(Arc<Element>, Vec<usize>),
    Delete(ElementReference),
    None,
}

pub(super) struct PreparedChange {
    pub solutions: SolutionChangesResult,
    pub write: IndexWrite,
}

pub(super) async fn prepare(
    plan: &VariableLengthMatchPlan,
    index: &dyn ElementIndex,
    evaluator: &ExpressionEvaluator,
    change: SourceChange,
    after_clock: Arc<dyn QueryClock>,
    base: &QueryVariables,
) -> Result<PreparedChange, EvaluationError> {
    let mut budget = WorkBudget::new(&plan.limits);
    let reference = change.get_reference().clone();
    let before = index.get_element(&reference).await?;
    let mut result = SolutionChangesResult::new();
    let mut future_original_time = None;
    let (after, write) = match change {
        SourceChange::Insert { element } => {
            let slots = plan.affinity(&element);
            let element = Arc::new(element);
            (Some(element.clone()), IndexWrite::Set(element, slots))
        }
        SourceChange::Update { mut element } => {
            if let Some(before) = &before {
                if std::mem::discriminant(&element) != std::mem::discriminant(before.as_ref()) {
                    return Err(EvaluationError::InvalidArgument);
                }
                element.merge_missing_properties(before);
            }
            let slots = plan.affinity(&element);
            let element = Arc::new(element);
            (Some(element.clone()), IndexWrite::Set(element, slots))
        }
        SourceChange::Delete { .. } => (None, IndexWrite::Delete(reference.clone())),
        SourceChange::Future { future_ref } => {
            result.is_future_reprocess = true;
            future_original_time = Some(future_ref.original_time);
            match &before {
                Some(element) => (Some(element.clone()), IndexWrite::None),
                None => {
                    return Ok(PreparedChange {
                        solutions: result,
                        write: IndexWrite::None,
                    });
                }
            }
        }
    };
    let before_clock: Arc<dyn QueryClock> = if let Some(original_time) = future_original_time {
        Arc::new(InstantQueryClock::new(original_time, original_time))
    } else {
        match &before {
            Some(element) => Arc::new(InstantQueryClock::new(
                element.get_effective_from(),
                after_clock.get_realtime(),
            )),
            None => after_clock.clone(),
        }
    };
    let old_solutions = match &before {
        Some(element) => {
            VariableLengthSolver {
                plan,
                graph: GraphView::current(index),
                evaluator,
                clock: before_clock.clone(),
            }
            .affected(element.clone(), &mut budget)
            .await?
        }
        None => HashMap::new(),
    };
    let mut new_solutions = match &after {
        Some(element) => {
            let graph = if result.is_future_reprocess {
                GraphView::current(index)
            } else {
                GraphView::changed(index, &reference, element.clone())
            };
            VariableLengthSolver {
                plan,
                graph,
                evaluator,
                clock: after_clock,
            }
            .affected(element.clone(), &mut budget)
            .await?
        }
        None => HashMap::new(),
    };
    result.before_clock = Some(before_clock);
    result.before_anchor_element = before;
    result.anchor_element = after;
    for (key, before) in old_solutions {
        let context = match new_solutions.remove(&key) {
            Some(after) => QueryPartEvaluationContext::Updating {
                before: before.variables(plan, base),
                after: after.variables(plan, base),
                row_signature: 0,
            },
            None => QueryPartEvaluationContext::Removing {
                before: before.variables(plan, base),
                row_signature: 0,
            },
        };
        result.changes.push(SolutionChange {
            signature: key.signature(),
            context,
        });
    }
    for (key, after) in new_solutions {
        result.changes.push(SolutionChange {
            signature: key.signature(),
            context: QueryPartEvaluationContext::Adding {
                after: after.variables(plan, base),
                row_signature: 0,
            },
        });
    }
    budget.check()?;
    Ok(PreparedChange {
        solutions: result,
        write,
    })
}
