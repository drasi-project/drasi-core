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

use chrono::{FixedOffset, NaiveTime};
use core::fmt::{self, Display};
use std::hash::Hash;

#[derive(Clone, Eq, PartialEq, Hash, Copy)]
pub struct ZonedTime {
    time: NaiveTime,
    offset: FixedOffset,
}

// impl Eq for ZonedTime {
//     fn eq(&self, other: &Self) -> bool {
//         match (self.time, other.time, self.offset, other.offset) {
//             (a, b, c, d) => a == b && c == d,
//         }
//     }
// }

impl core::fmt::Debug for ZonedTime {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "time: {}, offset: {}", self.time, self.offset)
    }
}

impl PartialEq<NaiveTime> for ZonedTime {
    fn eq(&self, other: &NaiveTime) -> bool {
        let (a, b) = (self.time, *other);
        a == b
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
