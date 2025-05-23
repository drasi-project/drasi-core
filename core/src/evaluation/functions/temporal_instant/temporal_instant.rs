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

use crate::evaluation::temporal_constants;
use crate::evaluation::variable_value::zoned_datetime::ZonedDateTime;
use crate::evaluation::variable_value::zoned_time::ZonedTime;
use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{ExpressionEvaluationContext, FunctionError, FunctionEvaluationError};
use chrono::{
    prelude::*, Datelike, Days, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime,
    NaiveTime, TimeZone, Timelike, Weekday,
};
use chrono_tz::Tz;
use log::error;
use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;

use regex::Regex;

#[derive(Debug, PartialEq)]
pub struct Date {}

#[async_trait]
impl ScalarFunction for Date {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() > 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        if args.is_empty() {
            // current date
            let local = Local::now();
            let date = local.date_naive();
            return Ok(VariableValue::Date(date));
        }
        match &args[0] {
            VariableValue::String(s) => {
                let date_str = s.as_str();

                match parse_date_string(date_str).await {
                    Ok(result_date) => Ok(VariableValue::Date(result_date)),
                    Err(e) => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: e,
                    }),
                }
            }
            VariableValue::Object(o) => {
                let valid_keys: HashSet<String> = [
                    "year",
                    "month",
                    "week",
                    "day",
                    "ordinalDay",
                    "quarter",
                    "dayOfWeek",
                    "dayOfQuarter",
                    "timezone",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();

                let invalid_keys: Vec<_> =
                    o.keys().filter(|&key| !valid_keys.contains(key)).collect();
                if !invalid_keys.is_empty() {
                    error!("Invalid keys in the date object");
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(0),
                    });
                }
                let result = create_date_from_componet(o.clone()).await;
                match result {
                    Ok(date) => Ok(date),
                    Err(e) => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: e,
                    }),
                }
            }
            VariableValue::Date(d) => Ok(VariableValue::Date(*d)),
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct LocalTime {}

#[async_trait]
impl ScalarFunction for LocalTime {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() > 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        if args.is_empty() {
            let local = Local::now().time();
            return Ok(VariableValue::LocalTime(local));
        }
        match &args[0] {
            VariableValue::String(s) => {
                let time_str = s.as_str();
                let time = parse_local_time_input(time_str).await;
                match time {
                    Ok(result_time) => Ok(VariableValue::LocalTime(result_time)),
                    _ => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR
                                .to_string(),
                        },
                    }),
                }
            }
            VariableValue::Object(o) => {
                let valid_keys: HashSet<String> = [
                    "hour",
                    "minute",
                    "second",
                    "millisecond",
                    "microsecond",
                    "nanosecond",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();

                let invalid_keys: Vec<_> =
                    o.keys().filter(|&key| !valid_keys.contains(key)).collect();
                if !invalid_keys.is_empty() {
                    error!("Invalid keys in the localtime object");
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(0),
                    });
                }
                let result = create_time_from_componet(o.clone()).await;
                match result {
                    Ok(time) => Ok(time),
                    Err(e) => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: e,
                    }),
                }
            }
            VariableValue::LocalTime(t) => Ok(VariableValue::LocalTime(*t)),
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct LocalDateTime {}

#[async_trait]
impl ScalarFunction for LocalDateTime {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() > 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        if args.is_empty() {
            // current datetime
            let local = Local::now();
            let datetime = local.naive_local();
            return Ok(VariableValue::LocalDateTime(datetime));
        }
        match &args[0] {
            VariableValue::String(s) => {
                let datetime_str = s.as_str();
                let datetime = parse_local_date_time_input(datetime_str).await;
                match datetime {
                    Ok(result_datetime) => Ok(VariableValue::LocalDateTime(result_datetime)),
                    _ => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR
                                .to_string(),
                        },
                    }),
                }
            }
            VariableValue::Object(o) => {
                let valid_keys: HashSet<String> = vec![
                    "year",
                    "month",
                    "week",
                    "day",
                    "ordinalDay",
                    "quarter",
                    "dayOfWeek",
                    "dayOfQuarter",
                    "hour",
                    "minute",
                    "second",
                    "millisecond",
                    "microsecond",
                    "nanosecond",
                    "timezone",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();

                let invalid_keys: Vec<_> =
                    o.keys().filter(|&key| !valid_keys.contains(key)).collect();
                if !invalid_keys.is_empty() {
                    error!("Invalid keys in the localdatetime object");
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(0),
                    });
                }
                if o.get("timezone").is_some() {
                    // retrieve the naivedatetime from the timezone
                    let tz = match o.get("timezone") {
                        Some(tz) => {
                            let tz_str = match tz.as_str() {
                                Some(tz_str) => tz_str,
                                None => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            };
                            let tz: Tz = match tz_str.parse() {
                                Ok(tz) => tz,
                                Err(_) => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            };
                            tz
                        }
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidFormat {
                                    expected:
                                        temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR
                                            .to_string(),
                                },
                            })
                        }
                    };

                    let local: chrono::DateTime<Tz> = Local::now().with_timezone(&tz);
                    return Ok(VariableValue::LocalDateTime(local.naive_local()));
                }

                let date_result = match create_date_from_componet(o.clone()).await {
                    Ok(date) => date,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                let time_result = match create_time_from_componet(o.clone()).await {
                    Ok(time) => time,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                let local_time = match time_result.as_local_time() {
                    Some(time) => time,
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidFormat {
                                expected: temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR
                                    .to_string(),
                            },
                        })
                    }
                };
                let datetime = match date_result.as_date() {
                    Some(date) => date.and_time(local_time),
                    None => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: FunctionEvaluationError::InvalidArgument(0),
                        })
                    }
                };
                Ok(VariableValue::LocalDateTime(datetime))
            }
            VariableValue::LocalDateTime(dt) => Ok(VariableValue::LocalDateTime(*dt)),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Time {}

