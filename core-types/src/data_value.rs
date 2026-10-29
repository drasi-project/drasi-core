// Copyright 2026 The Drasi Authors.
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

//! Lossless, serializable representation of every [`VariableValue`].
//!
//! `VariableValue`'s own JSON serialization erases types or rejects values, so
//! graph and query publishers use [`DataValue`] instead. It keeps integer sign,
//! temporal zone metadata, element identities, list ranges and expression-valued
//! results. Float payloads are stored as raw IEEE-754 bits, so signed zero,
//! infinities and NaN payloads survive unchanged. This module only converts
//! data; it never evaluates expressions or parses query text.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use drasi_query_ast::ast::{
    self, BinaryExpression as Binary, Expression, Literal, UnaryExpression as Unary,
};
use serde::{Deserialize, Serialize};

use crate::{
    evaluation::variable_value::{
        duration::Duration, float::Float, integer::Integer, zoned_datetime::ZonedDateTime,
        zoned_time::ZonedTime, ListRange, RangeBound, VariableValue,
    },
    models::{Element, ElementMetadata, ElementReference},
};

/// A value that cannot be converted to or from [`DataValue`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
pub enum DataValueError {
    #[error("invalid query integer")]
    InvalidInteger,
    #[error("invalid fixed offset")]
    InvalidFixedOffset,
    #[error("invalid duration remainder")]
    InvalidDurationRemainder,
    #[error("duration is out of range")]
    DurationOutOfRange,
    #[error("invalid binary expression tag")]
    InvalidBinaryTag(u8),
    #[error("invalid unary expression tag")]
    InvalidUnaryTag(u8),
    #[error("function position {0} does not fit this platform")]
    PositionOutOfRange(u64),
}

/// Lossless data form of a [`VariableValue`].
///
/// Integers up to `u64::MAX` use `Unsigned`; only negative integers use
/// `Signed`. `Float` holds [`Float::to_bits`] verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataValue {
    Null,
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    /// Raw IEEE-754 bits, including signed zero, infinities and NaN payloads.
    Float(u64),
    String(String),
    List(Vec<DataValue>),
    Object(BTreeMap<String, DataValue>),
    Date(NaiveDate),
    LocalTime(NaiveTime),
    /// Local time and its offset in seconds east of UTC.
    ZonedTime(NaiveTime, i32),
    LocalDateTime(NaiveDateTime),
    /// Instant plus the optional original timezone name.
    ZonedDateTime(DateTime<FixedOffset>, Option<String>),
    Duration {
        seconds: i64,
        nanos: i32,
        years: i64,
        months: i64,
    },
    /// List range bounds; `None` is unbounded.
    Range(Option<i64>, Option<i64>),
    Element(Element),
    Metadata(ElementMetadata),
    Reference(ElementReference),
    Expression(DataExpression),
    Awaiting,
}

impl DataValue {
    /// Convert a query value into its lossless data form.
    pub fn encode(value: &VariableValue) -> Result<Self, DataValueError> {
        Ok(match value {
            VariableValue::Null => Self::Null,
            VariableValue::Bool(value) => Self::Bool(*value),
            VariableValue::Integer(value) => {
                if let Some(value) = value.as_u64() {
                    Self::Unsigned(value)
                } else {
                    Self::Signed(value.as_i64().ok_or(DataValueError::InvalidInteger)?)
                }
            }
            VariableValue::Float(value) => Self::Float(value.to_bits()),
            VariableValue::String(value) => Self::String(value.clone()),
            VariableValue::List(values) => {
                Self::List(values.iter().map(Self::encode).collect::<Result<_, _>>()?)
            }
            VariableValue::Object(values) => Self::Object(encode_variables(values)?),
            VariableValue::Date(value) => Self::Date(*value),
            VariableValue::LocalTime(value) => Self::LocalTime(*value),
            VariableValue::ZonedTime(value) => {
                Self::ZonedTime(*value.time(), value.offset().local_minus_utc())
            }
            VariableValue::LocalDateTime(value) => Self::LocalDateTime(*value),
            VariableValue::ZonedDateTime(value) => {
                Self::ZonedDateTime(*value.datetime(), value.timezone_name().clone())
            }
            VariableValue::Duration(value) => Self::Duration {
                seconds: value.duration().num_seconds(),
                nanos: value.duration().subsec_nanos(),
                years: *value.year(),
                months: *value.month(),
            },
            VariableValue::ListRange(value) => {
                let bound = |bound: &RangeBound| match bound {
                    RangeBound::Index(value) => Some(*value),
                    RangeBound::Unbounded => None,
                };
                Self::Range(bound(&value.start), bound(&value.end))
            }
            VariableValue::Element(value) => Self::Element(value.as_ref().clone()),
            VariableValue::ElementMetadata(value) => Self::Metadata(value.clone()),
            VariableValue::ElementReference(value) => Self::Reference(value.clone()),
            VariableValue::Expression(value) => Self::Expression(DataExpression::encode(value)),
            VariableValue::Awaiting => Self::Awaiting,
        })
    }

