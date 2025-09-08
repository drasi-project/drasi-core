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

use crate::evaluation::variable_value::float::Float;
use core::fmt::{self, Debug, Display};
use core::hash::{Hash, Hasher};
use serde::de::{self, Deserialize, Deserializer, Visitor};
use serde_json::Number;

#[derive(Clone, PartialEq, Hash)]
pub struct Integer {
    n: N,
}

#[derive(Copy, Clone)]
enum N {
    PosInt(u64), //why do we do this?
    NegInt(i64),
}

impl PartialEq for N {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (N::PosInt(a), N::PosInt(b)) => a == b,
            (N::NegInt(a), N::NegInt(b)) => a == b,
            _ => false,
        }
    }
}

impl PartialEq<Float> for N {
    fn eq(&self, other: &Float) -> bool {
        match (self, other.as_f64()) {
            (N::PosInt(a), Some(b)) => *a as f64 == b,
            (N::NegInt(a), Some(b)) => *a as f64 == b,
            _ => false,
        }
    }
}

impl Hash for N {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match *self {
            N::PosInt(i) => i.hash(h),
            N::NegInt(i) => i.hash(h),
        }
    }
}

impl Integer {
    #[inline]
    pub fn is_i64(&self) -> bool {
        match self.n {
            N::PosInt(v) => v <= i64::MAX as u64,
            N::NegInt(_) => true,
        }
    }

    #[inline]
    pub fn is_u64(&self) -> bool {
        match self.n {
            N::PosInt(_) => true,
            N::NegInt(_) => false,
        }
    }

    #[inline]
    pub fn as_i64(&self) -> Option<i64> {
        match self.n {
            N::PosInt(n) => {
                if n <= i64::MAX as u64 {
                    Some(n as i64)
                } else {
                    None
                }
            }
            N::NegInt(n) => Some(n),
        }
    }

    #[inline]
    pub fn as_u64(&self) -> Option<u64> {
        match self.n {
            N::PosInt(n) => Some(n),
            _ => None,
        }
    }
}

impl From<Integer> for Number {
    fn from(val: Integer) -> Self {
        match val.n {
            N::PosInt(u) => Number::from(u),
            N::NegInt(i) => Number::from(i),
        }
    }
}

impl Eq for Integer {}

impl Display for Integer {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self.n {
            N::PosInt(u) => formatter.write_str(itoa::Buffer::new().format(u)),
            N::NegInt(i) => formatter.write_str(itoa::Buffer::new().format(i)),
        }
    }
}

impl Debug for Integer {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "Integer({self})")
    }
}

macro_rules! impl_from_unsigned {
    (
        $($ty:ty),*
    ) => {
        $(
            impl From<$ty> for Integer {
                #[inline]
                fn from(u: $ty) -> Self {
                    let n = {
                        { N::PosInt(u as u64) }
                    };
                    Integer { n }
                }
            }
        )*
    };
}

macro_rules! impl_from_signed {
    (
        $($ty:ty),*
    ) => {
        $(
            impl From<$ty> for Integer {
                #[inline]
                fn from(i: $ty) -> Self {
                    let n = {
                        {
                            if i < 0 {
                                N::NegInt(i as i64)
                            } else {
                                N::PosInt(i as u64)
                            }
                        }
                    };
                    Integer { n }
                }
            }
        )*
    };
}

macro_rules! impl_from_float {
    ($ty:ty) => {
        impl From<$ty> for Integer {
            fn from(n: $ty) -> Self {
                let n = {
                    if n.fract() == 0.0 {
                        N::PosInt(n as u64).into()
                    } else {
                        panic!("Invalid conversion: Cannot convert {} to Integer", n);
                    }
                };
                Integer { n }
            }
        }
    };
}

impl_from_unsigned!(u8, u16, u32, u64, usize);
impl_from_signed!(i8, i16, i32, i64, isize);
impl_from_float!(f32);
impl_from_float!(f64);

impl PartialEq<Float> for Integer {
    fn eq(&self, other: &Float) -> bool {
        match (self.n, other.as_f64()) {
            (N::PosInt(a), Some(b)) => a as f64 == b,
            (N::NegInt(a), Some(b)) => a as f64 == b,
            _ => false,
        }
    }
}

impl<'de> Deserialize<'de> for Integer {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Integer, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct NumberVisitor;

        impl Visitor<'_> for NumberVisitor {
            type Value = Integer;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON number")
            }

            #[inline]
            fn visit_i64<E>(self, value: i64) -> Result<Integer, E> {
                Ok(value.into())
            }

            #[inline]
            fn visit_u64<E>(self, value: u64) -> Result<Integer, E> {
                Ok(value.into())
            }

            #[inline]
            fn visit_f64<E>(self, value: f64) -> Result<Integer, E>
            where
                E: de::Error,
            {
                Ok(Integer::from(value))
            }
        }

        deserializer.deserialize_any(NumberVisitor)
    }
}
