use crate::evaluation::temporal_constants;
use crate::evaluation::variable_value::zoned_datetime::ZonedDateTime;
use crate::evaluation::variable_value::zoned_time::ZonedTime;
use crate::evaluation::variable_value::VariableValue;
use async_trait::async_trait;
use chrono::format::Fixed;
use drasi_query_ast::ast;

use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext};
use chrono::{
    Datelike, Days, Duration, FixedOffset, LocalResult, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Timelike, Weekday, prelude::*
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
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() > 1 {
            return Err(EvaluationError::InvalidArgumentCount("date".to_string()));
        }
        if args.len() == 0 {
            // current date
            let local = Local::now();
            let date = local.date_naive();
            return Ok(VariableValue::Date(date));
        }
        match &args[0] {
            VariableValue::String(s) => {
                let date_str = s.as_str();
                let date = parse_date_string(date_str).await;
                match date {
                    Ok(result_date) => Ok(VariableValue::Date(result_date)),
                    _ => Err(EvaluationError::InvalidType),
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
                    return Err(EvaluationError::InvalidType);
                }
                if o.get("timezone").is_some() {
                    let tz = match o.get("timezone") {
                        Some(tz) => {
                            let tz_str = tz.as_str().unwrap();
                            let tz: Tz = match  tz_str.parse() {
                                Ok(tz) => tz,
                                Err(_) => return Err(EvaluationError::InvalidType),
                            };
                            tz
                        }
                        None => return Err(EvaluationError::InvalidType)
                    };
                    let local: chrono::DateTime<Tz> = Local::now().with_timezone(&tz);
                    return Ok(VariableValue::Date(local.date_naive()));
                }
                let result = create_date_from_componet(o.clone()).await;
                match result {
                    Some(date) => Ok(date),
                    None => Err(EvaluationError::InvalidType),
                }
            }
            VariableValue::Date(d) => Ok(VariableValue::Date(*d)),
            _ => Err(EvaluationError::InvalidType),
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
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() > 1 {
            return Err(EvaluationError::InvalidArgumentCount(
                "localtime".to_string(),
            ));
        }
        if args.len() == 0 {
            let local = Local::now().time();
            return Ok(VariableValue::LocalTime(local));
        }
        match &args[0] {
            VariableValue::String(s) => {
                let time_str = s.as_str();
                let time = parse_local_time_input(time_str).await;
                match time {
                    Ok(result_time) => Ok(VariableValue::LocalTime(result_time)),
                    _ => Err(EvaluationError::InvalidType),
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
                    return Err(EvaluationError::InvalidType);
                }
                let result = create_time_from_componet(o.clone()).await;
                match result {
                    Some(time) => Ok(time),
                    None => Err(EvaluationError::InvalidType),
                }
            }
            VariableValue::LocalTime(t) => Ok(VariableValue::LocalTime(*t)),
            _ => Err(EvaluationError::InvalidType),
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
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() > 1 {
            return Err(EvaluationError::InvalidArgumentCount(
                "localdatetime".to_string(),
            ));
        }
        if args.len() == 0 {
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
                    _ => Err(EvaluationError::InvalidType),
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
                    "timezone"
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();

                let invalid_keys: Vec<_> =
                    o.keys().filter(|&key| !valid_keys.contains(key)).collect();
                if !invalid_keys.is_empty() {
                    error!("Invalid keys in the localdatetime object");
                    return Err(EvaluationError::InvalidType);
                }
                if o.get("timezone").is_some() {
                    // retrieve the naivedatetime from the timezone
                    let tz = match o.get("timezone") {
                        Some(tz) => {
                            let tz_str = tz.as_str().unwrap();
                            let tz: Tz = match tz_str.parse() {
                                Ok(tz) => tz,
                                Err(_) => return Err(EvaluationError::InvalidType),
                            };
                            tz
                        }
                        None => return Err(EvaluationError::InvalidType),
                    };

                    let local: chrono::DateTime<Tz> = Local::now().with_timezone(&tz);
                    return Ok(VariableValue::LocalDateTime(local.naive_local()));
                }

                let date_result = match create_date_from_componet(o.clone()).await {
                    Some(date) => date,
                    None => return Err(EvaluationError::InvalidType),
                };
                let time_result = match create_time_from_componet(o.clone()).await {
                    Some(time) => time,
                    None => return Err(EvaluationError::InvalidType),
                };
                let datetime = date_result
                    .as_date()
                    .unwrap()
                    .and_time(time_result.as_local_time().unwrap());
                Ok(VariableValue::LocalDateTime(datetime))
            }
            VariableValue::LocalDateTime(dt) => Ok(VariableValue::LocalDateTime(*dt)),
            _ => Err(EvaluationError::InvalidType),
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
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() > 1 {
            return Err(EvaluationError::InvalidArgumentCount("time".to_string()));
        }
        if args.len() == 0 {
            let local = Local::now().time();
            let timezone = FixedOffset::east_opt(0).unwrap();
            return Ok(VariableValue::ZonedTime(ZonedTime::new(local, timezone)));
        }
        match &args[0] {
            VariableValue::String(s) => {
                let time_str = s.as_str();
                let time = parse_zoned_time_input(time_str).await;
                match time {
                    Ok(result_time) => Ok(VariableValue::ZonedTime(result_time)),
                    _ => Err(EvaluationError::InvalidType),
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
                    return Err(EvaluationError::InvalidType);
                }
               

                let timezone = match o.get("timezone") {
                    Some(tz) => {
                        let tz_str = match tz {
                            VariableValue::String(s) => s.as_str(),
                            _ => "UTC",
                        };
                        match handle_iana_timezone(tz_str).await {
                            Some(tz) => tz,
                            None => return Err(EvaluationError::ParseError),
                        }
                    }
                    None => Tz::UTC,
                };

                // if only the timezone is specified, return the current time in that timezone
                if o.get("timezone").is_some() && o.len() == 1 {
                    let local: chrono::DateTime<Tz> = Local::now().with_timezone(&timezone);
                    let fixed_offset = local.offset().fix();
                    return Ok(VariableValue::ZonedTime(ZonedTime::new(local.time(), fixed_offset)));    
                }

                let result = create_time_from_componet(o.clone()).await;
                match result {
                    Some(time) => {
                        let dummy_date = *temporal_constants::EPOCH_NAIVE_DATE;
                        let local_time = match time.as_local_time() {
                            Some(time) => time,
                            None => return Err(EvaluationError::InvalidType),
                        };
                        let datetime = dummy_date.and_time(local_time);
                        let zoned_datetime = match timezone.from_local_datetime(&datetime) {
                            LocalResult::Single(zoned_datetime) => zoned_datetime.fixed_offset(),
                            _ => return Err(EvaluationError::InvalidType),
                        };
                        let zoned_time = zoned_datetime.time();
                        let offset = zoned_datetime.offset();
                        let default = VariableValue::String("UTC".to_string());
                        let timezone_str = match o.get("timezone").unwrap_or(&default) {
                            VariableValue::String(s) => s.as_str(),
                            _ => "UTC",
                        };
                        if timezone_str.contains('+') || timezone_str.contains('-') {
                            let offset = match FixedOffset::from_str(timezone_str) {
                                Ok(offset) => offset,
                                Err(_) => return Err(EvaluationError::ParseError),
                            };
                            return Ok(VariableValue::ZonedTime(ZonedTime::new(
                                zoned_time, offset,
                            )));
                        }
                        return Ok(VariableValue::ZonedTime(ZonedTime::new(
                            zoned_time, *offset,
                        )));
                    }
                    None => Err(EvaluationError::InvalidType),
                }
            }
            VariableValue::ZonedTime(t) => Ok(VariableValue::ZonedTime(*t)),
            _ => Err(EvaluationError::InvalidType),
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
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() > 1 {
            return Err(EvaluationError::InvalidArgumentCount(
                "datetime".to_string(),
            ));
        }

        if args.len() == 0 {
            let local = Local::now();
            let datetime = local.with_timezone(&FixedOffset::east_opt(0).unwrap());
            return Ok(VariableValue::ZonedDateTime(ZonedDateTime::new(datetime, None)));
        }
        match &args[0] {
            VariableValue::String(s) => {
                let time_str = s.as_str();
                let time = parse_zoned_date_time_input(time_str).await;
                match time {
                    Ok(result_time) => Ok(VariableValue::ZonedDateTime(result_time)),
                    _ => Err(EvaluationError::InvalidType),
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
                    return Err(EvaluationError::InvalidType);
                }
                if o.contains_key("epochSeconds") || o.contains_key("epochMillis") {
                    let result = match create_date_time_from_epoch(o.clone()).await {
                        Some(datetime) => datetime,
                        None => return Err(EvaluationError::InvalidType),
                    };

                    return Ok(result);
                }
                let timezone = match o.get("timezone") {
                    Some(tz) => {
                        let tz_str = tz.as_str().unwrap();
                        match handle_iana_timezone(tz_str).await {
                            Some(tz) => tz,
                            None => return Err(EvaluationError::ParseError),
                        }
                    }
                    None => Tz::UTC,
                };

                if o.get("timezone").is_some() && o.len() == 1 {
                    let local: chrono::DateTime<Tz> = Local::now().with_timezone(&timezone);
                    let zoned_time = match timezone.from_local_datetime(&local.naive_local()) {
                        LocalResult::Single(zoned_time) => zoned_time.fixed_offset(),
                        _ => return Err(EvaluationError::InvalidType),
                    };
                    return Ok(VariableValue::ZonedDateTime(ZonedDateTime::new(zoned_time, None)));
                }
                let naive_date = match create_date_from_componet(o.clone()).await {
                    Some(date) => date.as_date().unwrap(),
                    None => return Err(EvaluationError::InvalidType),
                };
                let result = create_time_from_componet(o.clone()).await;
                match result {
                    Some(time) => {
                        let local_time = match time.as_local_time() {
                            Some(time) => time,
                            None => return Err(EvaluationError::InvalidType),
                        };
                        let datetime = naive_date.and_time(local_time);
                        let timezone_str = match o.get("timezone") {
                            Some(VariableValue::String(s)) => s.as_str(),
                            _ => "UTC",
                        };
                        if timezone_str.contains('+') || timezone_str.contains('-') {
                            let offset = match FixedOffset::from_str(timezone_str) {
                                Ok(offset) => offset,
                                Err(_) => return Err(EvaluationError::InvalidType),
                            };
                            let datetime_offset = match datetime.and_local_timezone(offset) {
                                LocalResult::Single(offset) => offset.fixed_offset(),
                                _ => return Err(EvaluationError::InvalidType),
                            };
                            return Ok(VariableValue::ZonedDateTime(ZonedDateTime::new(
                                datetime_offset,
                                Some(timezone_str.to_string()),
                            )));
                        }
                        let zoned_datetime = match timezone.from_local_datetime(&datetime) {
                            LocalResult::Single(zoned_datetime) => zoned_datetime.fixed_offset(),
                            _ => return Err(EvaluationError::InvalidType),
                        };
                        return Ok(VariableValue::ZonedDateTime(ZonedDateTime::new(
                            zoned_datetime,
                            Some(timezone_str.to_string()),
                        )));
                    }
                    None => Err(EvaluationError::InvalidType),
                }
            }
            VariableValue::ZonedDateTime(t) => Ok(VariableValue::ZonedDateTime(t.clone())),
            _ => Err(EvaluationError::InvalidType),
        }
    }
}

async fn create_date_from_componet(o: BTreeMap<String, VariableValue>) -> Option<VariableValue> {
    let year = match o.get("year") {
        Some(year) => year.as_i64().unwrap(),
        None => 0,
    };
    let week = match o.get("week") {
        Some(week) => week.as_i64().unwrap(),
        None => 0,
    };
    if week > 0 {
        let day_of_week = match o.get("dayOfWeek") {
            Some(day_of_week) => day_of_week.as_i64().unwrap(),
            None => 0,
        };
        let begin_date =
            NaiveDate::from_isoywd_opt(year as i32, week as u32, Weekday::Mon).unwrap();
        let date = begin_date + Duration::days(day_of_week - 1);
        return Some(VariableValue::Date(date));
    }
    let ordinal_day = match o.get("ordinalDay") {
        Some(ordinal_day) => ordinal_day.as_i64().unwrap(),
        None => 0,
    };
    if ordinal_day > 0 {
        let date = NaiveDate::from_yo_opt(year as i32, ordinal_day as u32).unwrap();
        return Some(VariableValue::Date(date));
    }
    let quarter = match o.get("quarter") {
        Some(quarter) => quarter.as_i64().unwrap(),
        None => 0,
    };
    if quarter > 0 {
        let day_of_quarter = match o.get("dayOfQuarter") {
            Some(day_of_quarter) => day_of_quarter.as_i64().unwrap(),
            None => 0,
        };
        let month = (quarter - 1) * 3 + 1;
        let begin_date = NaiveDate::from_ymd_opt(year as i32, month as u32, 1).unwrap();
        let date = begin_date + Duration::days(day_of_quarter - 1);
        return Some(VariableValue::Date(date));
    }
    let month = match o.get("month") {
        Some(month) => month.as_i64().unwrap(),
        None => 0,
    };
    let day = match o.get("day") {
        Some(day) => day.as_i64().unwrap(),
        None => 0,
    };
    let date = NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32).unwrap();
    Some(VariableValue::Date(date))
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
        let datetime =
            NaiveDateTime::from_timestamp_opt(epoch_seconds, nanoseconds as u32).unwrap();
        let zoned_datetime = match datetime.and_local_timezone(FixedOffset::east_opt(0).unwrap()) {
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
        let zoned_datetime = match datetime.and_local_timezone(FixedOffset::east_opt(0).unwrap()) {
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

async fn create_time_from_componet(o: BTreeMap<String, VariableValue>) -> Option<VariableValue> {
    let hour = match o.get("hour") {
        Some(hour) => hour.as_i64().unwrap_or(0),
        None => 0,
    };
    let minute = match o.get("minute") {
        Some(minute) => minute.as_i64().unwrap_or(0),
        None => 0,
    };
    let second = match o.get("second") {
        Some(second) => second.as_i64().unwrap_or(0),
        None => 0,
    };
    let millisecond = match o.get("millisecond") {
        Some(millisecond) => millisecond.as_i64().unwrap_or(0),
        None => 0,
    };
    let microsecond = match o.get("microsecond") {
        Some(microsecond) => microsecond.as_i64().unwrap_or(0),
        None => 0,
    };
    let nanosecond = match o.get("nanosecond") {
        Some(nanosecond) => nanosecond.as_i64().unwrap_or(0),
        None => 0,
    };

    let mut time = NaiveTime::from_hms_opt(hour as u32, minute as u32, second as u32).unwrap();
    time = time
        + Duration::nanoseconds(nanosecond)
        + Duration::microseconds(microsecond)
        + Duration::milliseconds(millisecond);
    Some(VariableValue::LocalTime(time))
}

#[derive(Debug, PartialEq)]
pub struct Truncate {}

#[async_trait]
impl ScalarFunction for Truncate {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        if args.len() < 2 || args.len() > 3 {
            return Err(EvaluationError::InvalidArgumentCount(
                "truncate".to_string(),
            ));
        }
        match (&args[0], &args[1], &args.get(2)) {
            (VariableValue::String(s), VariableValue::Date(d), None) => {
                let truncated_date = match truncate_date(s.to_string(), *d).await {
                    Ok(date) => date,
                    Err(_) => return Err(EvaluationError::InvalidType),
                };
                Ok(VariableValue::Date(truncated_date))
            }
            (VariableValue::String(s), VariableValue::Date(d), Some(VariableValue::Object(m))) => {
                let truncated_date =
                    match truncate_date_with_map(s.to_string(), *d, m.clone()).await {
                        Ok(date) => date,
                        Err(_) => return Err(EvaluationError::InvalidType),
                    };
                Ok(VariableValue::Date(truncated_date))
            }
            (VariableValue::String(s), VariableValue::LocalTime(d), None) => {
                let truncated_time = match truncate_local_time(s.to_string(), *d).await {
                    Ok(time) => time,
                    Err(_) => return Err(EvaluationError::InvalidType),
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
                        Err(_) => return Err(EvaluationError::InvalidType),
                    };
                Ok(VariableValue::LocalTime(truncated_time))
            }
            (VariableValue::String(s), VariableValue::LocalDateTime(dt), None) => {
                let truncated_date = match truncate_date(s.to_string(), dt.date()).await {
                    Ok(date) => date,
                    Err(_) => return Err(EvaluationError::InvalidType),
                };
                println!("truncated_date: {:?}", truncated_date);
                let truncated_time = match truncate_local_time(s.to_string(), dt.time()).await {
                    Ok(time) => time,
                    Err(_) => return Err(EvaluationError::InvalidType),
                };
                println!("truncated_time: {:?}", truncated_time);
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
                        Err(_) => return Err(EvaluationError::InvalidType),
                    };
                let truncated_time =
                    match truncate_local_time_with_map(s.to_string(), dt.time(), m.clone()).await {
                        Ok(time) => time,
                        Err(_) => return Err(EvaluationError::InvalidType),
                    };
                let truncated_date_time = NaiveDateTime::new(truncated_date, truncated_time);
                Ok(VariableValue::LocalDateTime(truncated_date_time))
            }
            (VariableValue::String(s), VariableValue::ZonedTime(t), None) => {
                let naive_time = *t.time();
                let offset = *t.offset();
                let truncated_time = match truncate_local_time(s.to_string(), naive_time).await {
                    Ok(time) => time,
                    Err(_) => return Err(EvaluationError::InvalidType),
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
                    Err(_) => return Err(EvaluationError::InvalidType),
                };
                let zoned_time_result = ZonedTime::new(truncated_time, offset);
                Ok(VariableValue::ZonedTime(zoned_time_result))
            }
            (VariableValue::String(s), VariableValue::ZonedDateTime(dt), None) => {
                let datetime = *dt.datetime();
                let timezone = dt.timezone().clone();
                let naive_date = datetime.date_naive();
                let naive_time = datetime.time();
                let offset = datetime.offset();
                let truncated_time = match truncate_local_time(s.to_string(), naive_time).await {
                    Ok(time) => time,
                    Err(_) => return Err(EvaluationError::InvalidType),
                };
                let truncate_date = match truncate_date(s.to_string(), naive_date).await {
                    Ok(date) => date,
                    Err(_) => return Err(EvaluationError::InvalidType),
                };
                let truncated_naive_date_time = NaiveDateTime::new(truncate_date, truncated_time);
                let truncated_date_time =
                    match truncated_naive_date_time.and_local_timezone(*offset) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),
                    };
                let zoned_date_time = ZonedDateTime::new(truncated_date_time, timezone);
                Ok(VariableValue::ZonedDateTime(zoned_date_time))
            },
            (VariableValue::String(s), VariableValue::ZonedDateTime(dt), Some(VariableValue::Object(m))) => {
                let datetime = *dt.datetime();
                let timezone = dt.timezone().clone();
                let naive_date = datetime.date_naive();
                let naive_time = datetime.time();
                let offset = datetime.offset();
                let truncated_time = match truncate_local_time_with_map(s.to_string(), naive_time, m.clone()).await {
                    Ok(time) => time,
                    Err(_) => return Err(EvaluationError::InvalidType),
                };
                println!("truncated_time: {:?}", truncated_time);
                let truncate_date = match truncate_date_with_map(s.to_string(), naive_date, m.clone()).await {
                    Ok(date) => date,
                    Err(_) => return Err(EvaluationError::InvalidType),
                };
                println!("truncate_date: {:?}", truncate_date);
                let truncated_naive_date_time = NaiveDateTime::new(truncate_date, truncated_time);
                if m.get("timezone").is_some() {
                    let timezone = match m.get("timezone") {
                        Some(tz) => {
                            let tz_str = tz.as_str().unwrap();
                            match handle_iana_timezone(tz_str).await {
                                Some(tz) => tz,
                                None => return Err(EvaluationError::ParseError),
                            }
                        }
                        None => return Err(EvaluationError::InvalidType),
                    };
                    let datetime_tz = match timezone.from_local_datetime(&truncated_naive_date_time) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),
                    };
                    let datetime_fixed_offset = datetime_tz.fixed_offset();
                    let timezone_string = m.get("timezone").unwrap().as_str().unwrap();
                    let zoned_date_time = ZonedDateTime::new(datetime_fixed_offset, Some(timezone_string.to_string()));
                    return Ok(VariableValue::ZonedDateTime(zoned_date_time));
                }
                let truncated_date_time =
                    match truncated_naive_date_time.and_local_timezone(*offset) {
                        LocalResult::Single(dt) => dt,
                        _ => return Err(EvaluationError::InvalidType),
                    };
                let zoned_date_time = ZonedDateTime::new(truncated_date_time, timezone);
                Ok(VariableValue::ZonedDateTime(zoned_date_time))
            },
            _ => Err(EvaluationError::InvalidType),
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
        _expression: &ast::FunctionExpression,
        args: Vec<VariableValue>,
    ) -> Result<VariableValue, EvaluationError> {
        let timestamp = match self.clock {
            Clock::RealTime => context.get_realtime(),
            Clock::Statement => context.get_realtime(),
            Clock::Transaction => context.get_transaction_time(),
        };

        if args.is_empty() {
            let zdt = ZonedDateTime::from_epoch_millis(timestamp);
            return Ok(match self.result {
                ClockResult::Date => VariableValue::Date(zdt.datetime().date_naive()),
                ClockResult::LocalTime => VariableValue::LocalTime(zdt.datetime().time()),
                ClockResult::LocalDateTime => {
                    VariableValue::LocalDateTime(zdt.datetime().naive_utc())
                }
                ClockResult::ZonedTime => VariableValue::ZonedTime(ZonedTime::new(
                    zdt.datetime().time(),
                    FixedOffset::east_opt(0).unwrap(),
                )),
                ClockResult::ZonedDateTime => VariableValue::ZonedDateTime(zdt),
            });
        } else {
            let timezone_string = args[0].as_str().unwrap();
            let tz = match handle_iana_timezone(timezone_string).await {
                Some(tz) => tz,
                None => return Err(EvaluationError::ParseError),
            };
            let date_time = match tz.timestamp_millis_opt(timestamp as i64) {
                LocalResult::Single(dt) => dt.fixed_offset(),
                _ => return Err(EvaluationError::ParseError),
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

async fn truncate_date(unit: String, date: NaiveDate) -> Result<NaiveDate, EvaluationError> {
    let year = date.year();
    let month = date.month();
    let day = date.day();

    let date_lowercase = unit.to_lowercase();
    let truncation_unit = date_lowercase.as_str();
    match truncation_unit {
        "millennium" => {
            let year = year / 1000 * 1000;
            Ok(NaiveDate::from_ymd_opt(year, 1, 1).unwrap())
        }
        "century" => {
            let year = year / 100 * 100;
            Ok(NaiveDate::from_ymd_opt(year, 1, 1).unwrap())
        }
        "decade" => {
            let year = year / 10 * 10;
            Ok(NaiveDate::from_ymd_opt(year, 1, 1).unwrap())
        }
        "year" => Ok(NaiveDate::from_ymd_opt(year, 1, 1).unwrap()),
        "quarter" => {
            let month = month / 3 * 3 + 1;
            Ok(NaiveDate::from_ymd_opt(year, month, 1).unwrap())
        }
        "month" => Ok(NaiveDate::from_ymd_opt(year, month, 1).unwrap()),
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
        "day" => Ok(NaiveDate::from_ymd_opt(year, month, day).unwrap()),
        "weekyear" => {
            // First day of the first week of the year
            let date_string = format!("{}-1-1", year);
            let date = match NaiveDate::parse_from_str(&date_string, "%Y-%W-%u") {
                Ok(date) => date,
                Err(_) => return Err(EvaluationError::InvalidType),
            };
            Ok(date)
        }
        "hour" | "minute" | "second" | "millisecond" | "microsecond" => {
            Ok(NaiveDate::from_ymd_opt(year, month, day).unwrap())
        }
        _ => Err(EvaluationError::InvalidType),
    }
}

async fn truncate_date_with_map(
    unit: String,
    date: NaiveDate,
    map: BTreeMap<String, VariableValue>,
) -> Result<NaiveDate, EvaluationError> {
    let year = date.year();
    let month = date.month();
    let day = date.day();

    let years_to_add = match map.get("year") {
        Some(year) => year.as_i64().unwrap() - 1,
        None => 0,
    };
    let months_to_add = match map.get("month") {
        Some(month) => month.as_i64().unwrap() - 1,
        None => 0,
    };
    let days_to_add = match map.get("day") {
        Some(day) => day.as_i64().unwrap() - 1,
        None => 0,
    };
    let days_of_week_to_add = match map.get("dayOfWeek") {
        Some(day_of_week) => day_of_week.as_i64().unwrap() - 1,
        None => 0,
    };

    let date_lowercase = unit.to_lowercase();
    let truncation_unit = date_lowercase.as_str();

    let mut truncated_date;
    match truncation_unit {
        "millennium" => {
            let year = year / 1000 * 1000;
            truncated_date = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
        }
        "century" => {
            let year = year / 100 * 100;
            truncated_date = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
        }
        "decade" => {
            let year = year / 10 * 10;
            truncated_date = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
        }
        "year" => {
            truncated_date = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
        }
        "quarter" => {
            let month = month / 3 * 3 + 1;
            truncated_date = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
        }
        "month" => {
            truncated_date = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
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
            truncated_date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
        }
        "weekyear" => {
            // First day of the first week of the year
            let date_string = format!("{}-1-1", year);
            let date = match NaiveDate::parse_from_str(&date_string, "%Y-%W-%u") {
                Ok(date) => date,
                Err(_) => return Err(EvaluationError::InvalidType),
            };
            truncated_date = date;
        }
        "hour" | "minute" | "second" | "millisecond" | "microsecond" => {
            truncated_date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
        }
        _ => {
            return Err(EvaluationError::InvalidType);
        }
    };

    truncated_date = NaiveDate::from_ymd_opt(
        truncated_date.year() + years_to_add as i32,
        truncated_date.month() + months_to_add as u32,
        truncated_date.day() + days_to_add as u32,
    )
    .unwrap();
    if truncation_unit == "week" || truncation_unit == "weekyear" {
        truncated_date += chrono::Duration::days(days_of_week_to_add);
    }

    Ok(truncated_date)
}

async fn truncate_local_time(unit: String, time: NaiveTime) -> Result<NaiveTime, EvaluationError> {
    let hour = time.hour();
    let minute = time.minute();
    let second = time.second();
    let nanosecond = time.nanosecond();

    let time_lowercase = unit.to_lowercase();
    let truncation_unit = time_lowercase.as_str();

    println!("truncation_unit: {:?}", truncation_unit);
    match truncation_unit {
        "millennium" => Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "century" => Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "decade" => Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "year" => Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "month" => Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "week" => Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "quarter" => Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "weekyear" => Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "day" => Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "hour" => Ok(NaiveTime::from_hms_opt(hour, 0, 0).unwrap()),
        "minute" => Ok(NaiveTime::from_hms_opt(hour, minute, 0).unwrap()),
        "second" => Ok(NaiveTime::from_hms_opt(hour, minute, second).unwrap()),
        "millisecond" => {
            let divisor = 10u32.pow(6);

            let truncated_nanos = (nanosecond / divisor) * divisor;

            Ok(NaiveTime::from_hms_nano_opt(hour, minute, second, truncated_nanos).unwrap())
        }
        "microsecond" => {
            let divisor = 10u32.pow(3);
            let truncated_nanos = (nanosecond / divisor) * divisor;

            Ok(NaiveTime::from_hms_nano_opt(hour, minute, second, truncated_nanos).unwrap())
        }
        _ => Err(EvaluationError::InvalidType),
    }
}

async fn truncate_local_time_with_map(
    unit: String,
    time: NaiveTime,
    map: BTreeMap<String, VariableValue>,
) -> Result<NaiveTime, EvaluationError> {
    let hour = time.hour();
    let minute = time.minute();
    let second = time.second();
    let nanosecond = time.nanosecond();

    let hour_to_add = match map.get("hour") {
        Some(hour) => hour.as_i64().unwrap(),
        None => 0,
    };
    let minute_to_add = match map.get("minute") {
        Some(minute) => minute.as_i64().unwrap(),
        None => 0,
    };
    let second_to_add = match map.get("second") {
        Some(second) => second.as_i64().unwrap(),
        None => 0,
    };
    let milliseconds_to_add = match map.get("millisecond") {
        Some(millisecond) => millisecond.as_i64().unwrap(),
        None => 0,
    };
    let microseconds_to_add = match map.get("microsecond") {
        Some(microsecond) => microsecond.as_i64().unwrap(),
        None => 0,
    };
    let nanoseconds_to_add = match map.get("nanosecond") {
        Some(nanosecond) => nanosecond.as_i64().unwrap(),
        None => 0,
    };

    let time_lowercase = unit.to_lowercase();
    let truncation_unit = time_lowercase.as_str();

    let mut truncated_time;
    match truncation_unit {
        "millennium" => {
            truncated_time = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        }
        "century" => {
            truncated_time = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        }
        "decade" => {
            truncated_time = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        }
        "weekyear" => {
            return Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        }
        "year" => {
            truncated_time = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        }
        "month" => {
            truncated_time = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        }
        "week" => {
            truncated_time = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        }
        "quarter" => {
            truncated_time = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        }
        "day" => {
            truncated_time = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        }
        "hour" => {
            truncated_time = NaiveTime::from_hms_opt(hour, 0, 0).unwrap();
        }
        "minute" => {
            truncated_time = NaiveTime::from_hms_opt(hour, minute, 0).unwrap();
        }
        "second" => {
            truncated_time = NaiveTime::from_hms_opt(hour, minute, second).unwrap();
        }
        "millisecond" => {
            let divisor = 10u32.pow(6);

            let truncated_nanos = (nanosecond / divisor) * divisor;

            truncated_time =
                NaiveTime::from_hms_nano_opt(hour, minute, second, truncated_nanos).unwrap();
        }
        "microsecond" => {
            let divisor = 10u32.pow(3);
            let truncated_nanos = (nanosecond / divisor) * divisor;

            truncated_time =
                NaiveTime::from_hms_nano_opt(hour, minute, second, truncated_nanos).unwrap();
        }
        "timezone" => {
            // don't truncate timezone
            truncated_time = NaiveTime::from_hms_nano_opt(hour, minute, second, nanosecond).unwrap();
        }
        _ => {
            return Err(EvaluationError::InvalidType);
        }
    }

    truncated_time = NaiveTime::from_hms_nano_opt(
        truncated_time.hour() + hour_to_add as u32,
        truncated_time.minute() + minute_to_add as u32,
        truncated_time.second() + second_to_add as u32,
        truncated_time.nanosecond()
            + milliseconds_to_add as u32 * 1_000_000
            + microseconds_to_add as u32 * 1_000
            + nanoseconds_to_add as u32,
    )
    .unwrap();
    Ok(truncated_time)
}

async fn handle_iana_timezone(input: &str) -> Option<Tz> {
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

async fn parse_date_string(date_str: &str) -> Result<NaiveDate, EvaluationError> {
    let temp = date_str_formatter(date_str).await;
    let input = temp.as_str();

    // YYYYMMDD
    if let Ok(date) = NaiveDate::parse_from_str(input, "%Y%m%d") {
        return Ok(date);
    }
    if let Some(_week_index) = input.find('W') {
        if let Ok(date) = NaiveDate::parse_from_str(input, "%YW%U%u") {
            return Ok(date);
        }

        return Err(EvaluationError::ParseError);
    }

    // YYYYDDD
    if input.len() == 7 {
        // Number of days since the day 1 of that year
        if let Ok(date) = NaiveDate::parse_from_str(input, "%Y%j") {
            return Ok(date);
        }
    }
    Err(EvaluationError::ParseError)
}

async fn date_str_formatter(input: &str) -> String {
    // Removes dash
    let temp = input.replace('-', "");
    let date_str = temp.as_str();

    // NaiveDate parser does not support date in the format of YYYYMM
    // Changing this to YYYYMM01 (as suggested by Neo4j)
    if date_str.len() == 6 && !date_str.contains('Q') && !date_str.contains('W') {
        return format!("{}01", date_str);
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
                    return format!(
                        "{}W{}{}1",
                        &date_str[..week_index],
                        parsed_week_number,
                        &date_str[week_index + 3..]
                    );
                }
                // Create the modified string
                return format!(
                    "{}W{}{}",
                    &date_str[..week_index],
                    parsed_week_number,
                    &date_str[week_index + 3..]
                );
            }
        }
    }

    if date_str.contains('Q') {
        // Attempt to parse as a quarter-based date
        if let Ok(date) = parse_quarter_date(input).await {
            return date.replace('-', "");
        }
    }

    // YYYY
    if date_str.len() == 4 {
        let formatted_input = format!("{}0101", date_str);
        return formatted_input;
    }

    date_str.to_string()
}

async fn parse_quarter_date(date_str: &str) -> Result<String, EvaluationError> {
    let parts: Vec<&str> = date_str.split('-').collect();
    if parts.len() < 2 {
        return Err(EvaluationError::ParseError);
    }

    let year = match parts[0].parse::<i32>() {
        Ok(y) => y,
        Err(_) => return Err(EvaluationError::ParseError),
    };
    let quarter = match parts[1].chars().nth(1) {
        Some(q) => {
            if q.is_ascii_digit() {
                match q.to_digit(10) {
                    Some(q) => q as i32,
                    None => return Err(EvaluationError::ParseError),
                }
            } else {
                return Err(EvaluationError::ParseError);
            }
        }
        None => return Err(EvaluationError::ParseError),
    };

    if !(1..=4).contains(&quarter) {
        return Err(EvaluationError::ParseError);
    }

    let day_of_quarter = match parts[2].parse::<i32>() {
        Ok(d) => d,
        Err(_) => return Err(EvaluationError::ParseError),
    };

    if !(1..=92).contains(&day_of_quarter) {
        return Err(EvaluationError::ParseError);
    }

    let month = match quarter {
        1 => 1,
        2 => 4,
        3 => 7,
        4 => 10,
        _ => unreachable!(),
    };

    let temp_date = NaiveDate::from_ymd_opt(year, month, 1).unwrap();

    let date = match temp_date.checked_add_days(Days::new(day_of_quarter as u64 - 1)) {
        Some(d) => d,
        None => return Err(EvaluationError::OutOfRange),
    };

    let date_format = date.to_string();
    Ok(date_format)
}

async fn parse_local_time_input(input: &str) -> Result<chrono::NaiveTime, EvaluationError> {
    let mut input_string = input.to_string().replace(':', "");

    let contains_fractional_seconds = input_string.contains('.');
    if contains_fractional_seconds {
        let time = match NaiveTime::parse_from_str(&input_string, "%H%M%S%.f") {
            Ok(t) => t,
            Err(_) => return Err(EvaluationError::ParseError),
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
        Err(_) => return Err(EvaluationError::ParseError),
    };
    Ok(time)
}

async fn parse_local_date_time_input(input: &str) -> Result<NaiveDateTime, EvaluationError> {
    let input_string = input.to_string().replace([':', '-'], "");

    let date_string = match input_string.split('T').next() {
        Some(date) => date,
        None => return Err(EvaluationError::ParseError),
    };

    let time_string = match input_string.split('T').last() {
        Some(time) => time,
        None => return Err(EvaluationError::ParseError),
    };

    let naive_date;
    let naive_time;

    if let Ok(date) = parse_date_string(date_string).await {
        naive_date = date;
    } else {
        println!("Failed to parse the date string");
        return Err(EvaluationError::ParseError);
    }

    if let Ok(time) = parse_local_time_input(time_string).await {
        naive_time = time;
    } else {
        println!("Failed to parse the time string");
        return Err(EvaluationError::ParseError);
    }

    Ok(NaiveDateTime::new(naive_date, naive_time))
}

async fn parse_zoned_time_input(input: &str) -> Result<ZonedTime, EvaluationError> {
    let input_string = input.to_string().replace(':', "");
    let contains_frac = input.contains('.');
    let is_utc = input.contains('Z');

    let time_string = match input_string
        .split(|c| c == 'Z' || c == '+' || c == '-')
        .next()
    {
        Some(time) => time,
        None => return Err(EvaluationError::ParseError),
    };

    let timezone = match extract_timezone(&input_string).await {
        Some(tz) => tz,
        None => return Err(EvaluationError::ParseError),
    };

    if is_utc {
        let offset = FixedOffset::east_opt(0).unwrap();
        if contains_frac {
            let naive_time = match NaiveTime::parse_from_str(time_string, "%H%M%S%.f") {
                Ok(t) => t,
                Err(_) => return Err(EvaluationError::ParseError),
            };
            return Ok(ZonedTime::new(naive_time, offset));
        }
        let naive_time = match NaiveTime::parse_from_str(time_string, "%H%M%S") {
            Ok(t) => t,
            Err(_) => return Err(EvaluationError::ParseError),
        };
        return Ok(ZonedTime::new(naive_time, offset));
    }

    let naive_time;

    if let Ok(time) = parse_local_time_input(time_string).await {
        naive_time = time;
    } else {
        println!("Failed to parse the time string");
        return Err(EvaluationError::ParseError);
    }

    let offset = match FixedOffset::from_str(timezone) {
        Ok(o) => o,
        Err(_) => return Err(EvaluationError::ParseError),
    };

    if contains_frac {
        let naive_time = match NaiveTime::parse_from_str(time_string, "%H%M%S%.f") {
            Ok(t) => t,
            Err(_) => return Err(EvaluationError::ParseError),
        };
        let zoned_time = ZonedTime::new(naive_time, offset);
        return Ok(zoned_time);
    }

    let zoned_time = ZonedTime::new(naive_time, offset);
    Ok(zoned_time)
}

async fn extract_timezone(input: &str) -> Option<&str> {
    let re = Regex::new(r"(\d{2,6}(\.\d{1,9})?)(([+-].*)|Z|(\[.*?\])?)?").unwrap();

    if let Some(captures) = re.captures(input) {
        captures.get(3).map(|m| m.as_str())
    } else {
        None
    }
}

async fn parse_zoned_date_time_input(input: &str) -> Result<ZonedDateTime, EvaluationError> {
    let is_utc = input.contains('Z');

    let date_part = match input.split('T').next() {
        Some(date) => date.replace('-', ""),
        None => return Err(EvaluationError::ParseError),
    };
    let date_string = date_part.as_str();

    let timezone_part = match input.split('T').last() {
        Some(timezone) => timezone.replace(':', ""),
        None => return Err(EvaluationError::ParseError),
    };
    let timezone_string = timezone_part.as_str();

    let time_string = match timezone_string
        .split(|c| c == '[' || c == 'Z' || c == '+' || c == '-')
        .next()
    {
        Some(time) => time,
        None => return Err(EvaluationError::ParseError),
    };
    let timezone = match extract_timezone(timezone_string).await {
        Some(tz) => tz,
        None => return Err(EvaluationError::ParseError),
    };

    let naive_date;
    let naive_time;

    if let Ok(date) = parse_date_string(date_string).await {
        naive_date = date;
    } else {
        println!("Failed to parse the date string");
        return Err(EvaluationError::ParseError);
    }

    if let Ok(time) = parse_local_time_input(time_string).await {
        naive_time = time;
    } else {
        println!("Failed to parse the time string");
        return Err(EvaluationError::ParseError);
    }

    let naive_date_time = NaiveDateTime::new(naive_date, naive_time);
    if is_utc {
        let date_time = match NaiveDateTime::and_local_timezone(
            &naive_date_time,
            FixedOffset::east_opt(0).unwrap(),
        ) {
            LocalResult::Single(dt) => dt,
            _ => return Err(EvaluationError::ParseError),
        };
        let zoned_date_time = ZonedDateTime::new(date_time, None);
        return Ok(zoned_date_time);
    }

    if timezone.contains('[') && timezone.contains(']') {
        let tz = match handle_iana_timezone(timezone).await {
            Some(t) => t,
            _err => return Err(EvaluationError::ParseError),
        };
        let date_time = match NaiveDateTime::and_local_timezone(&naive_date_time, tz) {
            LocalResult::Single(dt) => dt,
            _ => return Err(EvaluationError::ParseError),
        };
        let date_time_offset = date_time.fixed_offset();
        let zoned_date_time = ZonedDateTime::new(date_time_offset, Some(timezone.to_string()));
        return Ok(zoned_date_time);
    }

    let offset = match FixedOffset::from_str(timezone) {
        Ok(o) => o,
        Err(_) => {
            return Err(EvaluationError::FunctionError {
                function_name: "parse_zoned_date_time_input".to_string(),
                error: Box::new(EvaluationError::ParseError),
            })
        }
    };
    let date_time = match NaiveDateTime::and_local_timezone(&naive_date_time, offset) {
        LocalResult::Single(dt) => dt,
        _ => return Err(EvaluationError::ParseError),
    };
    let zoned_date_time = ZonedDateTime::new(date_time, None);
    Ok(zoned_date_time)
}