    /// Rebuild the query value, rejecting values that no valid query value can hold.
    pub fn decode(self) -> Result<VariableValue, DataValueError> {
        Ok(match self {
            Self::Null => VariableValue::Null,
            Self::Bool(value) => VariableValue::Bool(value),
            Self::Signed(value) => VariableValue::Integer(Integer::from(value)),
            Self::Unsigned(value) => VariableValue::Integer(Integer::from(value)),
            Self::Float(bits) => VariableValue::Float(Float::from(f64::from_bits(bits))),
            Self::String(value) => VariableValue::String(value),
            Self::List(values) => VariableValue::List(
                values
                    .into_iter()
                    .map(Self::decode)
                    .collect::<Result<_, _>>()?,
            ),
            Self::Object(values) => VariableValue::Object(decode_variables(values)?),
            Self::Date(value) => VariableValue::Date(value),
            Self::LocalTime(value) => VariableValue::LocalTime(value),
            Self::ZonedTime(value, offset) => VariableValue::ZonedTime(ZonedTime::new(
                value,
                FixedOffset::east_opt(offset).ok_or(DataValueError::InvalidFixedOffset)?,
            )),
            Self::LocalDateTime(value) => VariableValue::LocalDateTime(value),
            Self::ZonedDateTime(value, zone) => {
                VariableValue::ZonedDateTime(ZonedDateTime::new(value, zone))
            }
            Self::Duration {
                seconds,
                nanos,
                years,
                months,
            } => {
                if !(-999_999_999..=999_999_999).contains(&nanos) {
                    return Err(DataValueError::InvalidDurationRemainder);
                }
                let duration = chrono::Duration::try_seconds(seconds)
                    .and_then(|duration| {
                        duration.checked_add(&chrono::Duration::nanoseconds(i64::from(nanos)))
                    })
                    .ok_or(DataValueError::DurationOutOfRange)?;
                VariableValue::Duration(Duration::new(duration, years, months))
            }
            Self::Range(start, end) => VariableValue::ListRange(ListRange {
                start: start
                    .map(RangeBound::Index)
                    .unwrap_or(RangeBound::Unbounded),
                end: end.map(RangeBound::Index).unwrap_or(RangeBound::Unbounded),
            }),
            Self::Element(value) => VariableValue::Element(Arc::new(value)),
            Self::Metadata(value) => VariableValue::ElementMetadata(value),
            Self::Reference(value) => VariableValue::ElementReference(value),
            Self::Expression(value) => VariableValue::Expression(value.decode()?),
            Self::Awaiting => VariableValue::Awaiting,
        })
    }
}

impl TryFrom<&VariableValue> for DataValue {
    type Error = DataValueError;

    fn try_from(value: &VariableValue) -> Result<Self, Self::Error> {
        Self::encode(value)
    }
}

impl TryFrom<DataValue> for VariableValue {
    type Error = DataValueError;

    fn try_from(value: DataValue) -> Result<Self, Self::Error> {
        value.decode()
    }
}

/// Convert a named set of query values, such as a result row or property map.
pub fn encode_variables<K: AsRef<str>>(
    values: &BTreeMap<K, VariableValue>,
) -> Result<BTreeMap<String, DataValue>, DataValueError> {
    values
        .iter()
        .map(|(key, value)| Ok((key.as_ref().to_owned(), DataValue::encode(value)?)))
        .collect()
}

/// Rebuild a named set of query values encoded by [`encode_variables`].
pub fn decode_variables<K: From<String> + Ord>(
    values: BTreeMap<String, DataValue>,
) -> Result<BTreeMap<K, VariableValue>, DataValueError> {
    values
        .into_iter()
        .map(|(key, value)| Ok((key.into(), value.decode()?)))
        .collect()
}

