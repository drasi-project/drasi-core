use chrono::{FixedOffset, NaiveTime};
use core::fmt::{self, Display};
use std::hash::Hash;

#[derive(Clone, Eq, Hash, Copy)]
pub struct ZonedTime {
    time: NaiveTime,
    offset: FixedOffset,
}

impl PartialEq for ZonedTime {
    fn eq(&self, other: &Self) -> bool {
        match (self.time, other.time, self.offset, other.offset) {
            (a, b, c, d) => a == b && c == d,
        }
    }
}

impl core::fmt::Debug for ZonedTime {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "time: {}, offset: {}", self.time, self.offset)
    }
}

impl PartialEq<NaiveTime> for ZonedTime {
    fn eq(&self, other: &NaiveTime) -> bool {
        match (self.time, *other) {
            (a, b) => a == b,
        }
    }
}

impl Display for ZonedTime {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        self.time.fmt(formatter)
    }
}

impl ZonedTime {
    #[inline]
    pub fn is_zoned_time(&self) -> bool {
        true
    }

    #[inline]
    pub fn as_zoned_time(&self) -> Option<&ZonedTime> {
        Some(self)
    }

    pub fn new(time: NaiveTime, offset: FixedOffset) -> Self {
        ZonedTime { time, offset }
    }

    pub fn time(&self) -> &NaiveTime {
        &self.time
    }

    pub fn offset(&self) -> &FixedOffset {
        &self.offset
    }
}
