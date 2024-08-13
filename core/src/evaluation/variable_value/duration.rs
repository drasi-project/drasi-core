use chrono::Duration as ChronoDuration;
use core::fmt::{self, Display};
use std::hash::Hash;
use std::ops;

use crate::evaluation::EvaluationError;

#[derive(Clone, Eq, Hash)]
pub struct Duration {
    duration: ChronoDuration,
    year: i64,
    month: i64,
    // Rust does not support specifying the year or month as a duration (leap year concerns, etc.)
    // so we need to store them separately.
    // For years/months, we can't know the exact "duration" without knowing the starting date.
    // For example, 1 month could be 28, 29, 30, or 31 days depending on the starting date and
    // 1 year could be 365 or 366 days depending on the starting date.
}


impl PartialEq for Duration {
    fn eq(&self, other: &Self) -> bool {
        match (
            self.duration,
            other.duration,
            self.year,
            other.year,
            self.month,
            other.month,
        ) {
            (a, b, c, d, e, f) => a == b && c == d && e == f,
        }
    }
}

impl Duration {
    pub fn new(duration: ChronoDuration, year: i64, month: i64) -> Self {
        Duration {
            duration,
            year,
            month,
        }
    }

    pub fn try_add(&self, duration2: &Duration) -> Result<Duration, EvaluationError> {
        let new_duration = match self.duration.checked_add(duration2.duration()) {
            Some(d) => d,
            None => return Err(EvaluationError::OverflowError),
        };
        let mut year = self.year + duration2.year;
        let mut month = self.month + duration2.month;
        if month > 12 {
            year += 1;
            month -= 12;
        }
        Ok(Duration::new(new_duration, year, month))
    }

    pub fn try_sub(&self, duration2: &Duration) -> Result<Duration, EvaluationError> {
        let new_duration = match self.duration.checked_sub(duration2.duration()) {
            Some(d) => d,
            None => return Err(EvaluationError::OverflowError),
        };
        let mut year = self.year - duration2.year;
        let mut month = self.month - duration2.month;
        if month < 1 {
            year -= 1;
            month += 12;
        }
        Ok(Duration::new(new_duration, year, month))
    }

    pub fn duration(&self) -> &ChronoDuration {
        &self.duration
    }

    pub fn year(&self) -> &i64 {
        &self.year
    }

    pub fn month(&self) -> &i64 {
        &self.month
    }
}

impl Display for Duration {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        self.duration.fmt(formatter)
    }
}
