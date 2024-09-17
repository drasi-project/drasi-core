use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::temporal_constants;
use crate::evaluation::variable_value::duration::Duration;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, FunctionError, FunctionEvaluationError};

use chrono::{Datelike, Duration as ChronoDuration, LocalResult, NaiveDate};
use iso8601_duration::Duration as IsoDuration;
use log::error;
use regex::Regex;
use std::collections::HashSet;

#[derive(Debug)]
pub struct DurationFunc {}

#[async_trait]
impl ScalarFunction for DurationFunc {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match &args[0] {
            VariableValue::String(s) => {
                let duration_str = s.as_str();
                let duration = match parse_duration_input(duration_str).await {
                    Ok(duration) => duration,
                    Err(e) => return Err(e),
                };
                Ok(VariableValue::Duration(duration))
            }
            VariableValue::Object(o) => {
                let valid_keys: HashSet<String> = [
                    "years",
                    "months",
                    "weeks",
                    "days",
                    "hours",
                    "minutes",
                    "seconds",
                    "milliseconds",
                    "microseconds",
                    "nanoseconds",
                ]
                .iter()
                .map(|&s| s.to_string())
                .collect();

                let invalid_keys: Vec<_> =
                    o.keys().filter(|&key| !valid_keys.contains(key)).collect();
                if !invalid_keys.is_empty() {
                    error!("Invalid keys in duration object");
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(0),
                    });
                }

                let year = match o.get("years") {
                    Some(f) => match f.as_f64() {
                        Some(f) => f,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::OverflowError,
                            })
                        }
                    },
                    _ => 0.0,
                };
                let week = match o.get("weeks") {
                    Some(f) => match f.as_f64() {
                        Some(f) => f,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidArgument(0),
                            })
                        }
                    },
                    _ => 0.0,
                };
                let month = match o.get("months") {
                    Some(f) => match f.as_f64() {
                        Some(f) => f + year.fract() * 12.0,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidArgument(0),
                            })
                        }
                    },
                    _ => year.fract() * 12.0,
                };
                let day = match o.get("days") {
                    Some(f) => match f.as_f64() {
                        Some(f) => f + month.fract() * 30.0,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidArgument(0),
                            })
                        }
                    },
                    _ => month.fract() * 30.0,
                };
                let hour = match o.get("hours") {
                    Some(f) => match f.as_f64() {
                        Some(f) => f + day.fract() * 24.0,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidArgument(0),
                            })
                        }
                    },
                    _ => day.fract() * 24.0,
                };
                let minute = match o.get("minutes") {
                    Some(f) => match f.as_f64() {
                        Some(f) => f + hour.fract() * 60.0,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidArgument(0),
                            })
                        }
                    },
                    _ => hour.fract() * 60.0,
                };
                let second = match o.get("seconds") {
                    Some(f) => match f.as_f64() {
                        Some(f) => f + minute.fract() * 60.0,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidArgument(0),
                            })
                        }
                    },
                    _ => minute.fract() * 60.0,
                };
                let millisecond = match o.get("milliseconds") {
                    Some(f) => match f.as_f64() {
                        Some(f) => f + second.fract() * 1000.0,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidArgument(0),
                            })
                        }
                    },
                    _ => second.fract() * 1000.0,
                };
                let microsecond = match o.get("microseconds") {
                    Some(f) => match f.as_f64() {
                        Some(f) => f + millisecond.fract() * 1000.0,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidArgument(0),
                            })
                        }
                    },
                    _ => millisecond.fract() * 1000.0,
                };
                let nanosecond = match o.get("nanoseconds") {
                    Some(f) => match f.as_f64() {
                        Some(f) => f + microsecond.fract() * 1000.0,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidArgument(0),
                            })
                        }
                    },
                    _ => microsecond.fract() * 1000.0,
                };

                let chrono_duration = ChronoDuration::days(day.floor() as i64)
                    + ChronoDuration::weeks(week.floor() as i64)
                    + ChronoDuration::hours(hour.floor() as i64)
                    + ChronoDuration::minutes(minute.floor() as i64)
                    + ChronoDuration::seconds(second.floor() as i64)
                    + ChronoDuration::milliseconds(millisecond.floor() as i64)
                    + ChronoDuration::microseconds(microsecond.floor() as i64)
                    + ChronoDuration::nanoseconds(nanosecond.floor() as i64);
                let duration =
                    Duration::new(chrono_duration, year.floor() as i64, month.floor() as i64);
                Ok(VariableValue::Duration(duration))
            }
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct Between {}