/// Data form of a query AST expression carried by an expression-valued result.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataExpression {
    Unary(DataUnary),
    /// Operator tag and both operands. Tags 0..=19 are And, Or, Eq, Ne, Lt, Le, Gt, Ge,
    /// In, Add, Subtract, Multiply, Divide, Modulo, Exponent, HasLabel, Index,
    /// StartsWith, EndsWith, Contains.
    Binary(u8, Box<DataExpression>, Box<DataExpression>),
    /// Function name, arguments and the function's position in the query.
    Function(String, Vec<DataExpression>, u64),
    Case(
        Option<Box<DataExpression>>,
        Vec<(DataExpression, DataExpression)>,
        Option<Box<DataExpression>>,
    ),
    List(Vec<DataExpression>),
    Object(BTreeMap<String, DataExpression>),
    Iterator(
        String,
        Box<DataExpression>,
        Option<Box<DataExpression>>,
        Option<Box<DataExpression>>,
    ),
}

/// Data form of a unary query AST expression.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataUnary {
    /// Tag 0 `Not`, 1 `Exists`, 2 `IsNull`, 3 `IsNotNull`.
    Operator(u8, Box<DataExpression>),
    Literal(DataLiteral),
    Property(String, String),
    ExpressionProperty(Box<DataExpression>, String),
    Parameter(String),
    Identifier(String),
    Variable(String, Box<DataExpression>),
    Alias(Box<DataExpression>, String),
    Range(Option<Box<DataExpression>>, Option<Box<DataExpression>>),
}

/// Data form of a query AST literal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataLiteral {
    Integer(i64),
    /// Raw IEEE-754 bits of the real literal.
    Real(u64),
    Boolean(bool),
    Text(String),
    Date(String),
    LocalTime(String),
    ZonedTime(String),
    LocalDateTime(String),
    ZonedDateTime(String),
    Duration(String),
    Object(Vec<(String, DataLiteral)>),
    Expression(Box<DataExpression>),
    Null,
}

fn encoded_optional(value: Option<&Expression>) -> Option<Box<DataExpression>> {
    value.map(|value| Box::new(DataExpression::encode(value)))
}

fn decoded_optional(
    value: Option<Box<DataExpression>>,
) -> Result<Option<Box<Expression>>, DataValueError> {
    value.map(|value| value.decode().map(Box::new)).transpose()
}

macro_rules! binary_tags {
    ($apply:ident!($($prefix:tt)*)) => {
        $apply!($($prefix)*
            0=>And,1=>Or,2=>Eq,3=>Ne,4=>Lt,5=>Le,6=>Gt,7=>Ge,8=>In,9=>Add,
            10=>Subtract,11=>Multiply,12=>Divide,13=>Modulo,14=>Exponent,
            15=>HasLabel,16=>Index,17=>StartsWith,18=>EndsWith,19=>Contains)
    };
}
macro_rules! binary_parts {
    ($value:expr; $($tag:literal => $name:ident),+ $(,)?) => {
        match $value { $(Binary::$name(left, right) => ($tag, left, right)),+ }
    };
}
macro_rules! binary_from {
    ($tag:expr, $left:expr, $right:expr; $($number:literal => $name:ident),+ $(,)?) => {
        match $tag {
            $($number => Binary::$name($left, $right)),+,
            tag => return Err(DataValueError::InvalidBinaryTag(tag)),
        }
    };
}

impl DataExpression {
    /// Convert a query expression into its data form.
    pub fn encode(value: &Expression) -> Self {
        match value {
            Expression::UnaryExpression(value) => Self::Unary(DataUnary::encode(value)),
            Expression::BinaryExpression(value) => {
                let (tag, left, right) = binary_tags!(binary_parts!(value;));
                Self::Binary(
                    tag,
                    Box::new(Self::encode(left)),
                    Box::new(Self::encode(right)),
                )
            }
            Expression::FunctionExpression(value) => Self::Function(
                value.name.to_string(),
                value.args.iter().map(Self::encode).collect(),
                value.position_in_query as u64,
            ),
            Expression::CaseExpression(value) => Self::Case(
                encoded_optional(value.match_.as_deref()),
                value
                    .when
                    .iter()
                    .map(|(left, right)| (Self::encode(left), Self::encode(right)))
                    .collect(),
                encoded_optional(value.else_.as_deref()),
            ),
            Expression::ListExpression(value) => {
                Self::List(value.elements.iter().map(Self::encode).collect())
            }
            Expression::ObjectExpression(value) => Self::Object(
                value
                    .elements
                    .iter()
                    .map(|(key, value)| (key.to_string(), Self::encode(value)))
                    .collect(),
            ),
            Expression::IteratorExpression(value) => Self::Iterator(
                value.item_identifier.to_string(),
                Box::new(Self::encode(&value.list_expression)),
                encoded_optional(value.filter.as_deref()),
                encoded_optional(value.map_expression.as_deref()),
            ),
        }
    }