#[async_trait]
impl ScalarFunction for Time {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() > 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        if args.is_empty() {
            let local = Local::now().time();
            let timezone = *temporal_constants::UTC_FIXED_OFFSET;
            return Ok(VariableValue::ZonedTime(ZonedTime::new(local, timezone)));
        }
        match &args[0] {
            VariableValue::String(s) => {
                let time_str = s.as_str();
                let time = parse_zoned_time_input(time_str).await;
                match time {
                    Ok(result_time) => Ok(VariableValue::ZonedTime(result_time)),
                    _ => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_ZONED_TIME_FORMAT_ERROR
                                .to_string(),
                        },
                    }),
                }
            }
            VariableValue::Object(o) => {
                let valid_keys: HashSet<String> = [
                    "hour",
                    "minute",
                    "second",
                    "millisecond",
                    "microsecond",
                    "nanosecond",
                    "timezone",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();

                let invalid_keys: Vec<_> =
                    o.keys().filter(|&key| !valid_keys.contains(key)).collect();
                if !invalid_keys.is_empty() {
                    error!("Invalid keys in the time object");
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(0),
                    });
                }

                let timezone = match o.get("timezone") {
                    Some(tz) => {
                        let tz_str = match tz {
                            VariableValue::String(s) => s.as_str(),
                            _ => "UTC",
                        };
                        match handle_iana_timezone(tz_str) {
                            Some(tz) => tz,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_TIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                })
                            }
                        }
                    }
                    None => Tz::UTC,
                };

                // if only the timezone is specified, return the current time in that timezone
                if o.get("timezone").is_some() && o.len() == 1 {
                    let local: chrono::DateTime<Tz> = Local::now().with_timezone(&timezone);
                    let fixed_offset = local.offset().fix();
                    return Ok(VariableValue::ZonedTime(ZonedTime::new(
                        local.time(),
                        fixed_offset,
                    )));
                }

                let result = create_time_from_componet(o.clone()).await;
                match result {
                    Ok(time) => {
                        let dummy_date = *temporal_constants::EPOCH_NAIVE_DATE;
                        let local_time = match time.as_local_time() {
                            Some(time) => time,
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_TIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                })
                            }
                        };
                        let datetime = dummy_date.and_time(local_time);
                        let zoned_datetime = match timezone.from_local_datetime(&datetime) {
                            LocalResult::Single(zoned_datetime) => zoned_datetime.fixed_offset(),
                            _ => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_TIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                })
                            }
                        };
                        let zoned_time = zoned_datetime.time();
                        let offset = zoned_datetime.offset();
                        let default = VariableValue::String("UTC".to_string());
                        let timezone_str = match o.get("timezone").unwrap_or(&default) {
                            VariableValue::String(s) => s.as_str(),
                            _ => "UTC",
                        };
                        if timezone_str.contains('+') || timezone_str.contains('-') {
                            let offset =
                                match FixedOffset::from_str(timezone_str) {
                                    Ok(offset) => offset,
                                    Err(_) => return Err(FunctionError {
                                        function_name: expression.name.to_string(),
                                        error: FunctionEvaluationError::InvalidFormat {
                                            expected:
                                                temporal_constants::INVALID_ZONED_TIME_FORMAT_ERROR
                                                    .to_string(),
                                        },
                                    }),
                                };
                            return Ok(VariableValue::ZonedTime(ZonedTime::new(
                                zoned_time, offset,
                            )));
                        }
                        return Ok(VariableValue::ZonedTime(ZonedTime::new(
                            zoned_time, *offset,
                        )));
                    }
                    Err(e) => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: e,
                    }),
                }
            }
            VariableValue::ZonedTime(t) => Ok(VariableValue::ZonedTime(*t)),
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct DateTime {}

#[async_trait]
impl ScalarFunction for DateTime {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() > 1 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }

        if args.is_empty() {
            let local = Local::now();
            let datetime = local.with_timezone(&*temporal_constants::UTC_FIXED_OFFSET);
            return Ok(VariableValue::ZonedDateTime(ZonedDateTime::new(
                datetime, None,
            )));
        }
        match &args[0] {
            VariableValue::String(s) => {
                let time_str = s.as_str();
                let time = ZonedDateTime::from_string(time_str);
                match time {
                    Ok(result_time) => Ok(VariableValue::ZonedDateTime(result_time)),
                    _ => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                .to_string(),
                        },
                    }),
                }
            }
            VariableValue::Object(o) => {
                let valid_keys: HashSet<String> = vec![
                    "year",
                    "month",
                    "week",
                    "day",
                    "ordinalDay",
                    "quarter",
                    "dayOfWeek",
                    "dayOfQuarter",
                    "hour",
                    "minute",
                    "second",
                    "millisecond",
                    "microsecond",
                    "nanosecond",
                    "timezone",
                    "epochSeconds",
                    "epochMillis",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();

                let invalid_keys: Vec<_> =
                    o.keys().filter(|&key| !valid_keys.contains(key)).collect();
                if !invalid_keys.is_empty() {
                    error!("Invalid keys in the datetime object");
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(0),
                    });
                }
                if o.contains_key("epochSeconds") || o.contains_key("epochMillis") {
                    let result = match create_date_time_from_epoch(o.clone()).await {
                        Some(datetime) => datetime,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidFormat {
                                    expected:
                                        temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                            .to_string(),
                                },
                            })
                        }
                    };

                    return Ok(result);
                }
                let timezone =
                    match o.get("timezone") {
                        Some(tz) => {
                            let tz_str = match tz.as_str() {
                                Some(tz_str) => tz_str,
                                None => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            };
                            match handle_iana_timezone(tz_str) {
                                Some(tz) => tz,
                                None => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            }
                        }
                        None => Tz::UTC,
                    };

                if o.get("timezone").is_some() && o.len() == 1 {
                    let local: chrono::DateTime<Tz> = Local::now().with_timezone(&timezone);
                    let zoned_time = match timezone.from_local_datetime(&local.naive_local()) {
                        LocalResult::Single(zoned_time) => zoned_time.fixed_offset(),
                        _ => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidFormat {
                                    expected:
                                        temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                            .to_string(),
                                },
                            })
                        }
                    };
                    return Ok(VariableValue::ZonedDateTime(ZonedDateTime::new(
                        zoned_time, None,
                    )));
                }
                let naive_date = match create_date_from_componet(o.clone()).await {
                    Ok(date) => match date.as_date() {
                        Some(date) => date,
                        None => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidFormat {
                                    expected:
                                        temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                            .to_string(),
                                },
                            })
                        }
                    },
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                let result = create_time_from_componet(o.clone()).await;
                match result {
                    Ok(time) => {
                        let local_time =
                            match time.as_local_time() {
                                Some(time) => time,
                                None => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            };
                        let datetime = naive_date.and_time(local_time);
                        let timezone_str = match o.get("timezone") {
                            Some(VariableValue::String(s)) => s.as_str(),
                            _ => "UTC",
                        };
                        if timezone_str.contains('+') || timezone_str.contains('-') {
                            let offset = match FixedOffset::from_str(timezone_str) {
                                Ok(offset) => offset,
                                Err(_) => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            };
                            let datetime_offset = match datetime.and_local_timezone(offset) {
                                LocalResult::Single(offset) => offset.fixed_offset(),
                                _ => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            };
                            return Ok(VariableValue::ZonedDateTime(ZonedDateTime::new(
                                datetime_offset,
                                Some(timezone_str.to_string()),
                            )));
                        }
                        let zoned_datetime = match timezone.from_local_datetime(&datetime) {
                            LocalResult::Single(zoned_datetime) => zoned_datetime.fixed_offset(),
                            _ => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                })
                            }
                        };
                        return Ok(VariableValue::ZonedDateTime(ZonedDateTime::new(
                            zoned_datetime,
                            Some(timezone_str.to_string()),
                        )));
                    }
                    Err(e) => Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: e,
                    }),
                }
            }
            VariableValue::ZonedDateTime(t) => Ok(VariableValue::ZonedDateTime(t.clone())),
            VariableValue::Null => Ok(VariableValue::Null),
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

