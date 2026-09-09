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

//! Lossless graph-owned storage for query values. Legacy VariableValue JSON
//! serialization is intentionally not reused: it erases types or rejects values.

use std::{collections::BTreeMap, sync::Arc};

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use drasi_core::{
    evaluation::variable_value::{
        duration::Duration, float::Float, integer::Integer, zoned_datetime::ZonedDateTime,
        zoned_time::ZonedTime, ListRange, RangeBound, VariableValue,
    },
    models::{Element, ElementMetadata, ElementReference},
};
use drasi_query_ast::ast::{
    self, BinaryExpression as Binary, Expression, Literal, UnaryExpression as Unary,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(u64),
    String(String),
    List(Vec<Value>),
    Object(BTreeMap<String, Value>),
    Date(NaiveDate),
    LocalTime(NaiveTime),
    ZonedTime(NaiveTime, i32),
    LocalDateTime(NaiveDateTime),
    ZonedDateTime(DateTime<FixedOffset>, Option<String>),
    Duration {
        seconds: i64,
        nanos: i32,
        years: i64,
        months: i64,
    },
    Range(Option<i64>, Option<i64>),
    Element(Element),
    Metadata(ElementMetadata),
    Reference(ElementReference),
    Expression(Expr),
    Awaiting,
}

impl Value {
    pub(crate) fn encode(value: &VariableValue) -> anyhow::Result<Self> {
        Ok(match value {
            VariableValue::Null => Self::Null,
            VariableValue::Bool(value) => Self::Bool(*value),
            VariableValue::Integer(value) => {
                if let Some(value) = value.as_u64() {
                    Self::Unsigned(value)
                } else {
                    Self::Signed(
                        value
                            .as_i64()
                            .ok_or_else(|| anyhow::anyhow!("invalid query integer"))?,
                    )
                }
            }
            VariableValue::Float(value) => {
                // Display round-trips finite values including negative zero. The
                // existing A2 single-u64 projection retains nonfinite NaN payloads.
                let bits = if value.is_f64() {
                    value.to_string().parse::<f64>()?.to_bits()
                } else {
                    super::canonical::query_float_bits(value)?
                };
                Self::Float(bits)
            }
            VariableValue::String(value) => Self::String(value.clone()),
            VariableValue::List(values) => Self::List(
                values
                    .iter()
                    .map(Self::encode)
                    .collect::<anyhow::Result<_>>()?,
            ),
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
            VariableValue::Expression(value) => Self::Expression(Expr::encode(value)?),
            VariableValue::Awaiting => Self::Awaiting,
        })
    }

