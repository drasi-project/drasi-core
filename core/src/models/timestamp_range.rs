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

use crate::models::ElementTimestamp;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct TimestampRange<T> {
    pub from: TimestampBound<T>,
    pub to: ElementTimestamp,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum TimestampBound<T> {
    StartFromPrevious(T),
    Included(T),
}

impl<T> TimestampBound<T> {
    // &TimestampBound<T> -> TimestampBound<&T>
    pub fn as_ref(&self) -> TimestampBound<&T> {
        match *self {
            TimestampBound::StartFromPrevious(ref t) => TimestampBound::StartFromPrevious(t),
            TimestampBound::Included(ref t) => TimestampBound::Included(t),
        }
    }

    // &mut TimestampBound<T> -> TimestampBound<&mut T>
    pub fn as_mut(&mut self) -> TimestampBound<&mut T> {
        match *self {
            TimestampBound::StartFromPrevious(ref mut t) => TimestampBound::StartFromPrevious(t),
            TimestampBound::Included(ref mut t) => TimestampBound::Included(t),
        }
    }

    pub fn get_timestamp(&self) -> &T {
        match self {
            TimestampBound::StartFromPrevious(t) => t,
            TimestampBound::Included(t) => t,
        }
    }
}

impl<T: Clone> TimestampBound<&T> {
    pub fn cloned(self) -> TimestampBound<T> {
        match self {
            TimestampBound::StartFromPrevious(t) => TimestampBound::StartFromPrevious(t.clone()),
            TimestampBound::Included(t) => TimestampBound::Included(t.clone()),
        }
    }
}

impl TimestampRange<ElementTimestamp> {
    /// Returns true if the given timestamp falls within this range.
    ///
    /// The lower bound is inclusive for `Included`, and treated as exclusive
    /// for `StartFromPrevious` (the previous value itself is not part of the
    /// new range). The upper bound `to` is inclusive.
    pub fn contains(&self, ts: ElementTimestamp) -> bool {
        if ts > self.to {
            return false;
        }
        match &self.from {
            TimestampBound::Included(from) => ts >= *from,
            TimestampBound::StartFromPrevious(from) => ts > *from,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_included_lower_bound() {
        let range = TimestampRange {
            from: TimestampBound::Included(10u64),
            to: 20u64,
        };
        assert!(range.contains(10));
        assert!(range.contains(15));
        assert!(range.contains(20));
        assert!(!range.contains(9));
        assert!(!range.contains(21));
    }

    #[test]
    fn contains_start_from_previous_is_exclusive_lower() {
        let range = TimestampRange {
            from: TimestampBound::StartFromPrevious(10u64),
            to: 20u64,
        };
        assert!(!range.contains(10));
        assert!(range.contains(11));
        assert!(range.contains(20));
    }

    #[test]
    fn contains_empty_range_when_from_equals_to_included() {
        let range = TimestampRange {
            from: TimestampBound::Included(5u64),
            to: 5u64,
        };
        assert!(range.contains(5));
        assert!(!range.contains(4));
        assert!(!range.contains(6));
    }
}