#[async_trait]
impl ScalarFunction for Between {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match (&args[0], &args[1]) {
            (VariableValue::Date(start), VariableValue::Date(end)) => {
                let duration = end.signed_duration_since(*start);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::Date(_start), VariableValue::LocalTime(end)) => {
                let start_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
                let duration = end.signed_duration_since(start_time);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::Date(start), VariableValue::LocalDateTime(end)) => {
                let start_datetime = match start.and_hms_opt(0, 0, 0) {
                    Some(start_time) => start_time,
                    None => unreachable!(),
                };
                let duration = end.signed_duration_since(start_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::Date(_start), VariableValue::ZonedTime(end)) => {
                let start_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
                let end_time = end.time();
                let duration = end_time.signed_duration_since(start_time);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::Date(start), VariableValue::ZonedDateTime(end)) => {
                let start_time = match start.and_hms_opt(0, 0, 0) {
                    Some(start_time) => start_time,
                    None => unreachable!(),
                };
                let start_datetime = match start_time.and_local_timezone(*end.datetime().offset()) {
                    LocalResult::Single(start_datetime) => start_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };
                let end_datetime = end.datetime().fixed_offset();
                let duration = end_datetime.signed_duration_since(start_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::LocalTime(start), VariableValue::LocalTime(end)) => {
                let duration = end.signed_duration_since(*start);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::LocalTime(start), VariableValue::Date(_end)) => {
                let end_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
                let duration = end_time.signed_duration_since(*start);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::LocalTime(start), VariableValue::LocalDateTime(end)) => {
                let end_date = end.date();
                let start_datetime = end_date.and_time(*start);
                let duration = end.signed_duration_since(start_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::LocalTime(start), VariableValue::ZonedTime(end)) => {
                let end_time = end.time();
                let duration = end_time.signed_duration_since(*start);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::LocalTime(start), VariableValue::ZonedDateTime(end)) => {
                let end_datetime = end.datetime();
                let start_datetime = match end_datetime
                    .date_naive()
                    .and_time(*start)
                    .and_local_timezone(*end_datetime.offset())
                {
                    LocalResult::Single(start_datetime) => start_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };
                let duration = end_datetime.signed_duration_since(start_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::ZonedTime(start), VariableValue::ZonedTime(end)) => {
                let dummy_date = match NaiveDate::from_ymd_opt(2020, 1, 1) {
                    Some(date) => date,
                    None => unreachable!(),
                };
                let start_datetime = match dummy_date
                    .and_time(*start.time())
                    .and_local_timezone(*start.offset())
                {
                    LocalResult::Single(start_datetime) => start_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidArgument(0),
                        })
                    }
                };
                let end_datetime = match dummy_date
                    .and_time(*end.time())
                    .and_local_timezone(*end.offset())
                {
                    LocalResult::Single(end_datetime) => end_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidArgument(1),
                        })
                    }
                };

