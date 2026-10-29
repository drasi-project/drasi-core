// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Bounded structural checks before the standard record validators run.
//! These are additional transport bounds, not replacements for those validators.
//!
//! Graph records are canonical MessagePack. An iterative framing preflight bounds
//! nesting, node count and collection sizes before serde allocates; the shared
//! `ElementImage`/`ElementPatch`/`ElementBeforeImage` codecs then decode and
//! validate the record, and a walk enforces semantic limits and namespaces.
//! Expressions and awaiting values are inert data there. A complete source
//! transaction is the same standard graph data, so every operation record passes
//! this guard before its separately validated completion marker is read.

use std::collections::BTreeMap;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
};
use drasi_core_types::data_value::{DataExpression, DataLiteral, DataUnary, DataValue};
use drasi_lib::computation::v1::{
    decode_bounded_messagepack, ElementBeforeImage, ElementImage, ElementPatch, RecordImage,
};

use crate::{codec::preflight_nested, BridgeError, Limits, Namespace, Result};

/// The raw MessagePack of a record spends several nodes per semantic value
/// (tag, wrapper, key), so its node budget is a multiple of `max_values`.
const GRAPH_NODES_PER_VALUE: usize = 4;
/// Containers outside the value tree: patch/image/variant/fields/properties.
const GRAPH_WRAPPER_NESTING: usize = 8;
/// The deepest wrapping of one semantic level is a nested element:
/// `Typed`/`Element`/`Node`/fields/`properties`/`values`.
const GRAPH_LEVEL_NESTING: usize = 6;

pub(crate) struct PayloadGuard<'a> {
    limits: &'a Limits,
    namespace: &'a Namespace,
    values: usize,
}

impl<'a> PayloadGuard<'a> {
    pub(crate) fn new(limits: &'a Limits, namespace: &'a Namespace) -> Self {
        Self {
            limits,
            namespace,
            values: 0,
        }
    }

    fn bounded_identity(namespace: &str, bytes: &[u8]) -> Result<()> {
        if namespace.is_empty() || namespace.len() > 1024 || bytes.is_empty() || bytes.len() > 4096
        {
            return Err(BridgeError::invalid(
                "record identity exceeds bridge bounds",
            ));
        }
        Ok(())
    }

    /// Check one complete record: its identity, its payload, and for graph
    /// records that both name the same element.
    pub(crate) fn record(
        &mut self,
        image: RecordImage,
        namespace: &str,
        identity: &[u8],
        payload: &[u8],
    ) -> Result<()> {
        Self::bounded_identity(namespace, identity)?;
        let reference = self.graph_identity(namespace, identity)?;
        if self.graph_payload(image, payload)? != reference {
            return Err(BridgeError::invalid(
                "graph record identity does not match its payload",
            ));
        }
        Ok(())
    }

    pub(crate) fn identity(&mut self, namespace: &str, bytes: &[u8]) -> Result<()> {
        Self::bounded_identity(namespace, bytes)?;
        self.graph_identity(namespace, bytes).map(drop)
    }

    fn work(&mut self, depth: usize) -> Result<()> {
        self.values += 1;
        if depth > self.limits.max_depth || self.values > self.limits.max_values {
            return Err(BridgeError::invalid(
                "typed record depth or value budget exceeded",
            ));
        }
        Ok(())
    }

    fn text(&mut self, value: &str, max: usize) -> Result<()> {
        self.work(0)?;
        if value.len() > max {
            return Err(BridgeError::invalid("typed string exceeds bridge limit"));
        }
        Ok(())
    }

    fn graph_identity(&mut self, namespace: &str, bytes: &[u8]) -> Result<ElementReference> {
        preflight_nested(bytes, 4096, 16, 1)?;
        let reference: ElementReference = decode_bounded_messagepack(bytes, bytes.len())
            .map_err(|error| BridgeError::invalid(error.to_string()))?;
        let expected = drasi_lib::computation::v1::GraphChangeCodec::element_reference(&reference)
            .map_err(|error| BridgeError::invalid(error.to_string()))?;
        if expected.identity().namespace() != namespace || expected.identity().value() != bytes {
            return Err(BridgeError::invalid("graph element key is not canonical"));
        }
        self.graph_reference(&reference)?;
        Ok(reference)
    }

