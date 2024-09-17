use crate::evaluation::variable_value::integer::Integer;
use core::fmt::{self, Debug, Display};
use core::hash::{Hash, Hasher};
use serde::de::{self, Deserialize, Deserializer, Visitor};
use serde_json::Number;

#[derive(Clone, Default)]
pub struct Float {
    value: f64,
}

impl PartialEq for Float {
    fn eq(&self, other: &Self) -> bool {
        let (a, b) = (self.value, other.value);
        a == b
    }
}

impl PartialEq<f64> for Float {
    fn eq(&self, other: &f64) -> bool {
        let (a, b) = (self.value, *other);
        a == b
    }
}

impl PartialEq<Integer> for Float {
    fn eq(&self, other: &Integer) -> bool {
        match (self.value, other.as_i64()) {
            (a, Some(b)) => a == b as f64,
            _ => false,
        }
    }
}

impl Eq for Float {}

impl Hash for Float {
    fn hash<H: Hasher>(&self, h: &mut H) {
        match self.value {
            f if f == 0.0f64 => {
                // There are 2 zero representations, +0 and -0, which
                // compare equal but have different bits. We use the +0 hash
                // for both so that hash(+0) == hash(-0).
                0.0f64.to_bits().hash(h);
            }
            f => {
                f.to_bits().hash(h);
            }
        }
    }
}

impl Float {
    #[inline]
    pub fn is_f64(&self) -> bool {
        self.value.is_finite()
    }

    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self.value {
            n if n.is_finite() => Some(n),
            _ => None,
        }
    }

    pub(crate) fn as_f32(&self) -> Option<f32> {
        match self.value {
            n if n.is_finite() => Some(n as f32),
            _ => None,
        }
    }

    #[inline]
    pub fn from_f64(f: f64) -> Option<Float> {
        if f.is_finite() {
            Some(Float { value: f })
        } else {
            None
        }
    }

    #[inline]
    pub(crate) fn from_f32(f: f32) -> Option<Float> {
        if f.is_finite() {
            Some(Float { value: f as f64 })
        } else {
            None
        }
    }
}

impl From<Float> for Number {
    fn from(val: Float) -> Self {
        Number::from_f64(val.value).unwrap()
    }
}

impl Debug for Float {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "Float({})", self)
    }
}

impl Display for Float {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        {
            let value = &self.value;
            formatter.write_str(&value.to_string())
        }
    }
}

impl<'de> Deserialize<'de> for Float {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Float, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct NumberVisitor;

        impl<'de> Visitor<'de> for NumberVisitor {
            type Value = Float;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON number")
            }

            #[inline]
            fn visit_f64<E>(self, value: f64) -> Result<Float, E>
            where
                E: de::Error,
            {
                Ok(Float::from(value))
            }

            #[inline]
            fn visit_f32<E>(self, value: f32) -> Result<Float, E>
            where
                E: de::Error,
            {
                Ok(Float::from(value))
            }
        }

        deserializer.deserialize_any(NumberVisitor)
    }
}

macro_rules! impl_from_float {
    (
        $($ty:ty),*
    ) => {
        $(
            impl From<$ty> for Float {
                #[inline]
                fn from(i: $ty) -> Self {
                    let n = {
                        {
                            i as f64
                        }
                    };
                    Float { value: n }
                }
            }
        )*
    };
}

impl_from_float!(f32, f64, i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