                let start_time = start_datetime.fixed_offset();
                let end_time = end_datetime.fixed_offset();
                let duration = end_time.signed_duration_since(start_time);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::ZonedTime(start), VariableValue::Date(_end)) => {
                let end_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
                let start_time = start.time();

                let duration = end_time.signed_duration_since(*start_time);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::ZonedTime(start), VariableValue::LocalTime(end)) => {
                let start_time = start.time();

                let duration = end.signed_duration_since(*start_time);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::ZonedTime(start), VariableValue::LocalDateTime(end)) => {
                let start_offset = start.offset();
                let end_datetime = match end
                    .date()
                    .and_time(end.time())
                    .and_local_timezone(*start_offset)
                {
                    LocalResult::Single(end_datetime) => end_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };
                let start_datetime = match end
                    .date()
                    .and_time(*start.time())
                    .and_local_timezone(*start_offset)
                {
                    LocalResult::Single(start_datetime) => start_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };

                let duration = end_datetime.signed_duration_since(start_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::ZonedTime(start), VariableValue::ZonedDateTime(end)) => {
                let start_date = end.datetime().date_naive();

                let start_datetime = match start_date
                    .and_time(*start.time())
                    .and_local_timezone(*start.offset())
                {
                    LocalResult::Single(start_datetime) => start_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };

                let fixed_start_datetime = start_datetime.fixed_offset();
                let fixed_end_datetime = end.datetime().fixed_offset();
                let duration = fixed_end_datetime.signed_duration_since(fixed_start_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::LocalDateTime(start), VariableValue::LocalDateTime(end)) => {
                let duration = end.signed_duration_since(*start);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::LocalDateTime(start), VariableValue::Date(end)) => {
                let end_datetime = match end.and_hms_opt(0, 0, 0) {
                    Some(end_time) => end_time,
                    None => unreachable!(),
                };
                let duration = end_datetime.signed_duration_since(*start);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::LocalDateTime(start), VariableValue::LocalTime(end)) => {
                let end_date = start.date();
                let end_datetime = end_date.and_time(*end);
                let duration = end_datetime.signed_duration_since(*start);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::LocalDateTime(start), VariableValue::ZonedTime(end)) => {
                let offset = end.offset();
                let end_datetime = match start
                    .date()
                    .and_time(*end.time())
                    .and_local_timezone(*offset)
                {
                    LocalResult::Single(end_datetime) => end_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_ZONED_TIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };
                let start_datetime = match start.and_local_timezone(*offset) {
                    LocalResult::Single(start_datetime) => start_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };

                let duration = end_datetime.signed_duration_since(start_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::LocalDateTime(start), VariableValue::ZonedDateTime(end)) => {
                let start_offset = end.datetime().offset();
                let start_datetime = match start.and_local_timezone(*start_offset) {
                    LocalResult::Single(start_datetime) => start_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };

                let duration = end.datetime().signed_duration_since(start_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::ZonedDateTime(start), VariableValue::ZonedDateTime(end)) => {
                let duration = end.datetime().signed_duration_since(start.datetime());
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::ZonedDateTime(start), VariableValue::Date(end)) => {
                let start_datetime = start.datetime();
                let end_datetime =
                    match end.and_hms_opt(0, 0, 0) {
                        Some(end_datetime) => {
                            match end_datetime.and_local_timezone(*start_datetime.offset()) {
                                LocalResult::Single(end_datetime) => end_datetime,
                                _ => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            }
                        }
                        None => {
                            unreachable!()
                        }
                    };
                let duration = end_datetime.signed_duration_since(start_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::ZonedDateTime(start), VariableValue::LocalTime(end)) => {
                let start_datetime = start.datetime();
                let end_datetime = match start_datetime
                    .date_naive()
                    .and_time(*end)
                    .and_local_timezone(*start_datetime.offset())
                {
                    LocalResult::Single(end_datetime) => end_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };
                let duration = end_datetime.signed_duration_since(start_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::ZonedDateTime(start), VariableValue::ZonedTime(end)) => {
                let end_date = start.datetime().date_naive();

                let end_datetime = match end_date
                    .and_time(*end.time())
                    .and_local_timezone(*end.offset())
                {
                    LocalResult::Single(end_datetime) => end_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };

                let fixed_end_datetime = end_datetime.fixed_offset();
                let fixed_start_datetime = start.datetime().fixed_offset();
                let duration = fixed_start_datetime.signed_duration_since(fixed_end_datetime);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::ZonedDateTime(start), VariableValue::LocalDateTime(end)) => {
                let start_offset = start.datetime().offset();
                let end_datetime = match end.and_local_timezone(*start_offset) {
                    LocalResult::Single(end_datetime) => end_datetime,
                    _ => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };

                let duration = end_datetime.signed_duration_since(*start.datetime());
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::from(duration),
                    0,
                    0,
                )))
            }
            (VariableValue::Null, _) | (_, VariableValue::Null) => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct InMonths {}

#[async_trait]
impl ScalarFunction for InMonths {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match (&args[0], &args[1]) {
            (VariableValue::Date(start), VariableValue::Date(end)) => {
                let end_date = end;
                let start_date = start;

                let (years_diff, months_diff) = calculate_year_month(start_date, end_date);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::days(0),
                    years_diff,
                    months_diff,
                )))
            }
            (VariableValue::Date(_start), VariableValue::LocalTime(_end)) => Ok(
                VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, 0)),
            ),
            (VariableValue::Date(_start), VariableValue::ZonedTime(_end)) => Ok(
                VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, 0)),
            ),
            (VariableValue::Date(start), VariableValue::LocalDateTime(end)) => {
                let end_date = end.date();
                let start_date = start;

                let (years_diff, months_diff) = calculate_year_month(start_date, &end_date);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::days(0),
                    years_diff,
                    months_diff,
                )))
            }
            (VariableValue::Date(start), VariableValue::ZonedDateTime(end)) => {
                let end_date = end.datetime().date_naive();
                let start_date = start;

                let (years_diff, months_diff) = calculate_year_month(start_date, &end_date);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::days(0),
                    years_diff,
                    months_diff,
                )))
            }
            (VariableValue::LocalTime(_), VariableValue::LocalTime(_))
            | (VariableValue::LocalTime(_), VariableValue::Date(_))
            | (VariableValue::LocalTime(_), VariableValue::LocalDateTime(_))
            | (VariableValue::LocalTime(_), VariableValue::ZonedTime(_))
            | (VariableValue::LocalTime(_), VariableValue::ZonedDateTime(_)) => Ok(
                VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, 0)),
            ),
            (VariableValue::ZonedTime(_), VariableValue::LocalTime(_))
            | (VariableValue::ZonedTime(_), VariableValue::Date(_))
            | (VariableValue::ZonedTime(_), VariableValue::LocalDateTime(_))
            | (VariableValue::ZonedTime(_), VariableValue::ZonedTime(_))
            | (VariableValue::ZonedTime(_), VariableValue::ZonedDateTime(_)) => Ok(
                VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, 0)),
            ),
            (VariableValue::LocalDateTime(start), VariableValue::LocalDateTime(end)) => {
                let end_date = end.date();
                let start_date = start.date();

                let (years_diff, months_diff) = calculate_year_month(&start_date, &end_date);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::days(0),
                    years_diff,
                    months_diff,
                )))
            }
            (VariableValue::LocalDateTime(_), VariableValue::LocalTime(_))
            | (VariableValue::LocalDateTime(_), VariableValue::ZonedTime(_)) => Ok(
                VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, 0)),
            ),
            (VariableValue::LocalDateTime(start), VariableValue::Date(end)) => {
                let end_date = end;
                let start_date = start.date();

                let (years_diff, months_diff) = calculate_year_month(&start_date, end_date);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::days(0),
                    years_diff,
                    months_diff,
                )))
            }
            (VariableValue::LocalDateTime(start), VariableValue::ZonedDateTime(end)) => {
                let end_date = end.datetime().date_naive();
                let start_date = start.date();

                let (years_diff, months_diff) = calculate_year_month(&start_date, &end_date);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::days(0),
                    years_diff,
                    months_diff,
                )))
            }
            (VariableValue::ZonedDateTime(start), VariableValue::ZonedDateTime(end)) => {
                let end_date = end.datetime().fixed_offset().date_naive();
                let start_date = start.datetime().fixed_offset().date_naive();

                let (years_diff, months_diff) = calculate_year_month(&start_date, &end_date);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::days(0),
                    years_diff,
                    months_diff,
                )))
            }
            (VariableValue::ZonedDateTime(start), VariableValue::LocalDateTime(end)) => {
                let end_date = end.date();
                let start_date = start.datetime().fixed_offset().date_naive();

                let (years_diff, months_diff) = calculate_year_month(&start_date, &end_date);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::days(0),
                    years_diff,
                    months_diff,
                )))
            }
            (VariableValue::ZonedDateTime(start), VariableValue::Date(end)) => {
                let end_date = end;
                let start_date = start.datetime().fixed_offset().date_naive();

                let (years_diff, months_diff) = calculate_year_month(&start_date, end_date);
                Ok(VariableValue::Duration(Duration::new(
                    ChronoDuration::days(0),
                    years_diff,
                    months_diff,
                )))
            }
            (VariableValue::ZonedDateTime(_), VariableValue::LocalTime(_))
            | (VariableValue::ZonedDateTime(_), VariableValue::ZonedTime(_)) => Ok(
                VariableValue::Duration(Duration::new(ChronoDuration::days(0), 0, 0)),
            ),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug)]
