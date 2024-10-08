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

use chrono::Duration as ChronoDuration;
use core::fmt::{self, Display};
use std::hash::Hash;
use std::ops;

#[derive(Clone, Eq, Hash, PartialEq)]
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

impl ops::Add<Duration> for Duration {
    type Output = Duration;
    #[allow(clippy::unwrap_used)]
    fn add(self, duration2: Duration) -> Duration {
        let new_duration = self.duration().checked_add(duration2.duration()).unwrap();
        let mut year = self.year() + duration2.year();
        let mut month = self.month() + duration2.month();
        if month > 12 {
            year += 1;
            month -= 12;
        }
        Duration::new(new_duration, year, month)
    }
}

impl ops::Sub<Duration> for Duration {
    type Output = Duration;
    #[allow(clippy::unwrap_used)]
    fn sub(self, duration2: Duration) -> Duration {
        let new_duration = self.duration().checked_sub(duration2.duration()).unwrap();
        let mut year = self.year() - duration2.year();
        let mut month = self.month() - duration2.month();
        if month < 1 {
            year -= 1;
            month += 12;
        }
        Duration::new(new_duration, year, month)
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
