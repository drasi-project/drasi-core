extern crate alloc;
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime};
use core::fmt::{self, Debug, Display};
use drasi_query_ast::ast::Expression as AstExpression;
use duration::Duration;
use float::Float;
use index::Index;
use integer::Integer;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    sync::Arc,
};
use zoned_datetime::ZonedDateTime;
use zoned_time::ZonedTime;

use crate::models::{Element, ElementMetadata, ElementReference};

#[allow(clippy::derived_hash_with_manual_eq)]
#[derive(Clone, Hash, Eq, Default)]
pub enum VariableValue {
    #[default]
    Null,
    Bool(bool),
    Float(Float),
    Integer(Integer),
    String(String),
    List(Vec<VariableValue>),
    Object(BTreeMap<String, VariableValue>), //Do we need our own map type?

    Date(NaiveDate), //NaiveDate does not support TimeZone, which is consistent with Neo4j's documentation
    LocalTime(NaiveTime),
    ZonedTime(ZonedTime),
    LocalDateTime(NaiveDateTime), // no timezone info
    ZonedDateTime(ZonedDateTime),
    Duration(Duration),
    Expression(AstExpression),
    ListRange(ListRange),
    Element(Arc<Element>),
    ElementMetadata(ElementMetadata),
    ElementReference(ElementReference),
    Awaiting,
}