pub struct InDays {}

#[async_trait]
impl ScalarFunction for InDays {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let between = Between {};
        let result = between.call(_context, expression, args.clone()).await;
        match result {
            Ok(return_value) => {
                let days = match return_value.as_duration() {
                    Some(duration) => duration.duration().num_days(),
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };
                let duration =
                    VariableValue::Duration(Duration::new(ChronoDuration::days(days), 0, 0));
                Ok(duration)
            }
            Err(_error) => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidType {
                    expected: "Duration".to_string(),
                },
            }),
        }
    }
}

#[derive(Debug)]
pub struct InSeconds {}

#[async_trait]
impl ScalarFunction for InSeconds {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() != 2 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        let between = Between {};
        let result = between.call(_context, expression, args.clone()).await;
        match result {
            Ok(return_value) => {
                let seconds = match return_value.as_duration() {
                    Some(duration) => match duration.duration().num_nanoseconds() {
                        Some(seconds) => seconds,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidFormat {
                                    expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR
                                        .to_string(),
                                },
                            })
                        }
                    },
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };
                let duration = VariableValue::Duration(Duration::new(
                    ChronoDuration::nanoseconds(seconds),
                    0,
                    0,
                ));
                Ok(duration)
            }
            Err(_error) => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidType {
                    expected: "Duration".to_string(),
                },
            }),
        }
    }
}

