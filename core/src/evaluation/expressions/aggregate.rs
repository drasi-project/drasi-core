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

use std::sync::Arc;

use drasi_query_ast::ast::FunctionExpression;

use crate::{
    evaluation::{
        context::{QueryVariables, SideEffects},
        functions::{aggregation::Accumulator, AggregatingFunction, Function},
        temporal::{AggregateContribution, ContributionKey},
        variable_value::VariableValue,
        EvaluationError, ExpressionEvaluationContext, ExpressionEvaluator,
    },
    interface::{ResultKey, ResultOwner},
};

impl ExpressionEvaluator {
    pub(crate) async fn change_contribution(
        &self,
        expression: &FunctionExpression,
        receipt: &AggregateContribution,
        directive: SideEffects,
    ) -> Result<(), EvaluationError> {
        let function = self
            .functions
            .get_function(&expression.name)
            .ok_or_else(|| EvaluationError::UnknownFunction(expression.name.to_string()))?;
        let Function::Aggregating(aggregate) = function.as_ref() else {
            return Err(EvaluationError::CorruptData);
        };
        let ContributionKey::GroupBy(grouping) = &receipt.key else {
            return Err(EvaluationError::CorruptData);
        };
        let variables = QueryVariables::new();
        let mut context = ExpressionEvaluationContext::from_saved(&variables, &receipt.context);
        context.set_side_effects(directive);
        self.evaluate_aggregate(
            &context,
            expression,
            aggregate.as_ref(),
            receipt.arguments.clone(),
            Arc::new(grouping.clone()),
        )
        .await?;
        Ok(())
    }

    pub(super) async fn evaluate_aggregate(
        &self,
        context: &ExpressionEvaluationContext<'_>,
        expression: &FunctionExpression,
        aggregate: &dyn AggregatingFunction,
        values: Vec<VariableValue>,
        grouping_keys: Arc<Vec<VariableValue>>,
    ) -> Result<VariableValue, EvaluationError> {
        let result_key = ResultKey::GroupBy(grouping_keys.clone());
        let result_owner = ResultOwner::Function(expression.position_in_query);
        let mut accumulator = if aggregate.accumulator_is_lazy() {
            aggregate.initialize_accumulator(
                context,
                expression,
                &grouping_keys,
                self.result_index.clone(),
            )
        } else {
            match self.result_index.get(&result_key, &result_owner).await? {
                Some(accumulator) => Accumulator::Value(accumulator),
                None => aggregate.initialize_accumulator(
                    context,
                    expression,
                    &grouping_keys,
                    self.result_index.clone(),
                ),
            }
        };
        let result = match context.get_side_effects() {
            SideEffects::Apply => aggregate.apply(context, values, &mut accumulator).await,
            SideEffects::RevertForUpdate | SideEffects::RevertForDelete => {
                aggregate.revert(context, values, &mut accumulator).await
            }
            SideEffects::Snapshot => {
                return aggregate
                    .snapshot(context, values, &accumulator)
                    .await
                    .map_err(EvaluationError::FunctionError);
            }
        }
        .map_err(EvaluationError::FunctionError)?;
        match accumulator {
            Accumulator::Value(value) => {
                self.result_index
                    .set(result_key, result_owner, Some(value))
                    .await?;
            }
            Accumulator::LazySortedSet(mut accumulator) => accumulator.commit().await?,
        }
        Ok(result)
    }
}