async fn create_date_from_componet(
    o: BTreeMap<String, VariableValue>,
) -> Result<VariableValue, FunctionEvaluationError> {
    let year = match o.get("year") {
        Some(year) => match year.as_i64() {
            Some(year) => year,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let week = match o.get("week") {
        Some(week) => match week.as_i64() {
            Some(week) => week,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    if week > 0 {
        let day_of_week = match o.get("dayOfWeek") {
            Some(day_of_week) => match day_of_week.as_i64() {
                Some(day_of_week) => day_of_week,
                None => return Err(FunctionEvaluationError::OverflowError),
            },
            None => 0,
        };
        let begin_date = match NaiveDate::from_isoywd_opt(year as i32, week as u32, Weekday::Mon) {
            Some(begin_date) => begin_date,
            None => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                })
            }
        };
        let date = begin_date + Duration::days(day_of_week - 1);
        return Ok(VariableValue::Date(date));
    }
    let ordinal_day = match o.get("ordinalDay") {
        Some(ordinal_day) => match ordinal_day.as_i64() {
            Some(ordinal_day) => ordinal_day,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    if ordinal_day > 0 {
        let date = match NaiveDate::from_yo_opt(year as i32, ordinal_day as u32) {
            Some(date) => date,
            None => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                })
            }
        };
        return Ok(VariableValue::Date(date));
    }
    let quarter = match o.get("quarter") {
        Some(quarter) => match quarter.as_i64() {
            Some(quarter) => quarter,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    if quarter > 0 {
        let day_of_quarter = match o.get("dayOfQuarter") {
            Some(day_of_quarter) => match day_of_quarter.as_i64() {
                Some(day_of_quarter) => day_of_quarter,
                None => return Err(FunctionEvaluationError::OverflowError),
            },
            None => 0,
        };
        let month = (quarter - 1) * 3 + 1;
        let begin_date = match NaiveDate::from_ymd_opt(year as i32, month as u32, 1) {
            Some(begin_date) => begin_date,
            None => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                })
            }
        };
        let date = begin_date + Duration::days(day_of_quarter - 1);
        return Ok(VariableValue::Date(date));
    }
    let month = match o.get("month") {
        Some(month) => match month.as_i64() {
            Some(month) => month,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let day = match o.get("day") {
        Some(day) => match day.as_i64() {
            Some(day) => day,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let date = match NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32) {
        Some(date) => date,
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
            })
        }
    };
    Ok(VariableValue::Date(date))
}

async fn create_date_time_from_epoch(o: BTreeMap<String, VariableValue>) -> Option<VariableValue> {
    let nanoseconds = match o.get("nanosecond") {
        Some(nanoseconds) => nanoseconds.as_i64().unwrap_or(0),
        None => 0,
    };
    if o.contains_key("epochSeconds") {
        let epoch_seconds = match o.get("epochSeconds") {
            Some(epoch_seconds) => epoch_seconds.as_i64().unwrap_or(0),
            None => 0,
        };
        let datetime = match chrono::DateTime::from_timestamp(epoch_seconds, nanoseconds as u32) {
            Some(datetime) => datetime.naive_local(),
            None => return None,
        };
        let zoned_datetime =
            match datetime.and_local_timezone(*temporal_constants::UTC_FIXED_OFFSET) {
                LocalResult::Single(zoned_datetime) => zoned_datetime,
                _ => return None,
            };
        return Some(VariableValue::ZonedDateTime(ZonedDateTime::new(
            zoned_datetime,
            None,
        )));
    } else if o.contains_key("epochMillis") {
        let epoch_millis = match o.get("epochMillis") {
            Some(epoch_millis) => epoch_millis.as_i64().unwrap_or(0),
            None => 0,
        };
        let datetime_epoch = match chrono::DateTime::from_timestamp_millis(epoch_millis) {
            Some(datetime_epoch) => datetime_epoch.naive_local(),
            None => return None,
        };
        let datetime = datetime_epoch + Duration::nanoseconds(nanoseconds);
        let zoned_datetime =
            match datetime.and_local_timezone(*temporal_constants::UTC_FIXED_OFFSET) {
                LocalResult::Single(zoned_datetime) => zoned_datetime,
                _ => return None,
            };
        return Some(VariableValue::ZonedDateTime(ZonedDateTime::new(
            zoned_datetime,
            None,
        )));
    }

    None
}

