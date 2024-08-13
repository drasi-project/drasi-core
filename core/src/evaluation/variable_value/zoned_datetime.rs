use chrono::{offset::LocalResult, DateTime, FixedOffset, TimeZone};
use core::fmt::{self, Display};
use serde::{Deserialize, Serialize};
use std::hash::Hash;

use crate::evaluation::{temporal_constants, EvaluationError};

#[derive(Clone, Eq, Hash, Serialize, Deserialize)]
pub struct ZonedDateTime {
    datetime: DateTime<FixedOffset>,
    timezone: Option<String>, // timezone is optional; depending on the input
}

impl ZonedDateTime {
    pub fn new(datetime: DateTime<FixedOffset>, timezone: Option<String>) -> Self {
        ZonedDateTime { datetime, timezone }
    }

    pub fn from_epoch_millis(epoch_millis: u64) -> Result<Self,EvaluationError> {
        let offset = *temporal_constants::UTC_FIXED_OFFSET;
        let datetime = match offset.timestamp_millis_opt(epoch_millis as i64) {
            LocalResult::Single(datetime) => datetime,
            _ => return Err(EvaluationError::OverflowError),
        };
        Ok(ZonedDateTime {
            datetime,
            timezone: None,
        })
    }

    pub fn datetime(&self) -> &DateTime<FixedOffset> {
        &self.datetime
    }

    pub fn timezone(&self) -> &Option<String> {
        &self.timezone
    }
}

impl PartialEq for ZonedDateTime {
    fn eq(&self, other: &Self) -> bool {
        match (
            self.datetime,
            other.datetime,
            self.timezone.clone(),
            other.timezone.clone(),
        ) {
            (a, b, c, d) => {
                let utc_time1 = a.with_timezone(&chrono::Utc);
                let utc_time2 = b.with_timezone(&chrono::Utc);
                let timezone = c.unwrap_or_default();
                let other_timezone = d.unwrap_or_default();
                utc_time1 == utc_time2 && timezone == other_timezone
            }
        }
    }
}

impl Display for ZonedDateTime {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        self.datetime.fmt(formatter)
    }
}
