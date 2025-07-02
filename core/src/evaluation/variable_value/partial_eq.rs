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

use super::VariableValue;
use crate::evaluation::variable_value::duration::Duration;
use crate::evaluation::variable_value::float::Float;
use chrono::{NaiveDate, NaiveTime};

fn eq_i64(value: &VariableValue, other: i64) -> bool {
    match value {
        VariableValue::Integer(n) => n.as_i64().map_or(false, |n| n == other),
        VariableValue::Float(n) => n.as_f64().map_or(false, |n| n == other as f64),
        _ => false,
    }
}

fn eq_u64(value: &VariableValue, other: u64) -> bool {
    match value {
        VariableValue::Integer(n) => n.as_u64().map_or(false, |n| n == other),
        VariableValue::Float(n) => n.as_f64().map_or(false, |n| n == other as f64),
        _ => false,
    }
}

fn eq_f32(value: &VariableValue, other: f32) -> bool {
    match value {
        VariableValue::Float(n) => n.as_f32().map_or(false, |n| n == other),
        VariableValue::Integer(n) => n.as_i64().map_or(false, |n| n as f64 == other as f64),
        _ => false,
    }
}

fn eq_f64(value: &VariableValue, other: f64) -> bool {
    match value {
        VariableValue::Float(n) => n.as_f64().map_or(false, |n| n == other),
        VariableValue::Integer(n) => n.as_i64().map_or(false, |n| n as f64 == other),
        _ => false,
    }
}

fn eq_bool(value: &VariableValue, other: bool) -> bool {
    value.as_bool().map_or(false, |n| n == other)
}

fn eq_str(value: &VariableValue, other: &str) -> bool {
    value.as_str().map_or(false, |n| n == other)
}

impl PartialEq<str> for VariableValue {
    fn eq(&self, other: &str) -> bool {
        eq_str(self, other)
    }
}

impl PartialEq<&str> for VariableValue {
    fn eq(&self, other: &&str) -> bool {
        eq_str(self, other)
    }
}

impl PartialEq<VariableValue> for str {
    fn eq(&self, other: &VariableValue) -> bool {
        eq_str(other, self)
    }
}

impl PartialEq<VariableValue> for &str {
    fn eq(&self, other: &VariableValue) -> bool {
        eq_str(other, self)
    }
}

impl PartialEq<String> for VariableValue {
    fn eq(&self, other: &String) -> bool {
        eq_str(self, other.as_str())
    }
}

impl PartialEq<VariableValue> for String {
    fn eq(&self, other: &VariableValue) -> bool {
        eq_str(other, self.as_str())
    }
}

impl PartialEq<Float> for VariableValue {
    fn eq(&self, other: &Float) -> bool {
        eq_f64(
            self,
            match other.as_f64() {
                Some(n) => n,
                None => unreachable!(),
            },
        )
    }
}

impl PartialEq<NaiveTime> for VariableValue {
    fn eq(&self, other: &NaiveTime) -> bool {
        match self {
            VariableValue::LocalTime(time) => time == other,
            _ => false,
        }
    }
}

impl PartialEq<Duration> for VariableValue {
    fn eq(&self, other: &Duration) -> bool {
        match self {
            VariableValue::Duration(duration) => duration == other,
            _ => false,
        }
    }
}

impl PartialEq<NaiveDate> for VariableValue {
    fn eq(&self, other: &NaiveDate) -> bool {
        match self {
            VariableValue::Date(date) => date == other,
            _ => false,
        }
    }
}