async fn create_time_from_componet(
    o: BTreeMap<String, VariableValue>,
) -> Result<VariableValue, FunctionEvaluationError> {
    let hour = match o.get("hour") {
        Some(hour) => match hour.as_i64() {
            Some(hour) => hour,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let minute = match o.get("minute") {
        Some(minute) => match minute.as_i64() {
            Some(minute) => minute,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let second = match o.get("second") {
        Some(second) => match second.as_i64() {
            Some(second) => second,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let millisecond = match o.get("millisecond") {
        Some(millisecond) => match millisecond.as_i64() {
            Some(millisecond) => millisecond,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let microsecond = match o.get("microsecond") {
        Some(microsecond) => match microsecond.as_i64() {
            Some(microsecond) => microsecond,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let nanosecond = match o.get("nanosecond") {
        Some(nanosecond) => match nanosecond.as_i64() {
            Some(nanosecond) => nanosecond,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };

    let mut time = match NaiveTime::from_hms_opt(hour as u32, minute as u32, second as u32) {
        Some(time) => time,
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
            })
        }
    };
    time = time
        + Duration::nanoseconds(nanosecond)
        + Duration::microseconds(microsecond)
        + Duration::milliseconds(millisecond);
    Ok(VariableValue::LocalTime(time))
}

#[derive(Debug, PartialEq)]
pub struct Truncate {}

#[async_trait]
impl ScalarFunction for Truncate {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        if args.len() < 2 || args.len() > 3 {
            return Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgumentCount,
            });
        }
        match (&args[0], &args[1], &args.get(2)) {
            (VariableValue::String(s), VariableValue::Date(d), None) => {
                let truncated_date = match truncate_date(s.to_string(), *d).await {
                    Ok(date) => date,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                Ok(VariableValue::Date(truncated_date))
            }
            (VariableValue::String(s), VariableValue::Date(d), Some(VariableValue::Object(m))) => {
                let truncated_date =
                    match truncate_date_with_map(s.to_string(), *d, m.clone()).await {
                        Ok(date) => date,
                        Err(e) => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: e,
                            })
                        }
                    };
                Ok(VariableValue::Date(truncated_date))
            }
            (VariableValue::String(s), VariableValue::LocalTime(d), None) => {
                let truncated_time = match truncate_local_time(s.to_string(), *d).await {
                    Ok(time) => time,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                Ok(VariableValue::LocalTime(truncated_time))
            }
            (
                VariableValue::String(s),
                VariableValue::LocalTime(d),
                Some(VariableValue::Object(m)),
            ) => {
                let truncated_time =
                    match truncate_local_time_with_map(s.to_string(), *d, m.clone()).await {
                        Ok(time) => time,
                        Err(e) => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: e,
                            })
                        }
                    };
                Ok(VariableValue::LocalTime(truncated_time))
            }
            (VariableValue::String(s), VariableValue::LocalDateTime(dt), None) => {
                let truncated_date = match truncate_date(s.to_string(), dt.date()).await {
                    Ok(date) => date,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                let truncated_time = match truncate_local_time(s.to_string(), dt.time()).await {
                    Ok(time) => time,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                let truncated_date_time = NaiveDateTime::new(truncated_date, truncated_time);
                Ok(VariableValue::LocalDateTime(truncated_date_time))
            }
            (
                VariableValue::String(s),
                VariableValue::LocalDateTime(dt),
                Some(VariableValue::Object(m)),
            ) => {
                let truncated_date =
                    match truncate_date_with_map(s.to_string(), dt.date(), m.clone()).await {
                        Ok(date) => date,
                        Err(e) => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: e,
                            })
                        }
                    };
                let truncated_time =
                    match truncate_local_time_with_map(s.to_string(), dt.time(), m.clone()).await {
                        Ok(time) => time,
                        Err(e) => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: e,
                            })
                        }
                    };
                let truncated_date_time = NaiveDateTime::new(truncated_date, truncated_time);
                Ok(VariableValue::LocalDateTime(truncated_date_time))
            }
            (VariableValue::String(s), VariableValue::ZonedTime(t), None) => {
                let naive_time = *t.time();
                let offset = *t.offset();
                let truncated_time = match truncate_local_time(s.to_string(), naive_time).await {
                    Ok(time) => time,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                let zoned_time_result = ZonedTime::new(truncated_time, offset);
                Ok(VariableValue::ZonedTime(zoned_time_result))
            }
            (
                VariableValue::String(s),
                VariableValue::ZonedTime(t),
                Some(VariableValue::Object(m)),
            ) => {
                let naive_time = *t.time();
                let offset = *t.offset();
                let truncated_time = match truncate_local_time_with_map(
                    s.to_string(),
                    naive_time,
                    m.clone(),
                )
                .await
                {
                    Ok(time) => time,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                let zoned_time_result = ZonedTime::new(truncated_time, offset);
                Ok(VariableValue::ZonedTime(zoned_time_result))
            }
            (VariableValue::String(s), VariableValue::ZonedDateTime(dt), None) => {
                let datetime = *dt.datetime();
                let timezone = dt.timezone_name().clone();
                let naive_date = datetime.date_naive();
                let naive_time = datetime.time();
                let offset = datetime.offset();
                let truncated_time = match truncate_local_time(s.to_string(), naive_time).await {
                    Ok(time) => time,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                let truncate_date = match truncate_date(s.to_string(), naive_date).await {
                    Ok(date) => date,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                let truncated_naive_date_time = NaiveDateTime::new(truncate_date, truncated_time);
                let truncated_date_time = match truncated_naive_date_time
                    .and_local_timezone(*offset)
                {
                    LocalResult::Single(dt) => dt,
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
                let zoned_date_time = ZonedDateTime::new(truncated_date_time, timezone);
                Ok(VariableValue::ZonedDateTime(zoned_date_time))
            }
            (
                VariableValue::String(s),
                VariableValue::ZonedDateTime(dt),
                Some(VariableValue::Object(m)),
            ) => {
                let datetime = *dt.datetime();
                let timezone = dt.timezone_name().clone();
                let naive_date = datetime.date_naive();
                let naive_time = datetime.time();
                let offset = datetime.offset();
                let truncated_time = match truncate_local_time_with_map(
                    s.to_string(),
                    naive_time,
                    m.clone(),
                )
                .await
                {
                    Ok(time) => time,
                    Err(e) => {
                        return Err(FunctionError {
                            function_name: expression.name.to_string(),
                            error: e,
                        })
                    }
                };
                let truncate_date =
                    match truncate_date_with_map(s.to_string(), naive_date, m.clone()).await {
                        Ok(date) => date,
                        Err(e) => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: e,
                            })
                        }
                    };
                let truncated_naive_date_time = NaiveDateTime::new(truncate_date, truncated_time);
                if m.get("timezone").is_some() {
                    let timezone = match m.get("timezone") {
                        Some(tz) => {
                            let tz_str = match tz.as_str() {
                                Some(tz_str) => tz_str,
                                None => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            };
                            match handle_iana_timezone(tz_str) {
                                Some(tz) => tz,
                                None => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            }
                        }
                        None => unreachable!(),
                    };
                    let datetime_tz = match timezone.from_local_datetime(&truncated_naive_date_time)
                    {
                        LocalResult::Single(dt) => dt,
                        _ => {
                            return Err(FunctionError {
                                function_name: expression.name.to_string(),
                                error: FunctionEvaluationError::InvalidFormat {
                                    expected:
                                        temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                            .to_string(),
                                },
                            })
                        }
                    };
                    let datetime_fixed_offset = datetime_tz.fixed_offset();
                    let timezone_string =
                        match m.get("timezone") {
                            Some(tz) => match tz.as_str() {
                                Some(tz_str) => tz_str,
                                None => return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                }),
                            },
                            None => {
                                return Err(FunctionError {
                                    function_name: expression.name.to_string(),
                                    error: FunctionEvaluationError::InvalidFormat {
                                        expected:
                                            temporal_constants::INVALID_ZONED_DATETIME_FORMAT_ERROR
                                                .to_string(),
                                    },
                                })
                            }
                        };
                    let zoned_date_time = ZonedDateTime::new(
                        datetime_fixed_offset,
                        Some(timezone_string.to_string()),
                    );
                    return Ok(VariableValue::ZonedDateTime(zoned_date_time));
                }
                let truncated_date_time = match truncated_naive_date_time
                    .and_local_timezone(*offset)
                {
                    LocalResult::Single(dt) => dt,
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
                let zoned_date_time = ZonedDateTime::new(truncated_date_time, timezone);
                Ok(VariableValue::ZonedDateTime(zoned_date_time))
            }
            _ => Err(FunctionError {
                function_name: expression.name.to_string(),
                error: FunctionEvaluationError::InvalidArgument(0),
            }),
        }
    }
}

