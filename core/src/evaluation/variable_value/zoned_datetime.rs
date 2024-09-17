use crate::evaluation::temporal_constants::{self, UTC_FIXED_OFFSET};
use chrono::{DateTime, FixedOffset, TimeZone};
use core::fmt::{self, Display};
use serde::{Deserialize, Serialize};
use std::hash::Hash;

#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ZonedDateTime {
    datetime: DateTime<FixedOffset>,
    timezone: Option<String>, // timezone is optional; depending on the input
}

impl ZonedDateTime {
    pub fn new(datetime: DateTime<FixedOffset>, timezone: Option<String>) -> Self {
        ZonedDateTime { datetime, timezone }
    }

    pub fn from_epoch_millis(epoch_millis: u64) -> Self {
        let offset = *UTC_FIXED_OFFSET;
        let datetime = offset.timestamp_millis_opt(epoch_millis as i64).unwrap();
        ZonedDateTime {
            datetime,
            timezone: None,
        }
    }

    pub fn datetime(&self) -> &DateTime<FixedOffset> {
        &self.datetime
    }

    pub fn timezone(&self) -> &Option<String> {
        &self.timezone
    }
}

impl Display for ZonedDateTime {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        self.datetime.fmt(formatter)
    }
}
