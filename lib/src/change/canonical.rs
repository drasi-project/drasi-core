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

//! Versioned canonical bytes for query-value identity hashing.
//!
//! Variant tags and presence markers are one byte. Lengths and numeric fields
//! are fixed-width big-endian values; strings are UTF-8 prefixed by a `u64`
//! byte length. Maps use their `BTreeMap` key order and lists retain input order.

use chrono::{Datelike, Timelike};
use drasi_core::{
    evaluation::{
        context::QueryVariables,
        variable_value::{ListRange, RangeBound, VariableValue},
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue},
};
use drasi_query_ast::ast::{
    BinaryExpression, Expression as AstExpression, Literal, UnaryExpression,
};
use std::hash::{Hash, Hasher};

const MAGIC: &[u8] = b"DRASI-QV";
const VERSION: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CanonicalEncodingError {
    #[error("query Float did not expose exactly one semantic IEEE-754 bit pattern")]
    InvalidFloat,
    #[error("query integer has no signed or unsigned representation")]
    InvalidInteger,
}

pub(crate) fn encode_query_variables(
    variables: &QueryVariables,
) -> Result<Vec<u8>, CanonicalEncodingError> {
    let mut encoder = CanonicalEncoder::default();
    encoder.raw(MAGIC);
    encoder.u8(VERSION);
    encoder.query_variables(variables)?;
    Ok(encoder.into_bytes())
}

#[derive(Default)]
struct CanonicalEncoder {
    bytes: Vec<u8>,
}