pub enum Clock {
    RealTime,
    Statement,
    Transaction,
}

pub enum ClockResult {
    Date,
    LocalTime,
    LocalDateTime,
    ZonedTime,
    ZonedDateTime,
}

pub struct ClockFunction {
    clock: Clock,
    result: ClockResult,
}

impl ClockFunction {
    pub fn new(clock: Clock, result: ClockResult) -> Self {
        Self { clock, result }
    }
}

#[async_trait]
impl ScalarFunction for ClockFunction {
    async fn call(
        &self,
        context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        let timestamp = match self.clock {
            Clock::RealTime => context.get_realtime(),
            Clock::Statement => context.get_realtime(),
            Clock::Transaction => context.get_transaction_time(),
        };

        if args.is_empty() {
            let zdt = ZonedDateTime::from_epoch_millis(timestamp as i64);
            return Ok(match self.result {
                ClockResult::Date => VariableValue::Date(zdt.datetime().date_naive()),
                ClockResult::LocalTime => VariableValue::LocalTime(zdt.datetime().time()),
                ClockResult::LocalDateTime => {
                    VariableValue::LocalDateTime(zdt.datetime().naive_utc())
                }
                ClockResult::ZonedTime => VariableValue::ZonedTime(ZonedTime::new(
                    zdt.datetime().time(),
                    *temporal_constants::UTC_FIXED_OFFSET,
                )),
                ClockResult::ZonedDateTime => VariableValue::ZonedDateTime(zdt),
            });
        } else {
            let timezone_string = match &args[0] {
                VariableValue::String(s) => s.as_str(),
                _ => {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(0),
                    })
                }
            };
            let tz = match handle_iana_timezone(timezone_string) {
                Some(tz) => tz,
                None => {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(0),
                    })
                }
            };
            let date_time = match tz.timestamp_millis_opt(timestamp as i64) {
                LocalResult::Single(dt) => dt.fixed_offset(),
                _ => {
                    return Err(FunctionError {
                        function_name: expression.name.to_string(),
                        error: FunctionEvaluationError::InvalidArgument(0),
                    })
                }
            };

            return Ok(match self.result {
                ClockResult::Date => VariableValue::Date(date_time.date_naive()),
                ClockResult::LocalTime => VariableValue::LocalTime(date_time.time()),
                ClockResult::LocalDateTime => VariableValue::LocalDateTime(date_time.naive_utc()),
                ClockResult::ZonedTime => {
                    VariableValue::ZonedTime(ZonedTime::new(date_time.time(), *date_time.offset()))
                }
                ClockResult::ZonedDateTime => VariableValue::ZonedDateTime(ZonedDateTime::new(
                    date_time,
                    Some(timezone_string.to_string()),
                )),
            });
        }
    }
}

async fn truncate_date(
    unit: String,
    date: NaiveDate,
) -> Result<NaiveDate, FunctionEvaluationError> {
    let year = date.year();
    let month = date.month();
    let day = date.day();

    let date_lowercase = unit.to_lowercase();
    let truncation_unit = date_lowercase.as_str();
    match truncation_unit {
        "millennium" => {
            let year = year / 1000 * 1000;
            Ok(match NaiveDate::from_ymd_opt(year, 1, 1) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            })
        }
        "century" => {
            let year = year / 100 * 100;
            Ok(match NaiveDate::from_ymd_opt(year, 1, 1) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            })
        }
        "decade" => {
            let year = year / 10 * 10;
            Ok(match NaiveDate::from_ymd_opt(year, 1, 1) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            })
        }
        "year" => Ok(match NaiveDate::from_ymd_opt(year, 1, 1) {
            Some(date) => date,
            None => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                })
            }
        }),
        "quarter" => {
            let month = month / 3 * 3 + 1;
            Ok(match NaiveDate::from_ymd_opt(year, month, 1) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            })
        }
        "month" => Ok(match NaiveDate::from_ymd_opt(year, month, 1) {
            Some(date) => date,
            None => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                })
            }
        }),
        "week" => {
            let weekday = date.weekday();
            let days_to_subtract = match weekday {
                Weekday::Mon => 0,
                Weekday::Tue => 1,
                Weekday::Wed => 2,
                Weekday::Thu => 3,
                Weekday::Fri => 4,
                Weekday::Sat => 5,
                Weekday::Sun => 6,
            };

            Ok(date - chrono::Duration::days(days_to_subtract as i64))
        }
        "day" => Ok(match NaiveDate::from_ymd_opt(year, month, day) {
            Some(date) => date,
            None => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                })
            }
        }),
        "weekyear" => {
            // First day of the first week of the year
            let date_string = format!("{}-1-1", year);
            let date = match NaiveDate::parse_from_str(&date_string, "%Y-%W-%u") {
                Ok(date) => date,
                Err(_) => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            };
            Ok(date)
        }
        "hour" | "minute" | "second" | "millisecond" | "microsecond" => {
            Ok(match NaiveDate::from_ymd_opt(year, month, day) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            })
        }
        _ => Err(FunctionEvaluationError::InvalidType {
            expected: "Valid truncation unit".to_string(),
        }),
    }
}