    /// Rebuild the query expression, rejecting unknown operator tags.
    pub fn decode(self) -> Result<Expression, DataValueError> {
        Ok(match self {
            Self::Unary(value) => Expression::UnaryExpression(value.decode()?),
            Self::Binary(tag, left, right) => {
                let left = Box::new(left.decode()?);
                let right = Box::new(right.decode()?);
                Expression::BinaryExpression(binary_tags!(binary_from!(tag, left, right;)))
            }
            Self::Function(name, args, position) => {
                Expression::FunctionExpression(ast::FunctionExpression {
                    name: Arc::from(name),
                    args: args
                        .into_iter()
                        .map(Self::decode)
                        .collect::<Result<_, _>>()?,
                    position_in_query: usize::try_from(position)
                        .map_err(|_| DataValueError::PositionOutOfRange(position))?,
                })
            }
            Self::Case(match_, when, else_) => Expression::CaseExpression(ast::CaseExpression {
                match_: decoded_optional(match_)?,
                else_: decoded_optional(else_)?,
                when: when
                    .into_iter()
                    .map(|(left, right)| Ok((left.decode()?, right.decode()?)))
                    .collect::<Result<_, DataValueError>>()?,
            }),
            Self::List(elements) => Expression::ListExpression(ast::ListExpression {
                elements: elements
                    .into_iter()
                    .map(Self::decode)
                    .collect::<Result<_, _>>()?,
            }),
            Self::Object(elements) => Expression::ObjectExpression(ast::ObjectExpression {
                elements: elements
                    .into_iter()
                    .map(|(key, value)| Ok((Arc::from(key), value.decode()?)))
                    .collect::<Result<_, DataValueError>>()?,
            }),
            Self::Iterator(item, list, filter, map) => {
                Expression::IteratorExpression(ast::IteratorExpression {
                    item_identifier: Arc::from(item),
                    list_expression: Box::new(list.decode()?),
                    filter: decoded_optional(filter)?,
                    map_expression: decoded_optional(map)?,
                })
            }
        })
    }
}

impl DataUnary {
    fn encode(value: &Unary) -> Self {
        match value {
            Unary::Not(value) => Self::Operator(0, Box::new(DataExpression::encode(value))),
            Unary::Exists(value) => Self::Operator(1, Box::new(DataExpression::encode(value))),
            Unary::IsNull(value) => Self::Operator(2, Box::new(DataExpression::encode(value))),
            Unary::IsNotNull(value) => Self::Operator(3, Box::new(DataExpression::encode(value))),
            Unary::Literal(value) => Self::Literal(DataLiteral::encode(value)),
            Unary::Property { name, key } => Self::Property(name.to_string(), key.to_string()),
            Unary::ExpressionProperty { exp, key } => {
                Self::ExpressionProperty(Box::new(DataExpression::encode(exp)), key.to_string())
            }
            Unary::Parameter(value) => Self::Parameter(value.to_string()),
            Unary::Identifier(value) => Self::Identifier(value.to_string()),
            Unary::Variable { name, value } => {
                Self::Variable(name.to_string(), Box::new(DataExpression::encode(value)))
            }
            Unary::Alias { source, alias } => {
                Self::Alias(Box::new(DataExpression::encode(source)), alias.to_string())
            }
            Unary::ListRange {
                start_bound,
                end_bound,
            } => Self::Range(
                encoded_optional(start_bound.as_deref()),
                encoded_optional(end_bound.as_deref()),
            ),
        }
    }

