use std::f32::consts::E;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use lazy_static::lazy_static;

lazy_static! {
    pub static ref EPOCH_NAIVE_DATE: NaiveDate = match NaiveDate::from_ymd_opt(1970, 1, 1) {
        Some(date) => date,
        None => unreachable!(), // This should never happen
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
