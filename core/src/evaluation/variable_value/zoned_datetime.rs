use chrono::{DateTime, FixedOffset, TimeZone};
use core::fmt::{self, Display};
use serde::{Deserialize, Serialize};
use std::hash::Hash;

#[derive(Clone, Eq, Hash, Serialize, Deserialize)]
pub struct ZonedDateTime {
    datetime: DateTime<FixedOffset>,
    timezone: Option<String>, // timezone is optional; depending on the input
}

impl ZonedDateTime {
    pub fn new(datetime: DateTime<FixedOffset>, timezone: Option<String>) -> Self {
        ZonedDateTime { datetime, timezone }
    }

    pub fn from_epoch_millis(epoch_millis: u64) -> Self {
        let offset = FixedOffset::east_opt(0).unwrap();
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
