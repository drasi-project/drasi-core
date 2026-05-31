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

use std::{fmt::Debug, sync::Arc};

use crate::{
    evaluation::{FunctionError, FunctionEvaluationError},
    interface::ResultIndex,
};

use async_trait::async_trait;

use drasi_query_ast::ast;

use crate::evaluation::{
    variable_value::duration::Duration, variable_value::float::Float,
    variable_value::VariableValue, ExpressionEvaluationContext,
};

use chrono::Duration as ChronoDuration;

use super::{super::AggregatingFunction, Accumulator, ValueAccumulator};

pub struct Avg {}

#[async_trait]
impl AggregatingFunction for Avg {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        _grouping_keys: &Vec<VariableValue>,
        _index: Arc<dyn ResultIndex>,
    ) -> Accumulator {
        Accumulator::Value(ValueAccumulator::Avg { sum: 0.0, count: 0 })
    }

    fn accumulator_is_lazy(&self) -> bool {
        false
    }

    async fn apply(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let (sum, count) = match accumulator {
            Accumulator::Value(ValueAccumulator::Avg { sum, count }) => (sum, count),
            _ => {
                return Err(FunctionError {
                    function_name: "Avg".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        match &args[0] {
            VariableValue::Float(n) => {
                *count += 1;
                *sum += match n.as_f64() {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Avg".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                let avg = *sum / *count as f64;

                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            VariableValue::Integer(n) => {
                *count += 1;
                *sum += match n.as_i64() {
                    Some(n) => n as f64,
                    None => {
                        return Err(FunctionError {
                            function_name: "Avg".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };
                let avg = *sum / *count as f64;

                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            // The average of two dates/times does not really make sense
            // Only adding duration for now
            VariableValue::Duration(d) => {
                *count += 1;
                *sum += d.duration().num_milliseconds() as f64;
                let avg = *sum / *count as f64;

                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::milliseconds(avg as i64),
                    0,
                    0,
                )))
            }
            VariableValue::Null => {
                let avg = *sum / *count as f64;
                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            _ => Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }

    async fn revert(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let (sum, count) = match accumulator {
            Accumulator::Value(ValueAccumulator::Avg { sum, count }) => (sum, count),
            _ => {
                return Err(FunctionError {
                    function_name: "Avg".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        match &args[0] {
            VariableValue::Float(n) => {
                *count -= 1;
                *sum -= match n.as_f64() {
                    Some(n) => n,
                    None => {
                        return Err(FunctionError {
                            function_name: "Avg".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };

                if *count == 0 {
                    return Ok(VariableValue::Float(
                        Float::from_f64(0.0).unwrap_or_default(),
                    ));
                }

                let avg = *sum / *count as f64;

                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            VariableValue::Integer(n) => {
                *count -= 1;
                *sum -= match n.as_i64() {
                    Some(n) => n as f64,
                    None => {
                        return Err(FunctionError {
                            function_name: "Avg".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        })
                    }
                };

                if *count == 0 {
                    return Ok(VariableValue::Float(
                        Float::from_f64(0.0).unwrap_or_default(),
                    ));
                }

                let avg = *sum / *count as f64;

                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            VariableValue::Duration(d) => {
                *count -= 1;
                *sum -= d.duration().num_milliseconds() as f64;

                if *count == 0 {
                    return Ok(VariableValue::Float(
                        Float::from_f64(0.0).unwrap_or_default(),
                    ));
                }

                let avg = *sum / *count as f64;

                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::milliseconds(avg as i64),
                    0,
                    0,
                )))
            }
            VariableValue::Null => {
                let avg = *sum / *count as f64;
                Ok(VariableValue::Float(
                    Float::from_f64(avg).unwrap_or_default(),
                ))
            }
            _ => Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }

    async fn snapshot(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let (sum, count) = match accumulator {
            Accumulator::Value(ValueAccumulator::Avg { sum, count }) => (sum, count),
            _ => {
                return Err(FunctionError {
                    function_name: "Avg".to_string(),
                    error: FunctionEvaluationError::CorruptData,
                })
            }
        };

        if *count == 0 {
            return Ok(VariableValue::Float(
                Float::from_f64(0.0).unwrap_or_default(),
            ));
        }

        let avg = *sum / *count as f64;

        match &args[0] {
            VariableValue::Float(_) => Ok(VariableValue::Float(
                Float::from_f64(avg).unwrap_or_default(),
            )),
            VariableValue::Integer(_) => Ok(VariableValue::Float(
                Float::from_f64(avg).unwrap_or_default(),
            )),
            VariableValue::Duration(_) => Ok(VariableValue::Duration(Duration::new(
                ChronoDuration::milliseconds(avg as i64),
                0,
                0,
            ))),
            // `apply` and `revert` already accept `Null` (see above) — for
            // example, when a property is missing on a bootstrap-originated
            // element. `snapshot` was the odd one out and crashed with
            // `InvalidArgument(0)` instead of returning the running average.
            // That manifests as a hard crash on RocksDB-persisted restart.
            // See https://github.com/drasi-project/drasi-core/issues/498
            VariableValue::Null => Ok(VariableValue::Float(
                Float::from_f64(avg).unwrap_or_default(),
            )),
            _ => Err(FunctionError {
                function_name: "Avg".to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

impl Debug for Avg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Avg")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        evaluation::{
            context::QueryVariables, variable_value::VariableValue, ExpressionEvaluationContext,
            InstantQueryClock,
        },
        in_memory_index::in_memory_result_index::InMemoryResultIndex,
    };
    use drasi_query_ast::ast;

    fn make_context() -> (QueryVariables, Arc<InstantQueryClock>) {
        (
            QueryVariables::new(),
            Arc::new(InstantQueryClock::new(0, 0)),
        )
    }

    fn expression() -> ast::FunctionExpression {
        ast::FunctionExpression {
            name: "avg".into(),
            args: vec![],
            position_in_query: 10,
        }
    }

    /// Regression test for <https://github.com/drasi-project/drasi-core/issues/498>.
    ///
    /// `Avg::apply` and `Avg::revert` already accept `VariableValue::Null` and
    /// just return the running average. `Avg::snapshot` was the odd one out
    /// and crashed with `InvalidArgument(0)`. On a RocksDB-persisted restart
    /// the bootstrap re-evaluates each element through `snapshot`; if any
    /// element's aggregation expression evaluates to `Null` (typical when
    /// the underlying property is missing/optional), the whole bootstrap
    /// blew up. After the fix, `snapshot` returns the running average just
    /// like `apply`/`revert` do.
    #[tokio::test]
    async fn snapshot_with_null_returns_running_average() {
        let avg = Avg {};
        let index = Arc::new(InMemoryResultIndex::new());
        let (variables, clock) = make_context();
        let context = ExpressionEvaluationContext::new(&variables, clock);
        let expr = expression();

        let mut accumulator = avg.initialize_accumulator(&context, &expr, &vec![], index);

        // Build up an average from two real samples: (10 + 20) / 2 == 15.0
        avg.apply(
            &context,
            vec![VariableValue::Integer(10.into())],
            &mut accumulator,
        )
        .await
        .unwrap();
        avg.apply(
            &context,
            vec![VariableValue::Integer(20.into())],
            &mut accumulator,
        )
        .await
        .unwrap();

        // Snapshot with a Null arg used to bail with InvalidArgument(0).
        // It must now succeed and return the running average unchanged.
        let snap = avg
            .snapshot(&context, vec![VariableValue::Null], &accumulator)
            .await
            .expect("Avg::snapshot must accept Null (issue #498)");

        match snap {
            VariableValue::Float(n) => {
                assert!(
                    (n.as_f64().unwrap() - 15.0).abs() < f64::EPSILON,
                    "expected running average 15.0, got {n:?}"
                );
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }

    /// `apply` keeps the average unchanged when fed `Null`. `snapshot` must
    /// agree — fixed-point invariant: `apply(Null); snapshot(Null) == snapshot(prev)`.
    #[tokio::test]
    async fn apply_then_snapshot_with_null_is_consistent() {
        let avg = Avg {};
        let index = Arc::new(InMemoryResultIndex::new());
        let (variables, clock) = make_context();
        let context = ExpressionEvaluationContext::new(&variables, clock);
        let expr = expression();

        let mut accumulator = avg.initialize_accumulator(&context, &expr, &vec![], index);

        avg.apply(
            &context,
            vec![VariableValue::Float(8.0.into())],
            &mut accumulator,
        )
        .await
        .unwrap();
        avg.apply(
            &context,
            vec![VariableValue::Float(4.0.into())],
            &mut accumulator,
        )
        .await
        .unwrap();

        // Apply a Null sample (no-op) then snapshot; must match a snapshot
        // taken before the Null apply.
        let baseline = avg
            .snapshot(
                &context,
                vec![VariableValue::Float(0.0.into())],
                &accumulator,
            )
            .await
            .unwrap();
        avg.apply(&context, vec![VariableValue::Null], &mut accumulator)
            .await
            .unwrap();
        let after_null = avg
            .snapshot(&context, vec![VariableValue::Null], &accumulator)
            .await
            .unwrap();

        match (baseline, after_null) {
            (VariableValue::Float(a), VariableValue::Float(b)) => {
                assert_eq!(a.as_f64(), b.as_f64());
            }
            (a, b) => panic!("expected two Float values, got ({a:?}, {b:?})"),
        }
    }
}
