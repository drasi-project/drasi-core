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
use crate::evaluation::variable_value::integer::Integer;
use chrono::{NaiveDate, NaiveTime};

fn eq_i64(value: &VariableValue, other: i64) -> bool {
    match value {
        VariableValue::Integer(n) => n.as_i64() == Some(other),
        VariableValue::Float(n) => n == &Integer::from(other),
        _ => false,
    }
}

fn eq_u64(value: &VariableValue, other: u64) -> bool {
    match value {
        VariableValue::Integer(n) => n.as_u64() == Some(other),
        VariableValue::Float(n) => n == &Integer::from(other),
        _ => false,
    }
}

fn eq_f32(value: &VariableValue, other: f32) -> bool {
    eq_f64(value, f64::from(other))
}

fn eq_f64(value: &VariableValue, other: f64) -> bool {
    match value {
        VariableValue::Float(n) => n == &other,
        VariableValue::Integer(n) => n == &Float::from(other),
        _ => false,
    }
}

fn eq_bool(value: &VariableValue, other: bool) -> bool {
    value.as_bool() == Some(other)
}

fn eq_str(value: &VariableValue, other: &str) -> bool {
    value.as_str() == Some(other)
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
        match self {
            VariableValue::Float(value) => value == other,
            VariableValue::Integer(value) => value == other,
            _ => false,
        }
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
            (VariableValue::Integer(integer), VariableValue::Float(float))
            | (VariableValue::Float(float), VariableValue::Integer(integer)) => float == integer,
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
            (VariableValue::Integer(lhs), VariableValue::Integer(rhs)) => lhs.partial_cmp(rhs),
            (VariableValue::Float(lhs), VariableValue::Float(rhs)) => lhs.partial_cmp(rhs),
            (VariableValue::Integer(lhs), VariableValue::Float(rhs)) => lhs.partial_cmp(rhs),
            (VariableValue::Float(lhs), VariableValue::Integer(rhs)) => lhs.partial_cmp(rhs),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_ordering_matches_exact_equality() {
        let signed = |value: i64| VariableValue::Integer(value.into());
        let unsigned = |value: u64| VariableValue::Integer(value.into());
        let float = |value: f64| VariableValue::Float(value.into());
        let groups = [
            vec![float(f64::NEG_INFINITY)],
            vec![float(-f64::MAX)],
            vec![float(f64::from_bits((i64::MIN as f64).to_bits() + 1))],
            vec![signed(i64::MIN), float(i64::MIN as f64)],
            vec![signed(i64::MIN + 1)],
            vec![signed(-9_007_199_254_740_993)],
            vec![
                signed(-9_007_199_254_740_992),
                float(-9_007_199_254_740_992.0),
            ],
            vec![signed(-1), float(-1.0)],
            vec![float(-0.5)],
            vec![float(-f64::from_bits(1))],
            vec![signed(0), unsigned(0), float(-0.0), float(0.0)],
            vec![float(f64::from_bits(1))],
            vec![float(0.5)],
            vec![signed(1), unsigned(1), float(1.0)],
            vec![
                unsigned(9_007_199_254_740_992),
                float(9_007_199_254_740_992.0),
            ],
            vec![unsigned(9_007_199_254_740_993)],
            vec![signed(i64::MAX)],
            vec![unsigned(1u64 << 63), float((1u64 << 63) as f64)],
            vec![unsigned(u64::MAX - 1)],
            vec![unsigned(u64::MAX)],
            vec![float(u64::MAX as f64)],
            vec![float(f64::MAX)],
            vec![float(f64::INFINITY)],
        ];
        for (left_group, left_values) in groups.iter().enumerate() {
            for (right_group, right_values) in groups.iter().enumerate() {
                let expected = left_group.cmp(&right_group);
                for left in left_values {
                    for right in right_values {
                        assert_eq!(
                            left.partial_cmp(right),
                            Some(expected),
                            "{left:?} compared with {right:?}"
                        );
                        assert_eq!(
                            left == right,
                            expected == Ordering::Equal,
                            "{left:?} compared with {right:?}"
                        );
                    }
                }
            }
        }
        let nan = float(f64::NAN);
        for value in groups.iter().flatten().chain(std::iter::once(&nan)) {
            assert_eq!(nan.partial_cmp(value), None);
            assert_eq!(value.partial_cmp(&nan), None);
            assert_ne!(nan, *value);
        }
    }

    #[test]
    fn primitive_numeric_equality_is_exact() {
        let rounded = VariableValue::Float(9_007_199_254_740_992.0.into());
        assert_ne!(rounded, 9_007_199_254_740_993i64);
        assert_ne!(9_007_199_254_740_993u64, rounded);
        let integer = VariableValue::Integer(9_007_199_254_740_993u64.into());
        assert_ne!(integer, 9_007_199_254_740_992.0f64);
        assert_ne!(9_007_199_254_740_992.0f64, integer);
        let unsigned = VariableValue::Integer((1u64 << 63).into());
        assert_eq!(unsigned, (1u64 << 63) as f64);
        assert_eq!((1u64 << 63) as f64, unsigned);
        assert_ne!(VariableValue::Integer(u64::MAX.into()), u64::MAX as f64);
        assert_ne!(VariableValue::Float(1.000_000_000_1.into()), 1.0f32);
        assert_ne!(
            VariableValue::Integer(16_777_217u64.into()),
            16_777_216.0f32
        );
        assert_eq!(VariableValue::Float((-0.0).into()), 0i64);
        assert_eq!(VariableValue::Float((-0.0).into()), 0u64);
        assert_eq!(
            VariableValue::Float(f64::INFINITY.into()),
            Float::from(f64::INFINITY)
        );
        assert_ne!(VariableValue::Integer(0.into()), Float::from(f64::NAN));
    }
}