impl PartialEq<VariableValue> for VariableValue {
    fn eq(&self, other: &VariableValue) -> bool {
        match (self, other) {
            (VariableValue::Integer(n), VariableValue::Integer(m)) => n == m,
            (VariableValue::Float(n), VariableValue::Float(m)) => n == m,
            (VariableValue::Integer(n), VariableValue::Float(m)) => n.as_i64().map_or(false, |n| {
                n as f64
                    == match m.as_f64() {
                        Some(m) => m,
                        None => unreachable!(),
                    }
            }),
            (VariableValue::Float(n), VariableValue::Integer(m)) => m.as_i64().map_or(false, |m| {
                m as f64
                    == match n.as_f64() {
                        Some(n) => n,
                        None => unreachable!(),
                    }
            }),
            (VariableValue::Bool(n), VariableValue::Bool(m)) => n == m,
            (VariableValue::String(n), VariableValue::String(m)) => n == m,
            (VariableValue::List(list1), VariableValue::List(list2)) => list1 == list2,
            (VariableValue::Null, VariableValue::Null) => true,
            (VariableValue::Object(obj1), VariableValue::Object(obj2)) => obj1 == obj2,
            (VariableValue::Date(date1), VariableValue::Date(date2)) => date1 == date2,
            (VariableValue::LocalTime(time1), VariableValue::LocalTime(time2)) => time1 == time2,
            (VariableValue::ZonedTime(time1), VariableValue::ZonedTime(time2)) => time1 == time2,
            (VariableValue::LocalDateTime(time1), VariableValue::LocalDateTime(time2)) => {
                time1 == time2
            }
            (VariableValue::ZonedDateTime(time1), VariableValue::ZonedDateTime(time2)) => {
                time1 == time2
            }
            (VariableValue::Duration(duration1), VariableValue::Duration(duration2)) => {
                duration1 == duration2
            }
            (VariableValue::Element(element1), VariableValue::Element(element2)) => {
                element1 == element2
            }
            (
                VariableValue::ElementReference(reference1),
                VariableValue::ElementReference(reference2),
            ) => reference1 == reference2,
            (
                VariableValue::ElementMetadata(metadata1),
                VariableValue::ElementMetadata(metadata2),
            ) => metadata1 == metadata2,
            (VariableValue::Expression(e1), VariableValue::Expression(e2)) => e1 == e2,
            (VariableValue::ListRange(r1), VariableValue::ListRange(r2)) => r1 == r2,
            (VariableValue::Awaiting, VariableValue::Awaiting) => true,
            _ => false,
        }
    }
}

macro_rules! partialeq_numeric {
    ($($eq:ident [$($ty:ty)*])*) => {
        $($(
            impl PartialEq<$ty> for VariableValue {
                fn eq(&self, other: &$ty) -> bool {
                    $eq(self, *other as _)
                }
            }

            impl PartialEq<VariableValue> for $ty {
                fn eq(&self, other: &VariableValue) -> bool {
                    $eq(other, *self as _)
                }
            }

            impl<'a> PartialEq<$ty> for &'a VariableValue {
                fn eq(&self, other: &$ty) -> bool {
                    $eq(*self, *other as _)
                }
            }

            impl<'a> PartialEq<$ty> for &'a mut VariableValue {
                fn eq(&self, other: &$ty) -> bool {
                    $eq(*self, *other as _)
                }
            }
        )*)*
    }
}

partialeq_numeric! {
    eq_i64[i8 i16 i32 i64 isize]
    eq_u64[u8 u16 u32 u64 usize]
    eq_f32[f32]
    eq_f64[f64]
    eq_bool[bool]
}