    /// Frame-check, decode with the shared canonical codecs and walk one full
    /// image, patch or before-image. Returns the element it names.
    fn graph_payload(&mut self, image: RecordImage, bytes: &[u8]) -> Result<ElementReference> {
        let nodes = self.limits.max_values.saturating_mul(GRAPH_NODES_PER_VALUE);
        let nesting = GRAPH_WRAPPER_NESTING
            .saturating_add(GRAPH_LEVEL_NESTING.saturating_mul(self.limits.max_depth));
        preflight_nested(bytes, self.limits.max_envelope_bytes, nodes, nesting)?;
        let invalid = |error: &dyn std::fmt::Display| BridgeError::invalid(error.to_string());
        match image {
            RecordImage::Full => {
                let image = ElementImage::decode(bytes, bytes.len()).map_err(|e| invalid(&e))?;
                self.graph_image(&image)?;
                Ok(image.metadata().reference.clone())
            }
            RecordImage::Patch => {
                // The shared decoder rejects a property that is both set and removed.
                let patch = ElementPatch::decode(bytes, bytes.len()).map_err(|e| invalid(&e))?;
                self.graph_image(patch.image())?;
                for name in patch.removed_properties() {
                    self.text(name, 256)?;
                }
                Ok(patch.reference().clone())
            }
            RecordImage::Partial => {
                let before: ElementBeforeImage =
                    decode_bounded_messagepack(bytes, bytes.len()).map_err(|e| invalid(&e))?;
                before.validate().map_err(|e| invalid(&e))?;
                match &before {
                    ElementBeforeImage::Metadata(metadata) => self.graph_metadata(metadata)?,
                    ElementBeforeImage::Element(image) => self.graph_image(image)?,
                }
                Ok(before.metadata().reference.clone())
            }
        }
    }

    fn graph_reference(&mut self, reference: &ElementReference) -> Result<()> {
        self.text(&reference.source_id, 256)?;
        self.namespace.source(&reference.source_id)?;
        self.text(&reference.element_id, 256)?;
        crate::config::identifier(&reference.element_id)
    }

    fn graph_metadata(&mut self, metadata: &ElementMetadata) -> Result<()> {
        self.graph_reference(&metadata.reference)?;
        for label in metadata.labels.iter() {
            self.text(label, 256)?;
        }
        Ok(())
    }

    fn graph_image(&mut self, image: &ElementImage) -> Result<()> {
        self.work(0)?;
        match image {
            ElementImage::Node {
                metadata,
                properties,
            } => {
                self.graph_metadata(metadata)?;
                self.data_properties(properties, 1)
            }
            ElementImage::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => {
                self.graph_metadata(metadata)?;
                self.graph_reference(in_node)?;
                self.graph_reference(out_node)?;
                self.data_properties(properties, 1)
            }
        }
    }

    /// Values of a property map sit at `depth`. Wrapper variants add no depth,
    /// so only nested containers, elements and expressions consume `max_depth`.
    fn data_properties(
        &mut self,
        properties: &BTreeMap<String, DataValue>,
        depth: usize,
    ) -> Result<()> {
        for (name, value) in properties {
            self.text(name, 256)?;
            self.data(value, depth)?;
        }
        Ok(())
    }

    fn element_properties(&mut self, properties: &ElementPropertyMap, depth: usize) -> Result<()> {
        for (name, value) in properties.map_iter(|name, value| (name, value)) {
            self.text(name, 256)?;
            self.element_value(value, depth)?;
        }
        Ok(())
    }

    /// Expressions and awaiting values are inert data here: walked for limits
    /// and never executed.
    fn data(&mut self, value: &DataValue, depth: usize) -> Result<()> {
        self.work(depth)?;
        match value {
            DataValue::Null
            | DataValue::Bool(_)
            | DataValue::Signed(_)
            | DataValue::Unsigned(_)
            | DataValue::Float(_)
            | DataValue::Date(_)
            | DataValue::LocalTime(_)
            | DataValue::ZonedTime(..)
            | DataValue::LocalDateTime(_)
            | DataValue::Duration { .. }
            | DataValue::Range(..)
            | DataValue::Awaiting => {}
            DataValue::String(value) => self.text(value, 64 * 1024)?,
            DataValue::ZonedDateTime(_, zone) => {
                if let Some(zone) = zone {
                    self.text(zone, 256)?;
                }
            }
            DataValue::List(values) => {
                for value in values {
                    self.data(value, depth + 1)?;
                }
            }
            DataValue::Object(values) => self.data_properties(values, depth + 1)?,
            DataValue::Element(element) => match element {
                Element::Node {
                    metadata,
                    properties,
                } => {
                    self.graph_metadata(metadata)?;
                    self.element_properties(properties, depth + 1)?;
                }
                Element::Relation {
                    metadata,
                    in_node,
                    out_node,
                    properties,
                } => {
                    self.graph_metadata(metadata)?;
                    self.graph_reference(in_node)?;
                    self.graph_reference(out_node)?;
                    self.element_properties(properties, depth + 1)?;
                }
            },
            DataValue::Metadata(metadata) => self.graph_metadata(metadata)?,
            DataValue::Reference(reference) => self.graph_reference(reference)?,
            DataValue::Expression(expression) => self.expression(expression, depth + 1)?,
        }
        Ok(())
    }