fn calculate_year_month(start: &NaiveDate, end: &NaiveDate) -> (i64, i64) {
    let months_diff = end.month() as i64 - start.month() as i64;
    let years_diff = (end.year() - start.year()) as i64;

    let result_month = if months_diff >= 0 {
        (years_diff * 12) + months_diff
    } else {
        (years_diff - 1) * 12 + (months_diff + 12)
    };

    (result_month / 12, result_month % 12)
}

async fn parse_duration_input(duration_str: &str) -> Result<Duration, FunctionError> {
    let mut duration_result = ChronoDuration::days(0);

    //Durtion string must start with 'P'
    let duration = match duration_str.strip_prefix('P') {
        Some(duration) => duration,
        None => {
            return Err(FunctionError {
                function_name: "Duration".to_string(),
                error: FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR.to_string(),
                },
            })
        }
    };

    let date_duration = duration.split('T').next();
    let mut time_duration = None;
    if duration.contains('T') {
        time_duration = duration.split('T').last();
    }

    let mut duration_years = 0;
    let mut duration_months = 0;

    if let Some(date_duration) = date_duration {
        let pattern = r"(\d{1,19}Y)?(\d{1,19}M)?(\d{1,19}W)?(\d{1,19}(?:\.\d{1,9})?D)?";
        let re = match Regex::new(pattern) {
            Ok(re) => re,
            Err(_) => {
                return Err(FunctionError {
                    function_name: "Duration".to_string(),
                    error: FunctionEvaluationError::InvalidType {
                        expected: "a valid regex".to_string(),
                    },
                })
            }
        };

        if let Some(cap) = re.captures(date_duration) {
            for part in cap.iter().skip(1).flatten() {
                let matched_value = part.as_str();
                if matched_value.contains('Y') {
                    let substring = &matched_value[..(matched_value.len() - 1)];
                    if let Ok(years) = substring.parse::<i64>() {
                        duration_years = years;
                    } else {
                        return Err(FunctionError {
                            function_name: "Duration".to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR
                                    .to_string(),
                            },
                        });
                    }
                }
                if matched_value.contains('M') {
                    let substring = &matched_value[..(matched_value.len() - 1)];
                    if let Ok(months) = substring.parse::<i64>() {
                        duration_months = months;
                    } else {
                        return Err(FunctionError {
                            function_name: "Duration".to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR
                                    .to_string(),
                            },
                        });
                    }
                }
                if matched_value.contains('W') {
                    let substring = &matched_value[..(matched_value.len() - 1)];
                    if let Ok(weeks) = substring.parse::<i64>() {
                        duration_result += ChronoDuration::weeks(weeks);
                    } else {
                        return Err(FunctionError {
                            function_name: "Duration".to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR
                                    .to_string(),
                            },
                        });
                    }
                }
                if matched_value.contains('D') {
                    let substring = &matched_value[..(matched_value.len() - 1)];
                    if substring.contains('.') {
                        if let Ok(days) = substring.parse::<f64>() {
                            duration_result +=
                                ChronoDuration::nanoseconds((days * 86400000000000.0) as i64);
                        } else {
                            return Err(FunctionError {
                                function_name: "Duration".to_string(),
                                error: FunctionEvaluationError::InvalidFormat {
                                    expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR
                                        .to_string(),
                                },
                            });
                        }
                    } else if let Ok(days) = substring.parse::<i64>() {
                        duration_result += ChronoDuration::days(days);
                    } else {
                        return Err(FunctionError {
                            function_name: "Duration".to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR
                                    .to_string(),
                            },
                        });
                    }
                }
            }
        }
    }

    if let Some(time_duration) = time_duration {
        let time_duration_string = format!("PT{}", time_duration);

        let iso_duration = match time_duration_string.parse::<IsoDuration>() {
            Ok(iso_duration) => iso_duration,
            Err(_) => {
                return Err(FunctionError {
                    function_name: "Duration".to_string(),
                    error: FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR.to_string(),
                    },
                })
            }
        };
        let seconds = match iso_duration.num_seconds() {
            Some(seconds) => seconds,
            None => {
                return Err(FunctionError {
                    function_name: "Duration".to_string(),
                    error: FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR.to_string(),
                    },
                })
            }
        };

        if time_duration_string.contains('.') {
            let mut fract_string = match time_duration_string.split('.').last() {
                Some(fract_string) => fract_string,
                None => {
                    return Err(FunctionError {
                        function_name: "Duration".to_string(),
                        error: FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR.to_string(),
                        },
                    })
                }
            };
            fract_string = &fract_string[..fract_string.len() - 1];
            let nanoseconds = match fract_string.parse::<i64>() {
                Ok(nanoseconds) => nanoseconds,
                Err(_) => {
                    return Err(FunctionError {
                        function_name: "Duration".to_string(),
                        error: FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_DURATION_FORMAT_ERROR.to_string(),
                        },
                    })
                }
            };
            duration_result += ChronoDuration::nanoseconds(nanoseconds * 100_000_000_i64);
        }
        duration_result += ChronoDuration::seconds(seconds.trunc() as i64);
    }

    let result = Duration::new(duration_result, duration_years, duration_months);
    Ok(result)
}
