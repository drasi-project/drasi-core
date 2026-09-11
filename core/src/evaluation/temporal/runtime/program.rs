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

use crate::evaluation::QueryExecutionError;

use drasi_query_ast::ast::{
    Expression, FunctionExpression, Literal, ParentExpression, ProjectionClause, Query,
    UnaryExpression,
};

use crate::evaluation::{functions::FunctionRegistry, EvaluationError};

use super::super::PartId;

#[derive(Debug)]
pub struct ExpressionProgram {
    pub expression: Expression,
    pub effects: Vec<FunctionExpression>,
}

#[derive(Debug)]
pub struct PartProgram {
    pub id: PartId,
    pub predicates: Vec<ExpressionProgram>,
    pub projection: Vec<ExpressionProgram>,
    pub grouping: Vec<Expression>,
    pub grouped: bool,
}

#[derive(Debug)]
pub struct TemporalProgram {
    pub parts: Vec<PartProgram>,
}

impl TemporalProgram {
    pub fn compile(
        query: &Query,
        registry: &FunctionRegistry,
    ) -> Result<Option<Self>, EvaluationError> {
        let temporal = query.parts.iter().any(|part| {
            part.where_clauses
                .iter()
                .chain(projection_expressions(&part.return_clause))
                .any(|expression| contains_temporal(expression, registry))
        });
        if !temporal {
            return Ok(None);
        }
        let mut parts = Vec::new();
        for (ordinal, part) in query.parts.iter().enumerate() {
            let grouping = match &part.return_clause {
                ProjectionClause::Item(_) => Vec::new(),
                ProjectionClause::GroupBy { grouping, .. } => grouping.clone(),
            };
            if grouping
                .iter()
                .any(|expression| contains_temporal(expression, registry))
            {
                return Err(EvaluationError::from(
                    QueryExecutionError::UnsupportedTemporalEffect(
                        "temporal grouping expressions have a cyclic cell-owner dependency",
                    ),
                ));
            }
            let predicates = part
                .where_clauses
                .iter()
                .map(|expression| ExpressionProgram::compile(expression, registry))
                .collect::<Result<_, _>>()?;
            let projection = projection_expressions(&part.return_clause)
                .map(|expression| ExpressionProgram::compile(expression, registry))
                .collect::<Result<_, _>>()?;
            parts.push(PartProgram {
                id: PartId(ordinal + 1),
                predicates,
                projection,
                grouping,
                grouped: matches!(part.return_clause, ProjectionClause::GroupBy { .. }),
            });
        }
        Ok(Some(Self { parts }))
    }
}

impl ExpressionProgram {
    pub fn compile(
        expression: &Expression,
        registry: &FunctionRegistry,
    ) -> Result<Self, EvaluationError> {
        let mut effects = Vec::new();
        collect_effects(expression, registry, &mut effects)?;
        Ok(Self {
            expression: expression.clone(),
            effects,
        })
    }
}

fn projection_expressions(projection: &ProjectionClause) -> impl Iterator<Item = &Expression> {
    let (grouping, expressions): (&[Expression], &[Expression]) = match projection {
        ProjectionClause::Item(expressions) => (&[], expressions),
        ProjectionClause::GroupBy {
            grouping,
            aggregates,
        } => (grouping, aggregates),
    };
    grouping.iter().chain(expressions)
}

fn contains_temporal(expression: &Expression, registry: &FunctionRegistry) -> bool {
    if let Expression::FunctionExpression(function) = expression {
        if registry
            .get_function(&function.name)
            .is_some_and(|function| function.as_temporal().is_some())
        {
            return true;
        }
    }
    children(expression)
        .iter()
        .any(|child| contains_temporal(child, registry))
}

fn collect_effects(
    expression: &Expression,
    registry: &FunctionRegistry,
    effects: &mut Vec<FunctionExpression>,
) -> Result<(), EvaluationError> {
    if let Expression::FunctionExpression(function) = expression {
        let registered = registry
            .get_function(&function.name)
            .ok_or_else(|| EvaluationError::UnknownFunction(function.name.to_string()))?;
        if registered.is_lazy_temporal() {
            if function.args.len() != 2 {
                return Err(EvaluationError::InvalidArgument);
            }
            collect_effects(&function.args[0], registry, effects)?;
            effects.push(function.clone());
            collect_effects(&function.args[1], registry, effects)?;
            return Ok(());
        }
        let start = effects.len();
        for argument in &function.args {
            collect_effects(argument, registry, effects)?;
        }
        if function.name.as_ref() == "reduce" && effects.len() != start {
            return Err(EvaluationError::from(
                QueryExecutionError::UnsupportedTemporalEffect(
                    "stateful reduction has an iteration-to-iteration effect dependency",
                ),
            ));
        }
        if registered.as_temporal().is_some() || registered.is_aggregating() {
            effects.push(function.clone());
        }
    } else {
        for child in children(expression) {
            collect_effects(child, registry, effects)?;
        }
    }
    Ok(())
}

fn children(expression: &Expression) -> Vec<&Expression> {
    match expression {
        Expression::UnaryExpression(unary) => match unary {
            UnaryExpression::ExpressionProperty { exp, .. } => vec![exp],
            UnaryExpression::ListRange {
                start_bound,
                end_bound,
            } => start_bound
                .iter()
                .chain(end_bound)
                .map(Box::as_ref)
                .collect(),
            UnaryExpression::Literal(literal) => {
                let mut expressions = Vec::new();
                literal_children(literal, &mut expressions);
                expressions
            }
            other => other.get_children(),
        },
        Expression::IteratorExpression(iterator) => {
            let mut children = vec![iterator.list_expression.as_ref()];
            children.extend(iterator.filter.as_deref());
            children.extend(iterator.map_expression.as_deref());
            children
        }
        other => other.get_children(),
    }
}

fn literal_children<'a>(literal: &'a Literal, expressions: &mut Vec<&'a Expression>) {
    match literal {
        Literal::Expression(expression) => expressions.push(expression),
        Literal::Object(values) => {
            for (_, value) in values {
                literal_children(value, expressions);
            }
        }
        Literal::Integer(_)
        | Literal::Real(_)
        | Literal::Boolean(_)
        | Literal::Text(_)
        | Literal::Date(_)
        | Literal::LocalTime(_)
        | Literal::ZonedTime(_)
        | Literal::LocalDateTime(_)
        | Literal::ZonedDateTime(_)
        | Literal::Duration(_)
        | Literal::Null => {}
    }
}
