use std::{fmt::Debug, sync::Arc};

use crate::{
    evaluation::{
        temporal_constants, variable_value::zoned_datetime::ZonedDateTime, EvaluationError,
    },
    interface::ResultIndex,
};

use async_trait::async_trait;

use drasi_query_ast::ast;

use crate::evaluation::{
    variable_value::duration::Duration, variable_value::float::Float,
    variable_value::zoned_time::ZonedTime, variable_value::VariableValue,
    ExpressionEvaluationContext,
};

use super::{super::AggregatingFunction, lazy_sorted_set::LazySortedSet, Accumulator};
use chrono::{Duration as ChronoDuration, FixedOffset, NaiveDateTime, NaiveTime};

#[derive(Clone)]
pub struct Min {}

#[async_trait]
impl AggregatingFunction for Min {
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
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount("Min".to_string()));
        }

        let accumulator = match accumulator {
            Accumulator::LazySortedSet(accumulator) => accumulator,
            _ => return Err(EvaluationError::InvalidType),
        };

        match &args[0] {
            VariableValue::Float(n) => {
                let value = n.as_f64().unwrap();
                accumulator.insert(value).await;
                match accumulator.get_head().await? {
                    Some(head) => match Float::from_f64(head) {
                        Some(f) => Ok(VariableValue::Float(f)),
                        None => Err(EvaluationError::InvalidState),
                    },
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::Integer(n) => {
                let value = n.as_i64().unwrap();
                accumulator.insert(value as f64).await;
                match accumulator.get_head().await? {
                    Some(head) => match Float::from_f64(head) {
                        Some(f) => Ok(VariableValue::Float(f)),
                        None => Err(EvaluationError::InvalidState),
                    },
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::ZonedDateTime(zdt) => {
                let value = zdt.datetime().timestamp_millis() as f64;
                accumulator.insert(value).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::ZonedDateTime(
                        ZonedDateTime::from_epoch_millis(head as u64),
                    )),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::Duration(d) => {
                let value = d.duration().num_milliseconds() as f64;
                accumulator.insert(value).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::Duration(Duration::new(
                        ChronoDuration::milliseconds(head as i64),
                        0,
                        0,
                    ))),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::Date(d) => {
                let reference_date = *temporal_constants::EPOCH_NAIVE_DATE;
                let days_since_epoch = d.signed_duration_since(reference_date).num_days() as f64;
                accumulator.insert(days_since_epoch).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::Date(
                        reference_date + ChronoDuration::days((head) as i64),
                    )),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::LocalTime(t) => {
                let reference_time = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
                let seconds_since_midnight =
                    t.signed_duration_since(reference_time).num_milliseconds() as f64;
                accumulator.insert(seconds_since_midnight).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::LocalTime(
                        reference_time + ChronoDuration::milliseconds((head) as i64),
                    )),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::LocalDateTime(dt) => {
                let duration_since_epoch = dt.timestamp_millis() as f64;
                accumulator.insert(duration_since_epoch).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::LocalDateTime(
                        NaiveDateTime::from_timestamp_millis(head as i64).unwrap(),
                    )),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::ZonedTime(t) => {
                let epoch_date = *temporal_constants::EPOCH_NAIVE_DATE;
                let epoch_datetime = epoch_date
                    .and_time(*t.time())
                    .and_local_timezone(*t.offset())
                    .unwrap();
                let duration_since_epoch = epoch_datetime.timestamp_millis() as f64;
                accumulator.insert(duration_since_epoch).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::ZonedTime(ZonedTime::new(
                        (epoch_datetime + ChronoDuration::milliseconds((head) as i64)).time(),
                        FixedOffset::east_opt(0).unwrap(),
                    ))),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(EvaluationError::InvalidType),
        }
    }

    async fn revert(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &mut Accumulator,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount("Min".to_string()));
        }
        let accumulator = match accumulator {
            Accumulator::LazySortedSet(accumulator) => accumulator,
            _ => return Err(EvaluationError::InvalidType),
        };

        match &args[0] {
            VariableValue::Float(n) => {
                let value = n.as_f64().unwrap();
                accumulator.remove(value).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::Float(Float::from_f64(head).unwrap())),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::Integer(n) => {
                let value = n.as_i64().unwrap();
                accumulator.remove((value as f64) * 1.0).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::Float(Float::from_f64(head).unwrap())),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::ZonedDateTime(zdt) => {
                let value = zdt.datetime().timestamp_millis() as f64;
                accumulator.remove(value).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::ZonedDateTime(
                        ZonedDateTime::from_epoch_millis((head) as u64),
                    )),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::Duration(d) => {
                let value = d.duration().num_milliseconds() as f64;
                accumulator.remove(value).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::Duration(Duration::new(
                        ChronoDuration::milliseconds((head) as i64),
                        0,
                        0,
                    ))),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::Date(d) => {
                // For date (Chrono::NaiveDate), we can store the number of days since the epoch
                let reference_date = *temporal_constants::EPOCH_NAIVE_DATE;
                let days_since_epoch = d.signed_duration_since(reference_date).num_days() as f64;
                accumulator.remove(days_since_epoch).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::Date(
                        reference_date + ChronoDuration::days((head) as i64),
                    )),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::LocalTime(t) => {
                let reference_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
                let duration_since_midnight =
                    t.signed_duration_since(reference_time).num_milliseconds() as f64;
                accumulator.remove(duration_since_midnight).await;

                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::LocalTime(
                        reference_time + ChronoDuration::milliseconds((head) as i64),
                    )),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::LocalDateTime(dt) => {
                let duration_since_epoch = dt.timestamp_millis() as f64;
                accumulator.remove(duration_since_epoch).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::LocalDateTime(
                        NaiveDateTime::from_timestamp_millis((head) as i64).unwrap(),
                    )),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::ZonedTime(t) => {
                let epoch_date = *temporal_constants::EPOCH_NAIVE_DATE;
                let epoch_datetime = epoch_date
                    .and_time(*t.time())
                    .and_local_timezone(*t.offset())
                    .unwrap();
                let duration_since_epoch = epoch_datetime.timestamp_millis() as f64;
                accumulator.remove(duration_since_epoch).await;
                match accumulator.get_head().await? {
                    Some(head) => Ok(VariableValue::ZonedTime(ZonedTime::new(
                        (epoch_datetime + ChronoDuration::milliseconds((head) as i64)).time(),
                        FixedOffset::east_opt(0).unwrap(),
                    ))),
                    None => Ok(VariableValue::Null),
                }
            }
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(EvaluationError::InvalidType),
        }
    }

    async fn snapshot(
        &self,
        _context: &ExpressionEvaluationContext,
        args: Vec<VariableValue>,
        accumulator: &Accumulator,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() != 1 {
            return Err(EvaluationError::InvalidArgumentCount("Min".to_string()));
        }

        let accumulator = match accumulator {
            Accumulator::LazySortedSet(accumulator) => accumulator,
            _ => return Err(EvaluationError::InvalidType),
        };

        let value = match accumulator.get_head().await? {
            Some(head) => head,
            None => return Ok(VariableValue::Null),
        };

        return match &args[0] {
            VariableValue::Float(_) => Ok(VariableValue::Float(Float::from_f64(value).unwrap())),
            VariableValue::Integer(_) => Ok(VariableValue::Integer((value as i64).into())),
            VariableValue::ZonedDateTime(_) => Ok(VariableValue::ZonedDateTime(
                ZonedDateTime::from_epoch_millis(value as u64),
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
                NaiveDateTime::from_timestamp_millis(value as i64).unwrap(),
            )),
            VariableValue::ZonedTime(_) => {
                let epoch_date = *temporal_constants::EPOCH_NAIVE_DATE;
                let epoch_datetime = epoch_date
                    .and_time(*temporal_constants::MIDNIGHT_NAIVE_TIME)
                    .and_local_timezone(FixedOffset::east_opt(0).unwrap())
                    .unwrap();
                Ok(VariableValue::ZonedTime(ZonedTime::new(
                    (epoch_datetime + ChronoDuration::milliseconds(value as i64)).time(),
                    FixedOffset::east_opt(0).unwrap(),
                )))
            }
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(EvaluationError::InvalidType),
        };
    }
}

impl Debug for Min {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Min")
    }
}