    fn decode(self) -> Result<Unary, DataValueError> {
        Ok(match self {
            Self::Operator(tag, value) => {
                let value = Box::new(value.decode()?);
                match tag {
                    0 => Unary::Not(value),
                    1 => Unary::Exists(value),
                    2 => Unary::IsNull(value),
                    3 => Unary::IsNotNull(value),
                    tag => return Err(DataValueError::InvalidUnaryTag(tag)),
                }
            }
            Self::Literal(value) => Unary::Literal(value.decode()?),
            Self::Property(name, key) => Unary::Property {
                name: Arc::from(name),
                key: Arc::from(key),
            },
            Self::ExpressionProperty(exp, key) => Unary::ExpressionProperty {
                exp: Box::new(exp.decode()?),
                key: Arc::from(key),
            },
            Self::Parameter(value) => Unary::Parameter(Arc::from(value)),
            Self::Identifier(value) => Unary::Identifier(Arc::from(value)),
            Self::Variable(name, value) => Unary::Variable {
                name: Arc::from(name),
                value: Box::new(value.decode()?),
            },
            Self::Alias(source, alias) => Unary::Alias {
                source: Box::new(source.decode()?),
                alias: Arc::from(alias),
            },
            Self::Range(start_bound, end_bound) => Unary::ListRange {
                start_bound: decoded_optional(start_bound)?,
                end_bound: decoded_optional(end_bound)?,
            },
        })
    }
}

impl DataLiteral {
    fn encode(value: &Literal) -> Self {
        match value {
            Literal::Integer(value) => Self::Integer(*value),
            Literal::Real(value) => Self::Real(value.to_bits()),
            Literal::Boolean(value) => Self::Boolean(*value),
            Literal::Text(value) => Self::Text(value.to_string()),
            Literal::Date(value) => Self::Date(value.to_string()),
            Literal::LocalTime(value) => Self::LocalTime(value.to_string()),
            Literal::ZonedTime(value) => Self::ZonedTime(value.to_string()),
            Literal::LocalDateTime(value) => Self::LocalDateTime(value.to_string()),
            Literal::ZonedDateTime(value) => Self::ZonedDateTime(value.to_string()),
            Literal::Duration(value) => Self::Duration(value.to_string()),
            Literal::Object(values) => Self::Object(
                values
                    .iter()
                    .map(|(key, value)| (key.to_string(), Self::encode(value)))
                    .collect(),
            ),
            Literal::Expression(value) => Self::Expression(Box::new(DataExpression::encode(value))),
            Literal::Null => Self::Null,
        }
    }

