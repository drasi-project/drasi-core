use std::f32::consts::E;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use lazy_static::lazy_static;

lazy_static! {
    pub static ref EPOCH_NAIVE_DATE: NaiveDate = match NaiveDate::from_ymd_opt(1970, 1, 1) {
        Some(date) => date,
        None => unreachable!(),
    };
    pub static ref MIDNIGHT_NAIVE_TIME: NaiveTime = match NaiveTime::from_hms_opt(0, 0, 0) {
        Some(time) => time,
        None => unreachable!(),
    };
    pub static ref EPOCH_MIDNIGHT_NAIVE_DATETIME: NaiveDateTime = {
        NaiveDateTime::new(*EPOCH_NAIVE_DATE, *MIDNIGHT_NAIVE_TIME)
    };
    pub static ref UTC_FIXED_OFFSET: chrono::FixedOffset = match chrono::FixedOffset::east_opt(0) {
        Some(offset) => offset,
        None => unreachable!(),
    };
}

pub const INVALID_LOCAL_DATETIME_FORMAT_ERROR: &str = "A valid string representation of a local datetime or a map of local datetime components.\n\
                                Examples include: localdatetime('2015-07-21T21:40:32.142'), localdatetime('2015202T21'), localdatetime({year: 1984, week: 10, dayOfWeek: 3,hour: 12, minute: 31, second: 14, millisecond: 645})";

pub const INVALID_ZONED_TIME_FORMAT_ERROR: &str = "A valid string representation of a zoned time or a map of zoned time components. \n\
                                Examples include: time('214032.142Z'), time('21:40:32+01:00'), time('214032-0100'), time({hour: 12, minute: 31, second: 14, microsecond: 645876, timezone: '+01:00'})";

pub const INVALID_ZONED_DATETIME_FORMAT_ERROR: &str = "A valid string representation of a zoned datetime or a map of zoned datetime components.\n\
                                Examples include: datetime('20150721T21:40-01:30'), datetime('2015-07-21T21:40:32.142+0100'), datetime({year: 1984, ordinalDay: 202, hour: 12, minute: 31, second: 14, timezone: '+01:00'})";

pub const INVALID_LOCAL_TIME_FORMAT_ERROR: &str = "A valid string representation of a local time or a map of local time components.\n\
                                Examples include: localtime('21:40:32.142'), localtime('214032.142'), localtime({hour: 12, minute: 31, second: 14, nanosecond: 789, millisecond: 123, microsecond: 456})";

pub const INVALID_DURATION_FORMAT_ERROR: &str = "A valid string representation of a duration or a map of duration components \n\
                        Examples include: duration({days: 14, hours:16, minutes: 12}), duration({minutes: 1.5, seconds: 1, nanoseconds: 123456789}), duration('P14DT16H12M'), duration('PT0.75M'),";

pub const INVALID_DATE_FORMAT_ERROR: &str = "A valid string representation of a date or a map of date components.\n\
                        Examples include: date('2015-07-21'), date('2015202'), date({year: 1984, week: 10, dayOfWeek: 3})";