    pub(crate) fn decode(self) -> anyhow::Result<VariableValue> {
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
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::Object(values) => VariableValue::Object(decode_variables(values)?),
            Self::Date(value) => VariableValue::Date(value),
            Self::LocalTime(value) => VariableValue::LocalTime(value),
            Self::ZonedTime(value, offset) => VariableValue::ZonedTime(ZonedTime::new(
                value,
                FixedOffset::east_opt(offset)
                    .ok_or_else(|| anyhow::anyhow!("invalid fixed offset"))?,
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
                    anyhow::bail!("invalid duration remainder");
                }
                let duration = chrono::Duration::try_seconds(seconds)
                    .and_then(|duration| {
                        duration.checked_add(&chrono::Duration::nanoseconds(i64::from(nanos)))
                    })
                    .ok_or_else(|| anyhow::anyhow!("duration is out of range"))?;
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

pub(crate) fn encode_variables<K: AsRef<str>>(
    values: &BTreeMap<K, VariableValue>,
) -> anyhow::Result<BTreeMap<String, Value>> {
    values
        .iter()
        .map(|(key, value)| Ok((key.as_ref().to_owned(), Value::encode(value)?)))
        .collect()
}

pub(crate) fn decode_variables<K: From<String> + Ord>(
    values: BTreeMap<String, Value>,
) -> anyhow::Result<BTreeMap<K, VariableValue>> {
    values
        .into_iter()
        .map(|(key, value)| Ok((key.into(), value.decode()?)))
        .collect()
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Expr {
    Unary(UnaryValue),
    Binary(u8, Box<Expr>, Box<Expr>),
    Function(String, Vec<Expr>, u64),
    Case(Option<Box<Expr>>, Vec<(Expr, Expr)>, Option<Box<Expr>>),
    List(Vec<Expr>),
    Object(BTreeMap<String, Expr>),
    Iterator(String, Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
}

#[derive(Serialize, Deserialize)]
pub(crate) enum UnaryValue {
    Operator(u8, Box<Expr>),
    Literal(LiteralValue),
    Property(String, String),
    ExpressionProperty(Box<Expr>, String),
    Parameter(String),
    Identifier(String),
    Variable(String, Box<Expr>),
    Alias(Box<Expr>, String),
    Range(Option<Box<Expr>>, Option<Box<Expr>>),
}

#[derive(Serialize, Deserialize)]
pub(crate) enum LiteralValue {
    Integer(i64),
    Real(u64),
    Boolean(bool),
    Text(String),
    Date(String),
    LocalTime(String),
    ZonedTime(String),
    LocalDateTime(String),
    ZonedDateTime(String),
    Duration(String),
    Object(Vec<(String, LiteralValue)>),
    Expression(Box<Expr>),
    Null,
}

fn encoded_optional(value: Option<&Expression>) -> anyhow::Result<Option<Box<Expr>>> {
    value
        .map(|value| Expr::encode(value).map(Box::new))
        .transpose()
}
fn decoded_optional(value: Option<Box<Expr>>) -> anyhow::Result<Option<Box<Expression>>> {
    value.map(|value| value.decode().map(Box::new)).transpose()
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
            _ => anyhow::bail!("invalid binary expression tag"),
        }
    };
}

impl Expr {
    fn encode(value: &Expression) -> anyhow::Result<Self> {
        Ok(match value {
            Expression::UnaryExpression(value) => Self::Unary(UnaryValue::encode(value)?),
            Expression::BinaryExpression(value) => {
                let (tag, left, right) = binary_parts!(value;
                    0=>And,1=>Or,2=>Eq,3=>Ne,4=>Lt,5=>Le,6=>Gt,7=>Ge,8=>In,9=>Add,
                    10=>Subtract,11=>Multiply,12=>Divide,13=>Modulo,14=>Exponent,
                    15=>HasLabel,16=>Index,17=>StartsWith,18=>EndsWith,19=>Contains);
                Self::Binary(
                    tag,
                    Box::new(Self::encode(left)?),
                    Box::new(Self::encode(right)?),
                )
            }
            Expression::FunctionExpression(value) => Self::Function(
                value.name.to_string(),
                value
                    .args
                    .iter()
                    .map(Self::encode)
                    .collect::<anyhow::Result<_>>()?,
                value.position_in_query as u64,
            ),
            Expression::CaseExpression(value) => Self::Case(
                encoded_optional(value.match_.as_deref())?,
                value
                    .when
                    .iter()
                    .map(|(left, right)| Ok((Self::encode(left)?, Self::encode(right)?)))
                    .collect::<anyhow::Result<_>>()?,
                encoded_optional(value.else_.as_deref())?,
            ),
            Expression::ListExpression(value) => Self::List(
                value
                    .elements
                    .iter()
                    .map(Self::encode)
                    .collect::<anyhow::Result<_>>()?,
            ),
            Expression::ObjectExpression(value) => Self::Object(
                value
                    .elements
                    .iter()
                    .map(|(key, value)| Ok((key.to_string(), Self::encode(value)?)))
                    .collect::<anyhow::Result<_>>()?,
            ),
            Expression::IteratorExpression(value) => Self::Iterator(
                value.item_identifier.to_string(),
                Box::new(Self::encode(&value.list_expression)?),
                encoded_optional(value.filter.as_deref())?,
                encoded_optional(value.map_expression.as_deref())?,
            ),
        })
    }

    fn decode(self) -> anyhow::Result<Expression> {
        Ok(match self {
            Self::Unary(value) => Expression::UnaryExpression(value.decode()?),
            Self::Binary(tag, left, right) => {
                let left = Box::new(left.decode()?);
                let right = Box::new(right.decode()?);
                Expression::BinaryExpression(binary_from!(tag,left,right;
                    0=>And,1=>Or,2=>Eq,3=>Ne,4=>Lt,5=>Le,6=>Gt,7=>Ge,8=>In,9=>Add,
                    10=>Subtract,11=>Multiply,12=>Divide,13=>Modulo,14=>Exponent,
                    15=>HasLabel,16=>Index,17=>StartsWith,18=>EndsWith,19=>Contains))
            }
            Self::Function(name, args, position) => {
                Expression::FunctionExpression(ast::FunctionExpression {
                    name: Arc::from(name),
                    args: args
                        .into_iter()
                        .map(Self::decode)
                        .collect::<anyhow::Result<_>>()?,
                    position_in_query: usize::try_from(position)?,
                })
            }
            Self::Case(match_, when, else_) => Expression::CaseExpression(ast::CaseExpression {
                match_: decoded_optional(match_)?,
                else_: decoded_optional(else_)?,
                when: when
                    .into_iter()
                    .map(|(left, right)| Ok((left.decode()?, right.decode()?)))
                    .collect::<anyhow::Result<_>>()?,
            }),
            Self::List(elements) => Expression::ListExpression(ast::ListExpression {
                elements: elements
                    .into_iter()
                    .map(Self::decode)
                    .collect::<anyhow::Result<_>>()?,
            }),
            Self::Object(elements) => Expression::ObjectExpression(ast::ObjectExpression {
                elements: elements
                    .into_iter()
                    .map(|(key, value)| Ok((Arc::from(key), value.decode()?)))
                    .collect::<anyhow::Result<_>>()?,
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

impl UnaryValue {
    fn encode(value: &Unary) -> anyhow::Result<Self> {
        Ok(match value {
            Unary::Not(value) => Self::Operator(0, Box::new(Expr::encode(value)?)),
            Unary::Exists(value) => Self::Operator(1, Box::new(Expr::encode(value)?)),
            Unary::IsNull(value) => Self::Operator(2, Box::new(Expr::encode(value)?)),
            Unary::IsNotNull(value) => Self::Operator(3, Box::new(Expr::encode(value)?)),
            Unary::Literal(value) => Self::Literal(LiteralValue::encode(value)?),
            Unary::Property { name, key } => Self::Property(name.to_string(), key.to_string()),
            Unary::ExpressionProperty { exp, key } => {
                Self::ExpressionProperty(Box::new(Expr::encode(exp)?), key.to_string())
            }
            Unary::Parameter(value) => Self::Parameter(value.to_string()),
            Unary::Identifier(value) => Self::Identifier(value.to_string()),
            Unary::Variable { name, value } => {
                Self::Variable(name.to_string(), Box::new(Expr::encode(value)?))
            }
            Unary::Alias { source, alias } => {
                Self::Alias(Box::new(Expr::encode(source)?), alias.to_string())
            }
            Unary::ListRange {
                start_bound,
                end_bound,
            } => Self::Range(
                encoded_optional(start_bound.as_deref())?,
                encoded_optional(end_bound.as_deref())?,
            ),
        })
    }

    fn decode(self) -> anyhow::Result<Unary> {
        Ok(match self {
            Self::Operator(tag, value) => {
                let value = Box::new(value.decode()?);
                match tag {
                    0 => Unary::Not(value),
                    1 => Unary::Exists(value),
                    2 => Unary::IsNull(value),
                    3 => Unary::IsNotNull(value),
                    _ => anyhow::bail!("invalid unary expression tag"),
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

impl LiteralValue {
    fn encode(value: &Literal) -> anyhow::Result<Self> {
        Ok(match value {
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
                    .map(|(key, value)| Ok((key.to_string(), Self::encode(value)?)))
                    .collect::<anyhow::Result<_>>()?,
            ),
            Literal::Expression(value) => Self::Expression(Box::new(Expr::encode(value)?)),
            Literal::Null => Self::Null,
        })
    }

    fn decode(self) -> anyhow::Result<Literal> {
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
                    .collect::<anyhow::Result<_>>()?,
            ),
            Self::Expression(value) => Literal::Expression(Box::new(value.decode()?)),
            Self::Null => Literal::Null,
        })
    }
}