    fn decode(self) -> Result<Literal, DataValueError> {
        Ok(match self {
            Self::Integer(value) => Literal::Integer(value),
            Self::Real(value) => Literal::Real(f64::from_bits(value)),
            Self::Boolean(value) => Literal::Boolean(value),
            Self::Text(value) => Literal::Text(Arc::from(value)),
            Self::Date(value) => Literal::Date(Arc::from(value)),
            Self::LocalTime(value) => Literal::LocalTime(Arc::from(value)),
            Self::ZonedTime(value) => Literal::ZonedTime(Arc::from(value)),
            Self::LocalDateTime(value) => Literal::LocalDateTime(Arc::from(value)),
            Self::ZonedDateTime(value) => Literal::ZonedDateTime(Arc::from(value)),
            Self::Duration(value) => Literal::Duration(Arc::from(value)),
            Self::Object(values) => Literal::Object(
                values
                    .into_iter()
                    .map(|(key, value)| Ok((Arc::from(key), value.decode()?)))
                    .collect::<Result<_, DataValueError>>()?,
            ),
            Self::Expression(value) => Literal::Expression(Box::new(value.decode()?)),
            Self::Null => Literal::Null,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ElementPropertyMap;
    use chrono::{TimeZone, Utc};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn round_trip(value: &VariableValue) -> Result<VariableValue, Box<dyn std::error::Error>> {
        let bytes = rmp_serde::to_vec(&DataValue::encode(value)?)?;
        Ok(rmp_serde::from_slice::<DataValue>(&bytes)?.decode()?)
    }

    fn assert_stable(value: &VariableValue) -> TestResult {
        let encoded = DataValue::encode(value)?;
        let decoded = round_trip(value)?;
        assert_eq!(DataValue::encode(&decoded)?, encoded);
        Ok(())
    }

    fn float_bits(value: &VariableValue) -> Option<u64> {
        match value {
            VariableValue::Float(value) => Some(value.to_bits()),
            _ => None,
        }
    }

    fn reference() -> ElementReference {
        ElementReference::new("source", "element")
    }

    fn metadata() -> ElementMetadata {
        ElementMetadata {
            reference: reference(),
            labels: Arc::from(vec![Arc::<str>::from("Label")]),
            effective_from: 42,
        }
    }

    fn literal(value: Literal) -> Box<Expression> {
        Box::new(Expression::UnaryExpression(Unary::Literal(value)))
    }

    #[test]
    fn integer_boundaries_keep_sign_and_magnitude() -> TestResult {
        for value in [i64::MIN, -1, 0, 1, i64::MAX] {
            let decoded = round_trip(&VariableValue::Integer(Integer::from(value)))?;
            assert!(matches!(&decoded, VariableValue::Integer(i) if i.as_i64() == Some(value)));
        }
        let decoded = round_trip(&VariableValue::Integer(Integer::from(u64::MAX)))?;
        assert!(matches!(&decoded, VariableValue::Integer(i) if i.as_u64() == Some(u64::MAX)));
        assert_eq!(
            DataValue::encode(&VariableValue::Integer(Integer::from(i64::MIN)))?,
            DataValue::Signed(i64::MIN)
        );
        assert_eq!(
            DataValue::encode(&VariableValue::Integer(Integer::from(i64::MAX)))?,
            DataValue::Unsigned(i64::MAX as u64)
        );
        Ok(())
    }

    #[test]
    fn float_bit_patterns_are_preserved() -> TestResult {
        for bits in [
            0.0_f64.to_bits(),
            (-0.0_f64).to_bits(),
            1.25_f64.to_bits(),
            f64::MIN_POSITIVE.to_bits(),
            f64::MAX.to_bits(),
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            0x7ff8_0000_0000_0042,
            0xfff8_0000_0000_0081,
        ] {
            let value = VariableValue::Float(Float::from(f64::from_bits(bits)));
            assert_eq!(DataValue::encode(&value)?, DataValue::Float(bits));
            assert_eq!(float_bits(&round_trip(&value)?), Some(bits));
        }
        Ok(())
    }

    #[test]
    fn wire_equality_and_hash_are_bitwise() {
        use std::hash::{BuildHasher, RandomState};

        let state = RandomState::new();
        let nan = DataValue::Float(0x7ff8_0000_0000_0042);
        assert_eq!(nan, nan.clone());
        assert_eq!(state.hash_one(&nan), state.hash_one(nan.clone()));
        assert_ne!(
            DataValue::Float(0.0_f64.to_bits()),
            DataValue::Float((-0.0_f64).to_bits())
        );
        assert_ne!(DataValue::Signed(-1), DataValue::Unsigned(u64::MAX));
    }

    #[test]
    fn scalars_and_temporals_round_trip() -> TestResult {
        let date = NaiveDate::from_ymd_opt(2026, 9, 8).ok_or("date")?;
        let time = NaiveTime::from_hms_nano_opt(1, 2, 3, 456).ok_or("time")?;
        let offset = FixedOffset::east_opt(-5 * 3600 - 1800).ok_or("offset")?;
        let instant = offset
            .from_local_datetime(&NaiveDateTime::new(date, time))
            .single()
            .ok_or("instant")?;
        let values = [
            VariableValue::Null,
            VariableValue::Bool(true),
            VariableValue::String("text".into()),
            VariableValue::Date(date),
            VariableValue::LocalTime(time),
            VariableValue::ZonedTime(ZonedTime::new(time, offset)),
            VariableValue::LocalDateTime(NaiveDateTime::new(date, time)),
            VariableValue::ZonedDateTime(ZonedDateTime::new(instant, Some("America/Lima".into()))),
            VariableValue::ZonedDateTime(ZonedDateTime::new(
                Utc.timestamp_opt(0, 0).single().ok_or("utc")?.into(),
                None,
            )),
            VariableValue::Duration(Duration::new(
                chrono::Duration::try_seconds(-3)
                    .and_then(|duration| duration.checked_add(&chrono::Duration::nanoseconds(-7)))
                    .ok_or("duration")?,
                -2,
                11,
            )),
            VariableValue::Awaiting,
        ];
        for value in &values {
            assert_stable(value)?;
        }
        match round_trip(&values[5])? {
            VariableValue::ZonedTime(value) => {
                assert_eq!(value.offset().local_minus_utc(), -5 * 3600 - 1800);
                assert_eq!(*value.time(), time);
            }
            other => panic!("unexpected {other:?}"),
        }
        match round_trip(&values[7])? {
            VariableValue::ZonedDateTime(value) => {
                assert_eq!(*value.datetime(), instant);
                assert_eq!(value.timezone_name().as_deref(), Some("America/Lima"));
            }
            other => panic!("unexpected {other:?}"),
        }
        match round_trip(&values[9])? {
            VariableValue::Duration(value) => {
                assert_eq!(value.duration().num_seconds(), -3);
                assert_eq!(value.duration().subsec_nanos(), -7);
                assert_eq!((*value.year(), *value.month()), (-2, 11));
            }
            other => panic!("unexpected {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn nested_collections_round_trip() -> TestResult {
        let mut inner = BTreeMap::new();
        inner.insert(
            "nan".to_string(),
            VariableValue::Float(Float::from(f64::from_bits(0x7ff8_0000_0000_0042))),
        );
        inner.insert("neg".to_string(), VariableValue::Integer(Integer::from(-9)));
        let mut outer = BTreeMap::new();
        outer.insert(
            "list".to_string(),
            VariableValue::List(vec![
                VariableValue::Object(inner),
                VariableValue::List(vec![VariableValue::Null, VariableValue::Awaiting]),
            ]),
        );
        let value = VariableValue::Object(outer);
        assert_stable(&value)?;
        let decoded = round_trip(&value)?;
        let nan = decoded
            .pointer("/list/0/nan")
            .and_then(float_bits)
            .ok_or("nested float")?;
        assert_eq!(nan, 0x7ff8_0000_0000_0042);
        Ok(())
    }

    #[test]
    fn variables_helpers_round_trip() -> TestResult {
        let mut values = BTreeMap::new();
        values.insert("a".to_string(), VariableValue::Bool(false));
        values.insert(
            "b".to_string(),
            VariableValue::Integer(Integer::from(u64::MAX)),
        );
        let encoded = encode_variables(&values)?;
        let bytes = rmp_serde::to_vec(&encoded)?;
        let decoded: BTreeMap<String, VariableValue> =
            decode_variables(rmp_serde::from_slice(&bytes)?)?;
        assert_eq!(decoded, values);
        Ok(())
    }

    #[test]
    fn elements_and_references_round_trip() -> TestResult {
        let mut properties = ElementPropertyMap::new();
        properties.insert("name", crate::models::ElementValue::String("n".into()));
        let node = Element::Node {
            metadata: metadata(),
            properties: properties.clone(),
        };
        let relation = Element::Relation {
            metadata: metadata(),
            in_node: ElementReference::new("source", "in"),
            out_node: ElementReference::new("source", "out"),
            properties,
        };
        for element in [node, relation] {
            let value = VariableValue::Element(Arc::new(element.clone()));
            assert!(
                matches!(round_trip(&value)?, VariableValue::Element(decoded) if *decoded == element)
            );
        }
        assert!(matches!(
            round_trip(&VariableValue::ElementMetadata(metadata()))?,
            VariableValue::ElementMetadata(decoded) if decoded == metadata()
        ));
        assert!(matches!(
            round_trip(&VariableValue::ElementReference(reference()))?,
            VariableValue::ElementReference(decoded) if decoded == reference()
        ));
        Ok(())
    }

    #[test]
    fn ranges_round_trip() -> TestResult {
        for (start, end) in [
            (RangeBound::Index(-2), RangeBound::Index(5)),
            (RangeBound::Unbounded, RangeBound::Index(0)),
            (RangeBound::Index(i64::MIN), RangeBound::Unbounded),
            (RangeBound::Unbounded, RangeBound::Unbounded),
        ] {
            let range = ListRange { start, end };
            assert!(matches!(
                round_trip(&VariableValue::ListRange(range.clone()))?,
                VariableValue::ListRange(decoded) if decoded == range
            ));
        }
        Ok(())
    }

    fn sample_expression() -> Expression {
        let property = Expression::UnaryExpression(Unary::Property {
            name: "n".into(),
            key: "age".into(),
        });
        let add = |left: Expression, right: Expression| {
            Expression::BinaryExpression(Binary::Add(Box::new(left), Box::new(right)))
        };
        let mut object = BTreeMap::new();
        object.insert(
            Arc::<str>::from("k"),
            add(property.clone(), *literal(Literal::Integer(-1))),
        );
        Expression::ListExpression(ast::ListExpression {
            elements: vec![
                Expression::FunctionExpression(ast::FunctionExpression {
                    name: "abs".into(),
                    args: vec![property.clone()],
                    position_in_query: 7,
                }),
                Expression::CaseExpression(ast::CaseExpression {
                    match_: Some(Box::new(property.clone())),
                    when: vec![(*literal(Literal::Boolean(true)), *literal(Literal::Null))],
                    else_: Some(literal(Literal::Text("x".into()))),
                }),
                Expression::ObjectExpression(ast::ObjectExpression { elements: object }),
                Expression::IteratorExpression(ast::IteratorExpression {
                    item_identifier: "i".into(),
                    list_expression: Box::new(Expression::UnaryExpression(Unary::Identifier(
                        "xs".into(),
                    ))),
                    filter: Some(Box::new(Expression::UnaryExpression(Unary::Not(Box::new(
                        property.clone(),
                    ))))),
                    map_expression: None,
                }),
                Expression::UnaryExpression(Unary::ListRange {
                    start_bound: Some(literal(Literal::Integer(1))),
                    end_bound: None,
                }),
                Expression::UnaryExpression(Unary::Alias {
                    source: Box::new(property),
                    alias: "a".into(),
                }),
                Expression::UnaryExpression(Unary::Literal(Literal::Object(vec![(
                    "nested".into(),
                    Literal::Expression(literal(Literal::Duration("PT1S".into()))),
                )]))),
            ],
        })
    }

    #[test]
    fn expression_values_round_trip() -> TestResult {
        let expression = sample_expression();
        let value = VariableValue::Expression(expression.clone());
        assert_stable(&value)?;
        assert!(matches!(
            round_trip(&value)?,
            VariableValue::Expression(decoded) if decoded == expression
        ));
        Ok(())
    }

    #[test]
    fn every_binary_tag_round_trips() -> TestResult {
        for tag in 0..=19_u8 {
            let encoded = DataExpression::Binary(
                tag,
                Box::new(DataExpression::Unary(DataUnary::Literal(DataLiteral::Null))),
                Box::new(DataExpression::Unary(DataUnary::Literal(DataLiteral::Null))),
            );
            assert_eq!(DataExpression::encode(&encoded.clone().decode()?), encoded);
        }
        Ok(())
    }

    #[test]
    fn real_literal_keeps_nonfinite_bits() -> TestResult {
        for bits in [
            (-0.0_f64).to_bits(),
            f64::INFINITY.to_bits(),
            0x7ff8_0000_0000_0042,
        ] {
            let expression = *literal(Literal::Real(f64::from_bits(bits)));
            assert_eq!(
                DataExpression::encode(&expression),
                DataExpression::Unary(DataUnary::Literal(DataLiteral::Real(bits)))
            );
            let decoded = DataExpression::encode(&expression).decode()?;
            assert!(matches!(
                decoded,
                Expression::UnaryExpression(Unary::Literal(Literal::Real(value))) if value.to_bits() == bits
            ));
        }
        Ok(())
    }

    #[test]
    fn invalid_data_is_rejected() {
        assert_eq!(
            DataValue::ZonedTime(NaiveTime::MIN, 86_400).decode().err(),
            Some(DataValueError::InvalidFixedOffset)
        );
        for nanos in [1_000_000_000, -1_000_000_000] {
            assert_eq!(
                DataValue::Duration {
                    seconds: 0,
                    nanos,
                    years: 0,
                    months: 0
                }
                .decode()
                .err(),
                Some(DataValueError::InvalidDurationRemainder)
            );
        }
        assert_eq!(
            DataValue::Duration {
                seconds: i64::MAX,
                nanos: 0,
                years: 0,
                months: 0
            }
            .decode()
            .err(),
            Some(DataValueError::DurationOutOfRange)
        );
        let null = || Box::new(DataExpression::Unary(DataUnary::Literal(DataLiteral::Null)));
        assert_eq!(
            DataValue::Expression(DataExpression::Binary(20, null(), null()))
                .decode()
                .err(),
            Some(DataValueError::InvalidBinaryTag(20))
        );
        assert_eq!(
            DataValue::Expression(DataExpression::Unary(DataUnary::Operator(4, null())))
                .decode()
                .err(),
            Some(DataValueError::InvalidUnaryTag(4))
        );
        let nested = DataValue::List(vec![DataValue::Object(BTreeMap::from([(
            "bad".to_string(),
            DataValue::ZonedTime(NaiveTime::MIN, i32::MAX),
        )]))]);
        assert_eq!(
            nested.decode().err(),
            Some(DataValueError::InvalidFixedOffset)
        );
    }
}