use std::cmp::Ordering;
impl PartialOrd for VariableValue {
    fn partial_cmp(&self, other: &VariableValue) -> Option<Ordering> {
        match (self, other) {
            (VariableValue::Integer(lhs), VariableValue::Integer(rhs)) => {
                lhs.as_i64().partial_cmp(&rhs.as_i64())
            }
            (VariableValue::Float(lhs), VariableValue::Float(rhs)) => {
                lhs.as_f64().partial_cmp(&rhs.as_f64())
            }
            (VariableValue::Integer(lhs), VariableValue::Float(rhs)) => lhs
                .as_i64()
                .map(|lhs| lhs as f64)
                .partial_cmp(&rhs.as_f64()),
            (VariableValue::Float(lhs), VariableValue::Integer(rhs)) => lhs
                .as_f64()
                .partial_cmp(&rhs.as_i64().map(|rhs| rhs as f64)),
            (
                VariableValue::Integer(_),
                VariableValue::String(_)
                | VariableValue::Null
                | VariableValue::Bool(_)
                | VariableValue::Date(_)
                | VariableValue::LocalTime(_)
                | VariableValue::LocalDateTime(_)
                | VariableValue::ZonedTime(_)
                | VariableValue::ZonedDateTime(_)
                | VariableValue::Duration(_),
            ) => Some(Ordering::Greater),
            (
                VariableValue::Float(_),
                VariableValue::String(_)
                | VariableValue::Null
                | VariableValue::Bool(_)
                | VariableValue::Date(_)
                | VariableValue::LocalTime(_)
                | VariableValue::LocalDateTime(_)
                | VariableValue::ZonedTime(_)
                | VariableValue::ZonedDateTime(_)
                | VariableValue::Duration(_),
            ) => Some(Ordering::Greater),
            (VariableValue::String(s1), VariableValue::String(s2)) => s1.partial_cmp(s2),
            (
                VariableValue::String(_),
                VariableValue::Null
                | VariableValue::Bool(_)
                | VariableValue::Date(_)
                | VariableValue::LocalTime(_)
                | VariableValue::LocalDateTime(_)
                | VariableValue::ZonedTime(_)
                | VariableValue::ZonedDateTime(_)
                | VariableValue::Duration(_),
            ) => Some(Ordering::Greater),
            (VariableValue::String(_), VariableValue::Integer(_) | VariableValue::Float(_)) => {
                Some(Ordering::Less)
            }
            (VariableValue::Null, VariableValue::Null) => Some(Ordering::Equal),
            (
                VariableValue::Null,
                VariableValue::Bool(_)
                | VariableValue::Float(_)
                | VariableValue::Integer(_)
                | VariableValue::Date(_)
                | VariableValue::LocalTime(_)
                | VariableValue::LocalDateTime(_)
                | VariableValue::ZonedTime(_)
                | VariableValue::ZonedDateTime(_)
                | VariableValue::Duration(_),
            ) => Some(Ordering::Less),
            (VariableValue::Bool(b1), VariableValue::Bool(b2)) => b1.partial_cmp(b2),
            (
                VariableValue::Bool(_),
                VariableValue::String(_)
                | VariableValue::Date(_)
                | VariableValue::Float(_)
                | VariableValue::Integer(_)
                | VariableValue::LocalTime(_)
                | VariableValue::LocalDateTime(_)
                | VariableValue::ZonedTime(_)
                | VariableValue::ZonedDateTime(_)
                | VariableValue::Duration(_),
            ) => Some(Ordering::Less),
            (VariableValue::Date(d1), VariableValue::Date(d2)) => d1.partial_cmp(d2),
            (
                VariableValue::Date(_),
                VariableValue::LocalTime(_)
                | VariableValue::String(_)
                | VariableValue::Float(_)
                | VariableValue::Integer(_)
                | VariableValue::LocalDateTime(_)
                | VariableValue::ZonedTime(_)
                | VariableValue::ZonedDateTime(_)
                | VariableValue::Duration(_),
            ) => Some(Ordering::Less),
            (VariableValue::LocalTime(t1), VariableValue::LocalTime(t2)) => t1.partial_cmp(t2),
            (
                VariableValue::LocalTime(_),
                VariableValue::LocalDateTime(_)
                | VariableValue::Float(_)
                | VariableValue::String(_)
                | VariableValue::Integer(_)
                | VariableValue::ZonedTime(_)
                | VariableValue::ZonedDateTime(_)
                | VariableValue::Duration(_),
            ) => Some(Ordering::Less),
            (VariableValue::LocalDateTime(t1), VariableValue::LocalDateTime(t2)) => {
                t1.partial_cmp(t2)
            }
            (
                VariableValue::LocalDateTime(_),
                VariableValue::ZonedTime(_)
                | VariableValue::Float(_)
                | VariableValue::String(_)
                | VariableValue::Integer(_)
                | VariableValue::ZonedDateTime(_)
                | VariableValue::Duration(_),
            ) => Some(Ordering::Less),
            (VariableValue::ZonedTime(t1), VariableValue::ZonedTime(t2)) => {
                t1.time().partial_cmp(t2.time())
            }
            (
                VariableValue::ZonedTime(_),
                VariableValue::ZonedDateTime(_)
                | VariableValue::Float(_)
                | VariableValue::Integer(_)
                | VariableValue::String(_)
                | VariableValue::Duration(_),
            ) => Some(Ordering::Less),
            (VariableValue::ZonedDateTime(t1), VariableValue::ZonedDateTime(t2)) => {
                t1.datetime().partial_cmp(t2.datetime())
            }
            (
                VariableValue::ZonedDateTime(_),
                VariableValue::Float(_)
                | VariableValue::String(_)
                | VariableValue::Integer(_)
                | VariableValue::Duration(_),
            ) => Some(Ordering::Less),
            (VariableValue::Duration(d1), VariableValue::Duration(d2)) => {
                d1.duration().partial_cmp(d2.duration())
            }
            (
                VariableValue::Duration(_),
                VariableValue::Float(_) | VariableValue::String(_) | VariableValue::Integer(_),
            ) => Some(Ordering::Less),
            _ => None,
        }
    }
}

// impl Ord for VariableValue {
//     fn cmp(&self, other: &Self) -> Ordering {
//         Ordering::Equal
//     }
// }
