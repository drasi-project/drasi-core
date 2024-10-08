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
