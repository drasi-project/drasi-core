use std::{fmt::Debug, sync::Arc};

use crate::{
    evaluation::{
        temporal_constants, variable_value::{
            duration::Duration, zoned_datetime::ZonedDateTime, zoned_time::ZonedTime,
        }, EvaluationError, FunctionError, FunctionEvaluationError
    },
    interface::ResultIndex,
};

use async_trait::async_trait;

use drasi_query_ast::ast;

use crate::evaluation::{
    variable_value::float::Float, variable_value::VariableValue, ExpressionEvaluationContext,
};

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, LocalResult};

use super::{super::AggregatingFunction, lazy_sorted_set::LazySortedSet, Accumulator};

#[derive(Clone)]
pub struct Max {}

#[async_trait]
impl AggregatingFunction for Max {
    fn initialize_accumulator(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        grouping_keys: &Vec<VariableValue>,
        index: Arc<dyn ResultIndex>,
    ) -> Accumulator {
        Accumulator::LazySortedSet(LazySortedSet::new(
            expression.position_in_query,
            grouping_keys,
            index,
        ))
    }

    fn accumulator_is_lazy(&self) -> bool {
        true
    }

    async fn apply(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: "Max".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        log::info!("Applying Max with args: {:?}", args);
        let accumulator = match accumulator {
            Accumulator::LazySortedSet(accumulator) => accumulator,
            _ => return Err(FunctionError {
                function_name: "Max".to_string(),
                error: FunctionEvaluationError::CorruptData,
            }),
        };

        match &args[0] {
            VariableValue::Float(n) => {
                let value = match n.as_f64() {
                    Some(n) => n,
                    None => return Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::OverflowError,
                    }),
                };
                accumulator.insert(value * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::Float(match Float::from_f64(head * -1.0) {
                        Some(f) => f,
                        None => return Err(FunctionError {
                            function_name: "Max".to_string(),
                            error: FunctionEvaluationError::CorruptData,
                        }),
                    })),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                } 
            }
            VariableValue::Integer(n) => {
                let value = match n.as_i64() {
                    Some(n) => n,
                    None => return Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::OverflowError,
                    }),
                };
                accumulator.insert((value as f64) * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::Float(match Float::from_f64(head * -1.0) {
                        Some(f) => f,
                        None => return Err(FunctionError {
                            function_name: "Max".to_string(),
                            error: FunctionEvaluationError::CorruptData,
                        }),
                    })),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                } 
            }
            VariableValue::ZonedDateTime(zdt) => {
                let value = zdt.datetime().timestamp_millis() as f64;
                accumulator.insert(value * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::ZonedDateTime(
                        match ZonedDateTime::from_epoch_millis((head * -1.0) as u64) {
                            Ok(zdt) => zdt,
                            Err(e) => match e {
                                EvaluationError::OverflowError => return Err(FunctionError {
                                    function_name: "Max".to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                }),
                                _ => {
                                    return Err(FunctionError {
                                        function_name: "Max".to_string(),
                                        error: FunctionEvaluationError::InvalidFormat { expected: "A valid DateTime component".to_string() },
                                    });
                                }
                            },
                        },
                    )),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                } 
            }
            VariableValue::Duration(d) => {
                let value = d.duration().num_milliseconds() as f64;
                accumulator.insert(value * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::Duration(Duration::new(
                        ChronoDuration::milliseconds((head * -1.0) as i64),
                        0,
                        0,
                    ))),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                }
            }
            VariableValue::Date(d) => {
                // For date (Chrono::NaiveDate), we can store the number of days since the epoch
                let reference_date = *temporal_constants::EPOCH_NAIVE_DATE;
                let days_since_epoch = d.signed_duration_since(reference_date).num_days() as f64;
                accumulator.insert(days_since_epoch * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::Date(
                        reference_date + ChronoDuration::days((head * -1.0) as i64),
                    )),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                }
            }
            VariableValue::LocalTime(t) => {
                let reference_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
                let duration_since_midnight =
                    t.signed_duration_since(reference_time).num_milliseconds() as f64;
                accumulator.insert(duration_since_midnight * -1.0).await;

                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::LocalTime(
                        reference_time + ChronoDuration::milliseconds((head * -1.0) as i64),
                    )),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                }
            }
            VariableValue::LocalDateTime(dt) => {
                let duration_since_epoch = dt.and_utc().timestamp_millis() as f64;
                accumulator.insert(duration_since_epoch * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::LocalDateTime(
                        DateTime::from_timestamp_millis(head as i64 * -1.0 as i64)
                            .unwrap_or_default()
                            .naive_local(),
                    )),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                } 
            }
            VariableValue::ZonedTime(t) => {
                let epoch_date = *temporal_constants::EPOCH_NAIVE_DATE;
                let epoch_datetime = match epoch_date
                    .and_time(*t.time())
                    .and_local_timezone(*t.offset()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(FunctionError {
                            function_name: "Max".to_string(),
                            error: FunctionEvaluationError::InvalidFormat { expected: "A valid Time".to_string() },
                        }),
                    };
                let duration_since_epoch = epoch_datetime.timestamp_millis() as f64;
                accumulator.insert(duration_since_epoch * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::ZonedTime(ZonedTime::new(
                        (epoch_datetime + ChronoDuration::milliseconds((head * -1.0) as i64))
                            .time(),
                        *temporal_constants::UTC_FIXED_OFFSET,
                    ))),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                }
            }
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: "Max".to_string(),
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
                function_name: "Max".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        let accumulator = match accumulator {
            Accumulator::LazySortedSet(accumulator) => accumulator,
            _ => return Err(FunctionError {
                function_name: "Max".to_string(),
                error: FunctionEvaluationError::CorruptData,
            }),
        };

        match &args[0] {
            VariableValue::Float(n) => {
                let value = match n.as_f64() {
                    Some(n) => n,
                    None => return Err(FunctionError {
                    function_name: "Max".to_string(),
                    error: FunctionEvaluationError::OverflowError,
                }),
                };
                accumulator.remove(value * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::Float(match Float::from_f64(head * -1.0) {
                        Some(f) => f,
                        None => return Err(FunctionError {
                            function_name: "Max".to_string(),
                            error: FunctionEvaluationError::CorruptData,
                        }),
                    })),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                } 
            }
            VariableValue::Integer(n) => {
                let value = match n.as_i64() {
                    Some(n) => n,
                    None => return Err(FunctionError {
                    function_name: "Max".to_string(),
                    error: FunctionEvaluationError::OverflowError,
                }),
                };
                accumulator.remove((value as f64) * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::Float(match Float::from_f64(head * -1.0) {
                        Some(f) => f,
                        None => return Err(FunctionError {
                            function_name: "Max".to_string(),
                            error: FunctionEvaluationError::CorruptData,
                        }),
                    })),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                }
            }
            VariableValue::ZonedDateTime(zdt) => {
                let value = zdt.datetime().timestamp_millis() as f64;
                accumulator.remove(value * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::ZonedDateTime(
                        match ZonedDateTime::from_epoch_millis((head * -1.0) as u64) {
                            Ok(zdt) => zdt,
                            Err(e) => match e {
                                EvaluationError::OverflowError => return Err(FunctionError {
                                    function_name: "Max".to_string(),
                                    error: FunctionEvaluationError::OverflowError,
                                }),
                                _ => {
                                    return Err(FunctionError {
                                        function_name: "Max".to_string(),
                                        error: FunctionEvaluationError::InvalidFormat { expected: "A valid DateTime component".to_string() },
                                    });
                                }
                            },
                        }
                    )),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                }
            }
            VariableValue::Duration(d) => {
                let value = d.duration().num_milliseconds() as f64;
                accumulator.remove(value * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::Duration(Duration::new(
                        ChronoDuration::milliseconds((head * -1.0) as i64),
                        0,
                        0,
                    ))),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                } 
            }
            VariableValue::Date(d) => {
                // For date (Chrono::NaiveDate), we can store the number of days since the epoch
                let reference_date = *temporal_constants::EPOCH_NAIVE_DATE;
                let days_since_epoch = d.signed_duration_since(reference_date).num_days() as f64;
                accumulator.remove(days_since_epoch * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::Date(
                        reference_date + ChronoDuration::days((head * -1.0) as i64),
                    )),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                }
            }
            VariableValue::LocalTime(t) => {
                let reference_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
                let duration_since_midnight =
                    t.signed_duration_since(reference_time).num_milliseconds() as f64;
                accumulator.remove(duration_since_midnight * -1.0).await;

                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::LocalTime(
                        reference_time + ChronoDuration::milliseconds((head * -1.0) as i64),
                    )),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                }
            }
            VariableValue::LocalDateTime(dt) => {
                let duration_since_epoch = dt.and_utc().timestamp_millis() as f64;
                accumulator.remove(duration_since_epoch * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::LocalDateTime(
                        DateTime::from_timestamp_millis(head as i64 * -1.0 as i64)
                            .unwrap_or_default()
                            .naive_local(),
                    )),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                } 
            }
            VariableValue::ZonedTime(t) => {
                let epoch_date = *temporal_constants::EPOCH_NAIVE_DATE;
                let epoch_datetime = match epoch_date
                    .and_time(*t.time())
                    .and_local_timezone(*t.offset()) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(FunctionError {
                            function_name: "Max".to_string(),
                            error: FunctionEvaluationError::InvalidFormat { expected: "A valid Time".to_string() },
                        }),
                    };
                let duration_since_epoch = epoch_datetime.timestamp_millis() as f64;
                accumulator.remove(duration_since_epoch * -1.0).await;
                match accumulator.get_head().await {
                    Ok(Some(head)) => Ok(VariableValue::ZonedTime(ZonedTime::new(
                        (epoch_datetime + ChronoDuration::milliseconds((head * -1.0) as i64))
                            .time(),
                        *temporal_constants::UTC_FIXED_OFFSET,
                    ))),
                    Ok(None) => Ok(VariableValue::Null),
                    Err(e) => Err(FunctionError {
                        function_name: "Max".to_string(),
                        error: FunctionEvaluationError::IndexError(e)
                    }),
                } 
            }
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: "Max".to_string(),
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
                function_name: "Max".to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let accumulator = match accumulator {
            Accumulator::LazySortedSet(accumulator) => accumulator,
            _ => return Err(FunctionError {
                function_name: "Max".to_string(),
                error: FunctionEvaluationError::CorruptData,
            }),
        };

        let value = match accumulator.get_head().await {
            Ok(Some(head)) => head * -1.0,
            Ok(None) => return Ok(VariableValue::Null),
            Err(e) => return Err(FunctionError {
                function_name: "Max".to_string(),
                error: FunctionEvaluationError::IndexError(e),
            }),
        };

        return match &args[0] {
            VariableValue::Float(_) => Ok(VariableValue::Float(match Float::from_f64(value) {
                Some(f) => f,
                None => return Err(FunctionError {
                    function_name: "Max".to_string(),
                    error: FunctionEvaluationError::OverflowError,
                }),
            })),
            VariableValue::Integer(_) => Ok(VariableValue::Integer((value as i64).into())),
            VariableValue::ZonedDateTime(_) => Ok(VariableValue::ZonedDateTime(
                match ZonedDateTime::from_epoch_millis(value as u64) {
                    Ok(zdt) => zdt,
                    Err(e) => match e {
                        EvaluationError::OverflowError => return Err(FunctionError {
                            function_name: "Max".to_string(),
                            error: FunctionEvaluationError::OverflowError,
                        }),
                        _ => {
                            return Err(FunctionError {
                                function_name: "Max".to_string(),
                                error: FunctionEvaluationError::InvalidFormat { expected: "A valid DateTime component".to_string() },
                            });
                        }
                    },
                },
            )),
            VariableValue::Duration(_) => Ok(VariableValue::Duration(Duration::new(
                ChronoDuration::milliseconds(value as i64),
                0,
                0,
            ))),
            VariableValue::Date(_) => {
                let reference_date = *temporal_constants::EPOCH_NAIVE_DATE;
                Ok(VariableValue::Date(
                    reference_date + ChronoDuration::days(value as i64),
                ))
            }
            VariableValue::LocalTime(_) => {
                let reference_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
                Ok(VariableValue::LocalTime(
                    reference_time + ChronoDuration::milliseconds(value as i64),
                ))
            }
            VariableValue::LocalDateTime(_) => Ok(VariableValue::LocalDateTime(
                DateTime::from_timestamp_millis(value as i64)
                    .unwrap_or_default()
                    .naive_local(),
            )),
            VariableValue::ZonedTime(_) => {
                let epoch_date = *temporal_constants::EPOCH_NAIVE_DATE;
                let epoch_datetime = match epoch_date
                    .and_time(*temporal_constants::MIDNIGHT_NAIVE_TIME)
                    .and_local_timezone(*temporal_constants::UTC_FIXED_OFFSET) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(FunctionError {
                            function_name: "Max".to_string(),
                            error: FunctionEvaluationError::InvalidFormat { expected: "A valid Time".to_string() },
                        }),
                    };
                Ok(VariableValue::ZonedTime(ZonedTime::new(
                    (epoch_datetime + ChronoDuration::milliseconds(value as i64)).time(),
                    *temporal_constants::UTC_FIXED_OFFSET,
                )))
            }
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: "Max".to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        };
    }
}

impl Debug for Max {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Max")
    }
}