async fn truncate_date_with_map(
    unit: String,
    date: NaiveDate,
    map: BTreeMap<String, VariableValue>,
) -> Result<NaiveDate, FunctionEvaluationError> {
    let year = date.year();
    let month = date.month();
    let day = date.day();

    let years_to_add = match map.get("year") {
        Some(year) => match year.as_i64() {
            Some(year) => year - 1,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let months_to_add = match map.get("month") {
        Some(month) => match month.as_i64() {
            Some(month) => month - 1,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let days_to_add = match map.get("day") {
        Some(day) => match day.as_i64() {
            Some(day) => day - 1,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let days_of_week_to_add = match map.get("dayOfWeek") {
        Some(day_of_week) => match day_of_week.as_i64() {
            Some(day_of_week) => day_of_week - 1,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };

    let date_lowercase = unit.to_lowercase();
    let truncation_unit = date_lowercase.as_str();

    let mut truncated_date;
    match truncation_unit {
        "millennium" => {
            let year = year / 1000 * 1000;
            truncated_date = match NaiveDate::from_ymd_opt(year, 1, 1) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        "century" => {
            let year = year / 100 * 100;
            truncated_date = match NaiveDate::from_ymd_opt(year, 1, 1) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        "decade" => {
            let year = year / 10 * 10;
            truncated_date = match NaiveDate::from_ymd_opt(year, 1, 1) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        "year" => {
            truncated_date = match NaiveDate::from_ymd_opt(year, 1, 1) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        "quarter" => {
            let month = month / 3 * 3 + 1;
            truncated_date = match NaiveDate::from_ymd_opt(year, month, 1) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        "month" => {
            truncated_date = match NaiveDate::from_ymd_opt(year, month, 1) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        "week" => {
            let weekday = date.weekday();
            let days_to_subtract = match weekday {
                Weekday::Mon => 0,
                Weekday::Tue => 1,
                Weekday::Wed => 2,
                Weekday::Thu => 3,
                Weekday::Fri => 4,
                Weekday::Sat => 5,
                Weekday::Sun => 6,
            };

            truncated_date = date - chrono::Duration::days(days_to_subtract as i64);
        }
        "day" => {
            truncated_date = match NaiveDate::from_ymd_opt(year, month, day) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        "weekyear" => {
            // First day of the first week of the year
            let date_string = format!("{}-1-1", year);
            let date = match NaiveDate::parse_from_str(&date_string, "%Y-%W-%u") {
                Ok(date) => date,
                Err(_) => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            };
            truncated_date = date;
        }
        "hour" | "minute" | "second" | "millisecond" | "microsecond" => {
            truncated_date = match NaiveDate::from_ymd_opt(year, month, day) {
                Some(date) => date,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        _ => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
            });
        }
    };

    truncated_date = match NaiveDate::from_ymd_opt(
        truncated_date.year() + years_to_add as i32,
        truncated_date.month() + months_to_add as u32,
        truncated_date.day() + days_to_add as u32,
    ) {
        Some(date) => date,
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
            })
        }
    };
    if truncation_unit == "week" || truncation_unit == "weekyear" {
        truncated_date += chrono::Duration::days(days_of_week_to_add);
    }

    Ok(truncated_date)
}

async fn truncate_local_time(
    unit: String,
    time: NaiveTime,
) -> Result<NaiveTime, FunctionEvaluationError> {
    let hour = time.hour();
    let minute = time.minute();
    let second = time.second();
    let nanosecond = time.nanosecond();

    let time_lowercase = unit.to_lowercase();
    let truncation_unit = time_lowercase.as_str();
    match truncation_unit {
        "millennium" => Ok(*temporal_constants::MIDNIGHT_NAIVE_TIME),
        "century" => Ok(*temporal_constants::MIDNIGHT_NAIVE_TIME),
        "decade" => Ok(*temporal_constants::MIDNIGHT_NAIVE_TIME),
        "year" => Ok(*temporal_constants::MIDNIGHT_NAIVE_TIME),
        "month" => Ok(*temporal_constants::MIDNIGHT_NAIVE_TIME),
        "week" => Ok(*temporal_constants::MIDNIGHT_NAIVE_TIME),
        "quarter" => Ok(*temporal_constants::MIDNIGHT_NAIVE_TIME),
        "weekyear" => Ok(*temporal_constants::MIDNIGHT_NAIVE_TIME),
        "day" => Ok(*temporal_constants::MIDNIGHT_NAIVE_TIME),
        "hour" => Ok(match NaiveTime::from_hms_opt(hour, 0, 0) {
            Some(time) => time,
            None => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                })
            }
        }),
        "minute" => Ok(match NaiveTime::from_hms_opt(hour, minute, 0) {
            Some(time) => time,
            None => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                })
            }
        }),
        "second" => Ok(match NaiveTime::from_hms_opt(hour, minute, second) {
            Some(time) => time,
            None => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                })
            }
        }),
        "millisecond" => {
            let divisor = 10u32.pow(6);

            let truncated_nanos = (nanosecond / divisor) * divisor;

            Ok(
                match NaiveTime::from_hms_nano_opt(hour, minute, second, truncated_nanos) {
                    Some(time) => time,
                    None => {
                        return Err(FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR
                                .to_string(),
                        })
                    }
                },
            )
        }
        "microsecond" => {
            let divisor = 10u32.pow(3);
            let truncated_nanos = (nanosecond / divisor) * divisor;

            Ok(
                match NaiveTime::from_hms_nano_opt(hour, minute, second, truncated_nanos) {
                    Some(time) => time,
                    None => {
                        return Err(FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR
                                .to_string(),
                        })
                    }
                },
            )
        }
        _ => Err(FunctionEvaluationError::InvalidType {
            expected: "Valid truncation unit".to_string(),
        }),
    }
}