    fn element_value(&mut self, value: &ElementValue, depth: usize) -> Result<()> {
        self.work(depth)?;
        match value {
            ElementValue::Null
            | ElementValue::Bool(_)
            | ElementValue::Float(_)
            | ElementValue::Integer(_)
            | ElementValue::LocalDateTime(_)
            | ElementValue::ZonedDateTime(_) => {}
            ElementValue::String(value) => self.text(value, 64 * 1024)?,
            ElementValue::List(values) => {
                for value in values {
                    self.element_value(value, depth + 1)?;
                }
            }
            ElementValue::Object(properties) => self.element_properties(properties, depth + 1)?,
            ElementValue::Typed(value) => self.data(value.as_data_value(), depth)?,
        }
        Ok(())
    }

    fn expression(&mut self, expression: &DataExpression, depth: usize) -> Result<()> {
        self.work(depth)?;
        match expression {
            DataExpression::Unary(unary) => match unary {
                DataUnary::Operator(_, inner) => self.expression(inner, depth + 1)?,
                DataUnary::Literal(literal) => self.literal(literal, depth + 1)?,
                DataUnary::Property(first, second) => {
                    self.text(first, 256)?;
                    self.text(second, 256)?;
                }
                DataUnary::ExpressionProperty(inner, name)
                | DataUnary::Variable(name, inner)
                | DataUnary::Alias(inner, name) => {
                    self.text(name, 256)?;
                    self.expression(inner, depth + 1)?;
                }
                DataUnary::Parameter(name) | DataUnary::Identifier(name) => self.text(name, 256)?,
                DataUnary::Range(start, end) => {
                    for bound in [start, end].into_iter().flatten() {
                        self.expression(bound, depth + 1)?;
                    }
                }
            },
            DataExpression::Binary(_, left, right) => {
                self.expression(left, depth + 1)?;
                self.expression(right, depth + 1)?;
            }
            DataExpression::Function(name, arguments, _) => {
                self.text(name, 256)?;
                for argument in arguments {
                    self.expression(argument, depth + 1)?;
                }
            }
            DataExpression::Case(subject, branches, otherwise) => {
                for inner in [subject, otherwise].into_iter().flatten() {
                    self.expression(inner, depth + 1)?;
                }
                for (condition, result) in branches {
                    self.expression(condition, depth + 1)?;
                    self.expression(result, depth + 1)?;
                }
            }
            DataExpression::List(items) => {
                for item in items {
                    self.expression(item, depth + 1)?;
                }
            }
            DataExpression::Object(items) => {
                for (name, item) in items {
                    self.text(name, 256)?;
                    self.expression(item, depth + 1)?;
                }
            }
            DataExpression::Iterator(name, source, filter, map) => {
                self.text(name, 256)?;
                self.expression(source, depth + 1)?;
                for inner in [filter, map].into_iter().flatten() {
                    self.expression(inner, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    fn literal(&mut self, literal: &DataLiteral, depth: usize) -> Result<()> {
        self.work(depth)?;
        match literal {
            DataLiteral::Integer(_)
            | DataLiteral::Real(_)
            | DataLiteral::Boolean(_)
            | DataLiteral::Null => {}
            DataLiteral::Text(value) => self.text(value, 64 * 1024)?,
            DataLiteral::Date(value)
            | DataLiteral::LocalTime(value)
            | DataLiteral::ZonedTime(value)
            | DataLiteral::LocalDateTime(value)
            | DataLiteral::ZonedDateTime(value)
            | DataLiteral::Duration(value) => self.text(value, 256)?,
            DataLiteral::Object(entries) => {
                for (name, value) in entries {
                    self.text(name, 256)?;
                    self.literal(value, depth + 1)?;
                }
            }
            DataLiteral::Expression(inner) => self.expression(inner, depth + 1)?,
        }
        Ok(())
    }
}

/// JSON annotations remain bytes, but known framework readers later decode
/// them. Check nesting without first constructing a recursively dropped Value.
pub(crate) fn annotation_json(bytes: &[u8], max_depth: usize) -> Result<()> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > max_depth {
                        return Err(BridgeError::invalid("annotation JSON nesting limit"));
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}