impl From<VariableValue> for Value {
    fn from(val: VariableValue) -> Self {
        match val {
            VariableValue::Null => Value::Null,
            VariableValue::Bool(b) => Value::Bool(b),
            VariableValue::Float(f) => Value::Number(f.into()),
            VariableValue::Integer(i) => Value::Number(i.into()),
            VariableValue::String(s) => Value::String(s),
            VariableValue::List(l) => Value::Array(l.into_iter().map(|x| x.into()).collect()),
            VariableValue::Object(o) => {
                Value::Object(o.into_iter().map(|(k, v)| (k, v.into())).collect())
            }
            VariableValue::Date(d) => Value::String(d.to_string()),
            VariableValue::LocalTime(t) => Value::String(t.to_string()),
            VariableValue::ZonedTime(t) => Value::String(t.to_string()),
            VariableValue::LocalDateTime(t) => Value::String(t.to_string()),
            VariableValue::ZonedDateTime(t) => Value::String(t.to_string()),
            VariableValue::Duration(d) => Value::String(d.to_string()),
            VariableValue::Expression(e) => Value::String(format!("{:?}", e)),
            VariableValue::ListRange(r) => Value::String(r.to_string()),
            VariableValue::Element(e) => e.as_ref().into(),
            VariableValue::ElementMetadata(m) => Value::String(m.to_string()),
            VariableValue::ElementReference(r) => Value::String(r.to_string()),
            VariableValue::Awaiting => Value::String("Awaiting".to_string()),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ListRange {
    pub start: RangeBound,
    pub end: RangeBound,
}

impl Display for ListRange {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum RangeBound {
    Index(i64),
    Unbounded,
}

impl Display for RangeBound {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            RangeBound::Index(i) => write!(f, "{}", i),
            RangeBound::Unbounded => write!(f, "..."),
        }
    }
}

impl VariableValue {
    pub fn get<I: Index>(&self, index: I) -> Option<&VariableValue> {
        index.index_into(self)
    }

    pub fn get_mut<I: Index>(&mut self, index: I) -> Option<&mut VariableValue> {
        index.index_into_mut(self)
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, VariableValue>> {
        match self {
            VariableValue::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn as_object_mut(&mut self) -> Option<&mut BTreeMap<String, VariableValue>> {
        match self {
            VariableValue::Object(map) => Some(map),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        self.as_object().is_some()
    }

    pub fn as_array(&self) -> Option<&Vec<VariableValue>> {
        match self {
            VariableValue::List(list) => Some(list),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<VariableValue>> {
        match self {
            VariableValue::List(list) => Some(list),
            _ => None,
        }
    }

    pub fn is_array(&self) -> bool {
        self.as_array().is_some()
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            VariableValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_string(&self) -> bool {
        self.as_str().is_some()
    }

    pub fn is_number(&self) -> bool {
        matches!(*self, VariableValue::Integer(_) | VariableValue::Float(_))
    }

    pub fn is_i64(&self) -> bool {
        match self {
            VariableValue::Integer(n) => n.is_i64(),
            _ => false,
        }
    }

    pub fn is_f64(&self) -> bool {
        match self {
            VariableValue::Float(n) => n.is_f64(),
            _ => false,
        }
    }

    pub fn is_u64(&self) -> bool {
        match self {
            VariableValue::Integer(n) => n.is_u64(),
            _ => false,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            VariableValue::Integer(n) => n.as_i64(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            VariableValue::Float(n) => n.as_f64(),
            VariableValue::Integer(n) => n.as_i64().map(|n| n as f64),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            VariableValue::Integer(n) => n.as_u64(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            VariableValue::Bool(b) => Some(b),
            _ => None,
        }
    }

    pub fn is_boolean(&self) -> bool {
        self.as_bool().is_some()
    }

    pub fn as_null(&self) -> Option<()> {
        match *self {
            VariableValue::Null => Some(()),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        self.as_null().is_some()
    }

    pub fn as_date(&self) -> Option<NaiveDate> {
        match *self {
            VariableValue::Date(d) => Some(d),
            _ => None,
        }
    }

    pub fn is_date(&self) -> bool {
        self.as_date().is_some()
    }

    pub fn as_local_time(&self) -> Option<NaiveTime> {
        match *self {
            VariableValue::LocalTime(t) => Some(t),
            _ => None,
        }
    }

    pub fn is_local_time(&self) -> bool {
        self.as_local_time().is_some()
    }

    pub fn as_time(&self) -> Option<ZonedTime> {
        match self {
            VariableValue::ZonedTime(t) => Some(*t),
            _ => None,
        }
    }

    pub fn is_time(&self) -> bool {
        self.as_time().is_some()
    }

    pub fn as_local_date_time(&self) -> Option<NaiveDateTime> {
        match *self {
            VariableValue::LocalDateTime(t) => Some(t),
            _ => None,
        }
    }

    pub fn is_local_date_time(&self) -> bool {
        self.as_local_date_time().is_some()
    }

    pub fn as_zoned_date_time(&self) -> Option<ZonedDateTime> {
        match self {
            VariableValue::ZonedDateTime(t) => Some(t.clone()),
            _ => None,
        }
    }

    pub fn get_date_property(&self, property: String) -> Option<String> {
        if self.is_date() {
            match property.as_str() {
                "year" => Some(match self.as_date() {
                    Some(date) => date.year().to_string(),
                    None => return None,
                }),
                "month" => Some(match self.as_date() {
                    Some(date) => date.month().to_string(),
                    None => return None,
                }),
                "day" => Some(match self.as_date() {
                    Some(date) => date.day().to_string(),
                    None => return None,
                }),
                _ => None,
            }
        } else {
            None
        }
    }

    pub fn is_zoned_date_time(&self) -> bool {
        self.as_zoned_date_time().is_some()
    }

    pub fn as_duration(&self) -> Option<Duration> {
        match self {
            VariableValue::Duration(d) => Some(d.clone()),
            _ => None,
        }
    }

    pub fn is_duration(&self) -> bool {
        self.as_duration().is_some()
    }

    pub fn as_expression(&self) -> Option<AstExpression> {
        match self {
            VariableValue::Expression(e) => Some(e.clone()),
            _ => None,
        }
    }

    pub fn is_expression(&self) -> bool {
        self.as_expression().is_some()
    }

    pub fn as_list_range(&self) -> Option<ListRange> {
        match self {
            VariableValue::ListRange(r) => Some(r.clone()),
            _ => None,
        }
    }

    pub fn is_list_range(&self) -> bool {
        self.as_list_range().is_some()
    }

    pub fn pointer(&self, pointer: &str) -> Option<&VariableValue> {
        if pointer.is_empty() {
            return Some(self);
        }
        if !pointer.starts_with('/') {
            return None;
        }
        pointer
            .split('/')
            .skip(1)
            .map(|x| x.replace("~1", "/").replace("~0", "~"))
            .try_fold(self, |target, token| match target {
                VariableValue::Object(map) => map.get(&token),
                VariableValue::List(list) => parse_index(&token).and_then(|x| list.get(x)),
                _ => None,
            })
    }

    pub fn hash_for_groupby<H: Hasher>(&self, state: &mut H) {
        match self {
            VariableValue::Element(element) => element.get_reference().hash(state),
            _ => self.hash(state),
        }
    }
}

fn parse_index(s: &str) -> Option<usize> {
    if s.starts_with('+') || (s.starts_with('0') && s.len() != 1) {
        return None;
    }
    s.parse().ok()
}

impl Debug for VariableValue {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            VariableValue::Null => formatter.write_str("Null"),
            VariableValue::Bool(boolean) => write!(formatter, "Bool({})", boolean),
            VariableValue::Integer(integer) => write!(formatter, "Integer({})", integer),
            VariableValue::Float(float) => write!(formatter, "Float({})", float),
            VariableValue::String(string) => write!(formatter, "String({:?})", string),
            VariableValue::List(vec) => {
                let _ = formatter.write_str("List ");
                Debug::fmt(vec, formatter)
            }
            VariableValue::Object(map) => {
                let _ = formatter.write_str("Object ");
                Debug::fmt(map, formatter)
            }
            VariableValue::Date(date) => write!(formatter, "Date({})", date),
            VariableValue::LocalTime(time) => write!(formatter, "LocalTime({})", time),
            VariableValue::ZonedTime(time) => write!(formatter, "Time({})", time),
            VariableValue::LocalDateTime(time) => write!(formatter, "LocalDateTime({})", time),
            VariableValue::ZonedDateTime(time) => write!(formatter, "ZonedDateTime({})", time),
            VariableValue::Duration(duration) => write!(formatter, "Duration({})", duration),
            VariableValue::Expression(expression) => {
                write!(formatter, "Expression({:?})", expression)
            }
            VariableValue::ListRange(range) => write!(formatter, "ListRange({:?})", range),
            VariableValue::Element(element) => write!(formatter, "Element({:?})", element),
            VariableValue::ElementMetadata(metadata) => {
                write!(formatter, "ElementMetadata({:?})", metadata)
            }
            VariableValue::ElementReference(reference) => {
                write!(formatter, "ElementReference({:?})", reference)
            }
            VariableValue::Awaiting => write!(formatter, "Awaiting"),
        }
    }
}

impl Display for VariableValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self {
            VariableValue::Null => write!(f, "null"),
            VariableValue::Bool(b) => write!(f, "{}", b),
            VariableValue::Integer(i) => write!(f, "{}", i),
            VariableValue::Float(fl) => write!(f, "{}", fl),
            VariableValue::String(s) => write!(f, "{}", s),
            VariableValue::List(l) => {
                let mut first = true;
                write!(f, "[")?;
                for item in l {
                    if first {
                        first = false;
                    } else {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            VariableValue::Object(o) => {
                let mut first = true;
                write!(f, "{{")?;
                for (key, value) in o {
                    if first {
                        first = false;
                    } else {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", key, value)?;
                }
                write!(f, "}}")
            }
            VariableValue::Date(d) => write!(f, "{}", d),
            VariableValue::LocalTime(t) => write!(f, "{}", t),
            VariableValue::ZonedTime(t) => write!(f, "{}", t),
            VariableValue::LocalDateTime(t) => write!(f, "{}", t),
            VariableValue::ZonedDateTime(t) => write!(f, "{}", t),
            VariableValue::Duration(d) => write!(f, "{}", d),
            VariableValue::Expression(e) => write!(f, "{:?}", e),
            VariableValue::ListRange(r) => write!(f, "{}", r),
            VariableValue::Element(e) => write!(f, "{:?}", e),
            VariableValue::ElementMetadata(m) => write!(f, "{}", m),
            VariableValue::ElementReference(r) => write!(f, "{}", r),
            VariableValue::Awaiting => write!(f, "Awaiting"),
        }
    }
}

pub mod de;
pub mod duration;
pub mod float;
mod from;
mod index;
pub mod integer;
mod partial_eq;
pub mod ser;
#[cfg(test)]
mod tests;
pub mod zoned_datetime;
pub mod zoned_time;