async fn truncate_local_time_with_map(
    unit: String,
    time: NaiveTime,
    map: BTreeMap<String, VariableValue>,
) -> Result<NaiveTime, FunctionEvaluationError> {
    let hour = time.hour();
    let minute = time.minute();
    let second = time.second();
    let nanosecond = time.nanosecond();

    let hour_to_add = match map.get("hour") {
        Some(hour) => match hour.as_i64() {
            Some(hour) => hour,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let minute_to_add = match map.get("minute") {
        Some(minute) => match minute.as_i64() {
            Some(minute) => minute,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let second_to_add = match map.get("second") {
        Some(second) => match second.as_i64() {
            Some(second) => second,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let milliseconds_to_add = match map.get("millisecond") {
        Some(millisecond) => match millisecond.as_i64() {
            Some(millisecond) => millisecond,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let microseconds_to_add = match map.get("microsecond") {
        Some(microsecond) => match microsecond.as_i64() {
            Some(microsecond) => microsecond,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };
    let nanoseconds_to_add = match map.get("nanosecond") {
        Some(nanosecond) => match nanosecond.as_i64() {
            Some(nanosecond) => nanosecond,
            None => return Err(FunctionEvaluationError::OverflowError),
        },
        None => 0,
    };

    let time_lowercase = unit.to_lowercase();
    let truncation_unit = time_lowercase.as_str();

    let mut truncated_time;
    match truncation_unit {
        "millennium" => {
            truncated_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
        }
        "century" => {
            truncated_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
        }
        "decade" => {
            truncated_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
        }
        "weekyear" => {
            return Ok(*temporal_constants::MIDNIGHT_NAIVE_TIME);
        }
        "year" => {
            truncated_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
        }
        "month" => {
            truncated_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
        }
        "week" => {
            truncated_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
        }
        "quarter" => {
            truncated_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
        }
        "day" => {
            truncated_time = *temporal_constants::MIDNIGHT_NAIVE_TIME;
        }
        "hour" => {
            truncated_time = match NaiveTime::from_hms_opt(hour, 0, 0) {
                Some(time) => time,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        "minute" => {
            truncated_time = match NaiveTime::from_hms_opt(hour, minute, 0) {
                Some(time) => time,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        "second" => {
            truncated_time = match NaiveTime::from_hms_opt(hour, minute, second) {
                Some(time) => time,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        "millisecond" => {
            let divisor = 10u32.pow(6);

            let truncated_nanos = (nanosecond / divisor) * divisor;

            truncated_time =
                match NaiveTime::from_hms_nano_opt(hour, minute, second, truncated_nanos) {
                    Some(time) => time,
                    None => {
                        return Err(FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR
                                .to_string(),
                        })
                    }
                };
        }
        "microsecond" => {
            let divisor = 10u32.pow(3);
            let truncated_nanos = (nanosecond / divisor) * divisor;

            truncated_time =
                match NaiveTime::from_hms_nano_opt(hour, minute, second, truncated_nanos) {
                    Some(time) => time,
                    None => {
                        return Err(FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR
                                .to_string(),
                        })
                    }
                };
        }
        "timezone" => {
            // don't truncate timezone
            truncated_time = match NaiveTime::from_hms_nano_opt(hour, minute, second, nanosecond) {
                Some(time) => time,
                None => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                    })
                }
            };
        }
        _ => {
            return Err(FunctionEvaluationError::InvalidType {
                expected: "Valid truncation unit".to_string(),
            });
        }
    }

    truncated_time = match NaiveTime::from_hms_nano_opt(
        truncated_time.hour() + hour_to_add as u32,
        truncated_time.minute() + minute_to_add as u32,
        truncated_time.second() + second_to_add as u32,
        truncated_time.nanosecond()
            + milliseconds_to_add as u32 * 1_000_000
            + microseconds_to_add as u32 * 1_000
            + nanoseconds_to_add as u32,
    ) {
        Some(time) => time,
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
            })
        }
    };
    Ok(truncated_time)
}

fn handle_iana_timezone(input: &str) -> Option<Tz> {
    let timezone_str = input.replace(' ', "_").replace(['[', ']'], "");
    if timezone_str == "Z" || timezone_str.contains('+') || timezone_str.contains('-') {
        return Some(Tz::UTC);
    }
    let tz: Tz = match timezone_str.parse() {
        Ok(tz) => tz,
        Err(_) => return None,
    };
    Some(tz)
}

async fn parse_date_string(date_str: &str) -> Result<NaiveDate, FunctionEvaluationError> {
    let temp = date_str_formatter(date_str).await?;
    let input = temp.as_str();

    // YYYYMMDD
    if let Ok(date) = NaiveDate::parse_from_str(input, "%Y%m%d") {
        return Ok(date);
    }
    if let Some(_week_index) = input.find('W') {
        if let Ok(date) = NaiveDate::parse_from_str(input, "%YW%U%u") {
            return Ok(date);
        }

        return Err(FunctionEvaluationError::InvalidFormat {
            expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
        });
    }

    // YYYYDDD
    if input.len() == 7 {
        // Number of days since the day 1 of that year
        if let Ok(date) = NaiveDate::parse_from_str(input, "%Y%j") {
            return Ok(date);
        }
    }
    Err(FunctionEvaluationError::InvalidFormat {
        expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
    })
}

async fn date_str_formatter(input: &str) -> Result<String, FunctionEvaluationError> {
    // Removes dash
    let temp = input.replace('-', "");
    let date_str = temp.as_str();

    // NaiveDate parser does not support date in the format of YYYYMM
    // Changing this to YYYYMM01 (as suggested by Neo4j)
    if date_str.len() == 6 && !date_str.contains('Q') && !date_str.contains('W') {
        return Ok(format!("{}01", date_str));
    }

    // YYYYWwwD, YYYYWww
    if let Some(week_index) = date_str.find('W') {
        // Extract the two characters immediately after 'W'
        if let Some(week_digits) = date_str.get((week_index + 1)..(week_index + 3)) {
            // Attempt to parse the extracted characters as an i32 (week number)
            if let Ok(mut parsed_week_number) = week_digits.parse::<i32>() {
                // Subtract 1 from the week number
                parsed_week_number -= 1;
                if date_str.len() == 7 {
                    return Ok(format!(
                        "{}W{}{}1",
                        &date_str[..week_index],
                        parsed_week_number,
                        &date_str[week_index + 3..]
                    ));
                }
                // Create the modified string
                return Ok(format!(
                    "{}W{}{}",
                    &date_str[..week_index],
                    parsed_week_number,
                    &date_str[week_index + 3..]
                ));
            }
        }
    }

    if date_str.contains('Q') {
        // Attempt to parse as a quarter-based date
        let date = parse_quarter_date(input).await?;
        return Ok(date);
    }

    // YYYY
    if date_str.len() == 4 {
        let formatted_input = format!("{}0101", date_str);
        return Ok(formatted_input);
    }

    Ok(date_str.to_string())
}

async fn parse_quarter_date(date_str: &str) -> Result<String, FunctionEvaluationError> {
    let parts: Vec<&str> = date_str.split('-').collect();
    if parts.len() < 2 {
        return Err(FunctionEvaluationError::InvalidFormat {
            expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
        });
    }

    let year = match parts[0].parse::<i32>() {
        Ok(y) => y,
        Err(_) => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
            })
        }
    };
    let quarter = match parts[1].chars().nth(1) {
        Some(q) => {
            if q.is_ascii_digit() {
                match q.to_digit(10) {
                    Some(q) => q as i32,
                    None => {
                        return Err(FunctionEvaluationError::InvalidFormat {
                            expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                        })
                    }
                }
            } else {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
                });
            }
        }
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
            })
        }
    };

    if !(1..=4).contains(&quarter) {
        return Err(FunctionEvaluationError::InvalidFormat {
            expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
        });
    }

    let day_of_quarter = match parts[2].parse::<i32>() {
        Ok(d) => d,
        Err(_) => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
            })
        }
    };

    if !(1..=92).contains(&day_of_quarter) {
        return Err(FunctionEvaluationError::InvalidFormat {
            expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
        });
    }

    let month = match quarter {
        1 => 1,
        2 => 4,
        3 => 7,
        4 => 10,
        _ => unreachable!(),
    };

    let temp_date = match NaiveDate::from_ymd_opt(year, month, 1) {
        Some(date) => date,
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
            })
        }
    };

    let date = match temp_date.checked_add_days(Days::new(day_of_quarter as u64 - 1)) {
        Some(d) => d,
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_DATE_FORMAT_ERROR.to_string(),
            })
        }
    };

    let date_format = date.to_string();
    Ok(date_format)
}

