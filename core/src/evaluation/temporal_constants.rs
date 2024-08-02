use std::f32::consts::E;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use lazy_static::lazy_static;

lazy_static! {
    pub static ref EPOCH_NAIVE_DATE: NaiveDate = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    pub static ref MIDNIGHT_NAIVE_TIME: NaiveTime = match NaiveTime::from_hms_opt(0, 0, 0) {
        Some(time) => time,
        None => unreachable!(),
    };
    pub static ref EPOCH_MIDNIGHT_NAIVE_DATETIME: NaiveDateTime =
        NaiveDate::from_ymd_opt(1970, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();

    pub static ref UTC_FIXED_OFFSET: chrono::FixedOffset = match chrono::FixedOffset::east_opt(0) {
        Some(offset) => offset,
        None => unreachable!(),
    };
}