impl CanonicalEncoder {
    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.raw(&value.to_be_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.raw(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.raw(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.raw(&value.to_be_bytes());
    }

    fn length(&mut self, value: usize) {
        self.u64(value as u64);
    }

    fn bytes(&mut self, value: &[u8]) {
        self.length(value.len());
        self.raw(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn optional_string(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.string(value);
            }
            None => self.u8(0),
        }
    }

    fn query_variables(
        &mut self,
        variables: &QueryVariables,
    ) -> Result<(), CanonicalEncodingError> {
        self.length(variables.len());
        for (key, value) in variables {
            self.string(key);
            self.variable_value(value)?;
        }
        Ok(())
    }

    fn variable_value(&mut self, value: &VariableValue) -> Result<(), CanonicalEncodingError> {
        match value {
            VariableValue::Null => self.u8(0x00),
            VariableValue::Bool(value) => {
                self.u8(0x01);
                self.u8(u8::from(*value));
            }
            VariableValue::Float(value) => {
                self.u8(0x02);
                self.u64(query_float_bits(value)?);
            }
            VariableValue::Integer(value) => {
                if let Some(value) = value.as_u64() {
                    self.u8(0x03);
                    self.u64(value);
                } else if let Some(value) = value.as_i64() {
                    self.u8(0x04);
                    self.i64(value);
                } else {
                    return Err(CanonicalEncodingError::InvalidInteger);
                }
            }
            VariableValue::String(value) => {
                self.u8(0x05);
                self.string(value);
            }
            VariableValue::List(values) => {
                self.u8(0x06);
                self.length(values.len());
                for value in values {
                    self.variable_value(value)?;
                }
            }
            VariableValue::Object(values) => {
                self.u8(0x07);
                self.length(values.len());
                for (key, value) in values {
                    self.string(key);
                    self.variable_value(value)?;
                }
            }
            VariableValue::Date(value) => {
                self.u8(0x08);
                self.date(value);
            }
            VariableValue::LocalTime(value) => {
                self.u8(0x09);
                self.time(value);
            }
            VariableValue::ZonedTime(value) => {
                self.u8(0x0a);
                self.time(value.time());
                self.i32(value.offset().local_minus_utc());
            }
            VariableValue::LocalDateTime(value) => {
                self.u8(0x0b);
                self.date(&value.date());
                self.time(&value.time());
            }
            VariableValue::ZonedDateTime(value) => {
                self.u8(0x0c);
                self.fixed_datetime(value.datetime());
                self.optional_string(value.timezone_name().as_deref());
            }
            VariableValue::Duration(value) => {
                self.u8(0x0d);
                self.i64(value.duration().num_seconds());
                self.i32(value.duration().subsec_nanos());
                self.i64(*value.year());
                self.i64(*value.month());
            }
            VariableValue::Expression(value) => {
                self.u8(0x0e);
                self.expression(value)?;
            }
            VariableValue::ListRange(value) => {
                self.u8(0x0f);
                self.list_range(value);
            }
            VariableValue::Element(value) => {
                self.u8(0x10);
                self.element(value)?;
            }
            VariableValue::ElementMetadata(value) => {
                self.u8(0x11);
                self.element_metadata(value);
            }
            VariableValue::ElementReference(value) => {
                self.u8(0x12);
                self.element_reference(value);
            }
            VariableValue::Awaiting => self.u8(0x13),
        }
        Ok(())
    }

    fn date(&mut self, value: &chrono::NaiveDate) {
        self.i32(value.year());
        self.u8(value.month() as u8);
        self.u8(value.day() as u8);
    }

    fn time(&mut self, value: &chrono::NaiveTime) {
        self.u32(value.num_seconds_from_midnight());
        self.u32(value.nanosecond());
    }

    fn fixed_datetime(&mut self, value: &chrono::DateTime<chrono::FixedOffset>) {
        self.i64(value.timestamp());
        self.u32(value.timestamp_subsec_nanos());
        self.i32(value.offset().local_minus_utc());
    }

    fn list_range(&mut self, value: &ListRange) {
        self.range_bound(&value.start);
        self.range_bound(&value.end);
    }

    fn range_bound(&mut self, value: &RangeBound) {
        match value {
            RangeBound::Index(value) => {
                self.u8(0x00);
                self.i64(*value);
            }
            RangeBound::Unbounded => self.u8(0x01),
        }
    }

    fn element(&mut self, value: &Element) -> Result<(), CanonicalEncodingError> {
        match value {
            Element::Node {
                metadata,
                properties,
            } => {
                self.u8(0x00);
                self.element_metadata(metadata);
                self.element_properties(properties)?;
            }
            Element::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => {
                self.u8(0x01);
                self.element_metadata(metadata);
                self.element_reference(in_node);
                self.element_reference(out_node);
                self.element_properties(properties)?;
            }
        }
        Ok(())
    }

    fn element_metadata(&mut self, value: &ElementMetadata) {
        self.element_reference(&value.reference);
        self.length(value.labels.len());
        for label in value.labels.iter() {
            self.string(label);
        }
        self.u64(value.effective_from);
    }

    fn element_reference(&mut self, value: &ElementReference) {
        self.string(&value.source_id);
        self.string(&value.element_id);
    }

    fn element_properties(
        &mut self,
        value: &ElementPropertyMap,
    ) -> Result<(), CanonicalEncodingError> {
        let entries: Vec<_> = value
            .map_iter(|key, value| (key.clone(), value.clone()))
            .collect();
        self.length(entries.len());
        for (key, value) in entries {
            self.string(&key);
            self.element_value(&value)?;
        }
        Ok(())
    }

    fn element_value(&mut self, value: &ElementValue) -> Result<(), CanonicalEncodingError> {
        match value {
            ElementValue::Null => self.u8(0x00),
            ElementValue::Bool(value) => {
                self.u8(0x01);
                self.u8(u8::from(*value));
            }
            ElementValue::Float(value) => {
                self.u8(0x02);
                self.u64(canonical_f64_bits(value.into_inner()));
            }
            ElementValue::Integer(value) => {
                self.u8(0x03);
                self.i64(*value);
            }
            ElementValue::String(value) => {
                self.u8(0x04);
                self.string(value);
            }
            ElementValue::List(values) => {
                self.u8(0x05);
                self.length(values.len());
                for value in values {
                    self.element_value(value)?;
                }
            }
            ElementValue::Object(values) => {
                self.u8(0x06);
                self.element_properties(values)?;
            }
            ElementValue::LocalDateTime(value) => {
                self.u8(0x07);
                self.date(&value.date());
                self.time(&value.time());
            }
            ElementValue::ZonedDateTime(value) => {
                self.u8(0x08);
                self.fixed_datetime(value);
            }
        }
        Ok(())
    }

    fn expression(&mut self, value: &AstExpression) -> Result<(), CanonicalEncodingError> {
        match value {
            AstExpression::UnaryExpression(value) => {
                self.u8(0x00);
                self.unary_expression(value)?;
            }
            AstExpression::BinaryExpression(value) => {
                self.u8(0x01);
                self.binary_expression(value)?;
            }
            AstExpression::FunctionExpression(value) => {
                self.u8(0x02);
                self.string(&value.name);
                self.length(value.args.len());
                for argument in &value.args {
                    self.expression(argument)?;
                }
                self.u64(value.position_in_query as u64);
            }
            AstExpression::CaseExpression(value) => {
                self.u8(0x03);
                self.optional_expression(value.match_.as_deref())?;
                self.length(value.when.len());
                for (when, then) in &value.when {
                    self.expression(when)?;
                    self.expression(then)?;
                }
                self.optional_expression(value.else_.as_deref())?;
            }
            AstExpression::ListExpression(value) => {
                self.u8(0x04);
                self.length(value.elements.len());
                for element in &value.elements {
                    self.expression(element)?;
                }
            }
            AstExpression::ObjectExpression(value) => {
                self.u8(0x05);
                self.length(value.elements.len());
                for (key, expression) in &value.elements {
                    self.string(key);
                    self.expression(expression)?;
                }
            }
            AstExpression::IteratorExpression(value) => {
                self.u8(0x06);
                self.string(&value.item_identifier);
                self.expression(&value.list_expression)?;
                self.optional_expression(value.filter.as_deref())?;
                self.optional_expression(value.map_expression.as_deref())?;
            }
        }
        Ok(())
    }

    fn optional_expression(
        &mut self,
        value: Option<&AstExpression>,
    ) -> Result<(), CanonicalEncodingError> {
        match value {
            Some(value) => {
                self.u8(1);
                self.expression(value)?;
            }
            None => self.u8(0),
        }
        Ok(())
    }

    fn unary_expression(&mut self, value: &UnaryExpression) -> Result<(), CanonicalEncodingError> {
        match value {
            UnaryExpression::Not(value) => {
                self.u8(0x00);
                self.expression(value)?;
            }
            UnaryExpression::Exists(value) => {
                self.u8(0x01);
                self.expression(value)?;
            }
            UnaryExpression::IsNull(value) => {
                self.u8(0x02);
                self.expression(value)?;
            }
            UnaryExpression::IsNotNull(value) => {
                self.u8(0x03);
                self.expression(value)?;
            }
            UnaryExpression::Literal(value) => {
                self.u8(0x04);
                self.literal(value)?;
            }
            UnaryExpression::Property { name, key } => {
                self.u8(0x05);
                self.string(name);
                self.string(key);
            }
            UnaryExpression::ExpressionProperty { exp, key } => {
                self.u8(0x06);
                self.expression(exp)?;
                self.string(key);
            }
            UnaryExpression::Parameter(value) => {
                self.u8(0x07);
                self.string(value);
            }
            UnaryExpression::Identifier(value) => {
                self.u8(0x08);
                self.string(value);
            }
            UnaryExpression::Variable { name, value } => {
                self.u8(0x09);
                self.string(name);
                self.expression(value)?;
            }
            UnaryExpression::Alias { source, alias } => {
                self.u8(0x0a);
                self.expression(source)?;
                self.string(alias);
            }
            UnaryExpression::ListRange {
                start_bound,
                end_bound,
            } => {
                self.u8(0x0b);
                self.optional_expression(start_bound.as_deref())?;
                self.optional_expression(end_bound.as_deref())?;
            }
        }
        Ok(())
    }

    fn binary_expression(
        &mut self,
        value: &BinaryExpression,
    ) -> Result<(), CanonicalEncodingError> {
        let (tag, left, right) = match value {
            BinaryExpression::And(left, right) => (0x00, left, right),
            BinaryExpression::Or(left, right) => (0x01, left, right),
            BinaryExpression::Eq(left, right) => (0x02, left, right),
            BinaryExpression::Ne(left, right) => (0x03, left, right),
            BinaryExpression::Lt(left, right) => (0x04, left, right),
            BinaryExpression::Le(left, right) => (0x05, left, right),
            BinaryExpression::Gt(left, right) => (0x06, left, right),
            BinaryExpression::Ge(left, right) => (0x07, left, right),
            BinaryExpression::In(left, right) => (0x08, left, right),
            BinaryExpression::Add(left, right) => (0x09, left, right),
            BinaryExpression::Subtract(left, right) => (0x0a, left, right),
            BinaryExpression::Multiply(left, right) => (0x0b, left, right),
            BinaryExpression::Divide(left, right) => (0x0c, left, right),
            BinaryExpression::Modulo(left, right) => (0x0d, left, right),
            BinaryExpression::Exponent(left, right) => (0x0e, left, right),
            BinaryExpression::HasLabel(left, right) => (0x0f, left, right),
            BinaryExpression::Index(left, right) => (0x10, left, right),
            BinaryExpression::StartsWith(left, right) => (0x11, left, right),
            BinaryExpression::EndsWith(left, right) => (0x12, left, right),
            BinaryExpression::Contains(left, right) => (0x13, left, right),
        };
        self.u8(tag);
        self.expression(left)?;
        self.expression(right)?;
        Ok(())
    }

    fn literal(&mut self, value: &Literal) -> Result<(), CanonicalEncodingError> {
        match value {
            Literal::Integer(value) => {
                self.u8(0x00);
                self.i64(*value);
            }
            Literal::Real(value) => {
                self.u8(0x01);
                self.u64(canonical_f64_bits(*value));
            }
            Literal::Boolean(value) => {
                self.u8(0x02);
                self.u8(u8::from(*value));
            }
            Literal::Text(value) => {
                self.u8(0x03);
                self.string(value);
            }
            Literal::Date(value) => {
                self.u8(0x04);
                self.string(value);
            }
            Literal::LocalTime(value) => {
                self.u8(0x05);
                self.string(value);
            }
            Literal::ZonedTime(value) => {
                self.u8(0x06);
                self.string(value);
            }
            Literal::LocalDateTime(value) => {
                self.u8(0x07);
                self.string(value);
            }
            Literal::ZonedDateTime(value) => {
                self.u8(0x08);
                self.string(value);
            }
            Literal::Duration(value) => {
                self.u8(0x09);
                self.string(value);
            }
            Literal::Object(values) => {
                self.u8(0x0a);
                self.length(values.len());
                // Literal objects use Vec storage, so normalize their map key order here.
                let mut entries: Vec<_> = values.iter().collect();
                entries.sort_by_key(|(key, _)| key.clone());
                for (key, value) in entries {
                    self.string(key);
                    self.literal(value)?;
                }
            }
            Literal::Expression(value) => {
                self.u8(0x0b);
                self.expression(value)?;
            }
            Literal::Null => self.u8(0x0c),
        }
        Ok(())
    }
}

fn query_float_bits(
    value: &drasi_core::evaluation::variable_value::float::Float,
) -> Result<u64, CanonicalEncodingError> {
    // Float's explicit Hash implementation is its only lossless public projection:
    // it emits one u64 containing raw bits, except that equal signed zeros normalize.
    let mut capture = FloatBitsCapture::default();
    value.hash(&mut capture);
    capture
        .into_bits()
        .ok_or(CanonicalEncodingError::InvalidFloat)
}

fn canonical_f64_bits(value: f64) -> u64 {
    if value == 0.0 {
        0.0f64.to_bits()
    } else {
        value.to_bits()
    }
}

#[derive(Default)]
struct FloatBitsCapture {
    bits: Option<u64>,
    invalid_write: bool,
}

impl FloatBitsCapture {
    fn into_bits(self) -> Option<u64> {
        if self.invalid_write {
            None
        } else {
            self.bits
        }
    }
}

impl Hasher for FloatBitsCapture {
    fn finish(&self) -> u64 {
        self.bits.unwrap_or_default()
    }

    fn write(&mut self, _bytes: &[u8]) {
        self.invalid_write = true;
    }

    fn write_u64(&mut self, value: u64) {
        if self.bits.replace(value).is_some() {
            self.invalid_write = true;
        }
    }
}