async fn parse_local_time_input(input: &str) -> Result<chrono::NaiveTime, FunctionEvaluationError> {
    let mut input_string = input.to_string().replace(':', "");

    let contains_fractional_seconds = input_string.contains('.');
    if contains_fractional_seconds {
        let time = match NaiveTime::parse_from_str(&input_string, "%H%M%S%.f") {
            Ok(t) => t,
            Err(_) => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                })
            }
        };
        return Ok(time);
    }

    // HHMM case; Append 01 to the end
    if input_string.len() == 4 {
        input_string.push_str("00");
    } else if input_string.len() == 2 {
        input_string.push_str("0000");
    }
    let time = match NaiveTime::parse_from_str(&input_string, "%H%M%S") {
        Ok(t) => t,
        Err(_) => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
            })
        }
    };
    Ok(time)
}

async fn parse_local_date_time_input(
    input: &str,
) -> Result<NaiveDateTime, FunctionEvaluationError> {
    let input_string = input.to_string().replace([':', '-'], "");

    let date_string = match input_string.split('T').next() {
        Some(date) => date,
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR.to_string(),
            })
        }
    };

    let time_string = match input_string.split('T').last() {
        Some(time) => time,
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_LOCAL_DATETIME_FORMAT_ERROR.to_string(),
            })
        }
    };

    let naive_date = parse_date_string(date_string).await?;

    let naive_time = parse_local_time_input(time_string).await?;

    Ok(NaiveDateTime::new(naive_date, naive_time))
}

async fn parse_zoned_time_input(input: &str) -> Result<ZonedTime, FunctionEvaluationError> {
    let input_string = input.to_string().replace(':', "");
    let contains_frac = input.contains('.');
    let is_utc = input.contains('Z');

    let time_string = match input_string.split(['Z', '+', '-']).next() {
        Some(time) => time,
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
            })
        }
    };

    let timezone = match extract_timezone(&input_string).await {
        Some(tz) => tz,
        None => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
            })
        }
    };

    if is_utc {
        let offset = *temporal_constants::UTC_FIXED_OFFSET;
        if contains_frac {
            let naive_time = match NaiveTime::parse_from_str(time_string, "%H%M%S%.f") {
                Ok(t) => t,
                Err(_) => {
                    return Err(FunctionEvaluationError::InvalidFormat {
                        expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                    })
                }
            };
            return Ok(ZonedTime::new(naive_time, offset));
        }
        let naive_time = match NaiveTime::parse_from_str(time_string, "%H%M%S") {
            Ok(t) => t,
            Err(_) => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                })
            }
        };
        return Ok(ZonedTime::new(naive_time, offset));
    }

    let naive_time = parse_local_time_input(time_string).await?;

    let offset = match FixedOffset::from_str(timezone) {
        Ok(o) => o,
        Err(_) => {
            return Err(FunctionEvaluationError::InvalidFormat {
                expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
            })
        }
    };

    if contains_frac {
        let naive_time = match NaiveTime::parse_from_str(time_string, "%H%M%S%.f") {
            Ok(t) => t,
            Err(_) => {
                return Err(FunctionEvaluationError::InvalidFormat {
                    expected: temporal_constants::INVALID_LOCAL_TIME_FORMAT_ERROR.to_string(),
                })
            }
        };
        let zoned_time = ZonedTime::new(naive_time, offset);
        return Ok(zoned_time);
    }

    let zoned_time = ZonedTime::new(naive_time, offset);
    Ok(zoned_time)
}

async fn extract_timezone(input: &str) -> Option<&str> {
    let re = match Regex::new(r"(\d{2,6}(\.\d{1,9})?)(([+-].*)|Z|(\[.*?\])?)?") {
        Ok(re) => re,
        Err(_) => return None,
    };

    if let Some(captures) = re.captures(input) {
        captures.get(3).map(|m| m.as_str())
    } else {
        None
    }
}
