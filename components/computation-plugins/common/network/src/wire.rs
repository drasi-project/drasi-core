// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Network-only conversion. Graph events stay typed until the transport boundary.
//! Source, transformer and query output reach a network sink through the same
//! standard graph-change decoding; only the configured output binding names
//! the data, and no query annotation is read.

#[cfg(feature = "grpc-source")]
use crate::proto::source;
#[cfg(any(feature = "http-source", feature = "grpc-source"))]
use anyhow::Context;
use anyhow::{ensure, Result};
#[cfg(any(feature = "http-source", feature = "grpc-source"))]
use drasi_computation_contracts::ComponentId;
use drasi_computation_contracts::{
    BoundOutput, ChangeNotification, GraphChangeBatch, InputEnvelope, SinkOutputs,
};
#[cfg(any(feature = "http-source", feature = "grpc-source"))]
use drasi_core_types::models::SourceChange;
#[cfg(feature = "grpc-source")]
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
};
#[cfg(feature = "http-source")]
pub use drasi_http_common::{HttpElement, HttpSourceChange};
#[cfg(feature = "grpc-source")]
use serde_json::Value;
#[cfg(feature = "grpc-source")]
use std::sync::Arc;

#[cfg(any(feature = "http-source", feature = "grpc-source"))]
fn element_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && !value.chars().any(char::is_control),
        "element ID must be nonempty without control characters"
    );
    Ok(())
}
#[cfg(any(feature = "http-source", feature = "grpc-source"))]
fn labels(values: &[String]) -> Result<()> {
    for value in values {
        ensure!(
            !value.is_empty() && !value.chars().any(char::is_control),
            "invalid element label"
        );
    }
    Ok(())
}
#[cfg(feature = "http-source")]
pub fn http_change(mut change: HttpSourceChange, source_id: &str) -> Result<SourceChange> {
    ComponentId::try_new(source_id)?;
    let timestamp = match &mut change {
        HttpSourceChange::Insert { element, timestamp }
        | HttpSourceChange::Update { element, timestamp } => {
            match element {
                HttpElement::Node {
                    id, labels: names, ..
                } => {
                    element_id(id)?;
                    labels(names)?;
                }
                HttpElement::Relation {
                    id,
                    labels: names,
                    from,
                    to,
                    ..
                } => {
                    element_id(id)?;
                    labels(names)?;
                    element_id(from)?;
                    element_id(to)?;
                }
            }
            timestamp
        }
        HttpSourceChange::Delete {
            id,
            labels: names,
            timestamp,
        } => {
            element_id(id)?;
            if let Some(names) = names {
                labels(names)?;
            }
            timestamp
        }
    };
    if timestamp.is_none() {
        *timestamp = Some(u64::try_from(
            chrono::Utc::now()
                .timestamp_nanos_opt()
                .context("system timestamp out of range")?,
        )?);
    }
    // Reuse the existing published DTO converter, including the deliberately
    // different HTTP nested-value and nanoseconds-to-milliseconds semantics.
    drasi_http_common::convert_http_to_source_change(&change, source_id)
}

#[cfg(feature = "grpc-source")]
fn reference(value: &source::ElementReference) -> Result<ElementReference> {
    ComponentId::try_new(value.source_id.as_str())?;
    element_id(&value.element_id)?;
    Ok(ElementReference::new(&value.source_id, &value.element_id))
}
#[cfg(feature = "grpc-source")]
fn metadata(value: Option<source::ElementMetadata>, source_id: &str) -> Result<ElementMetadata> {
    let value = value.context("element metadata is required")?;
    let source_ref = value.reference.context("element reference is required")?;
    ensure!(
        source_ref.source_id == source_id,
        "element source ID does not match configured sourceId"
    );
    labels(&value.labels)?;
    Ok(ElementMetadata {
        reference: reference(&source_ref)?,
        labels: value
            .labels
            .into_iter()
            .map(Arc::from)
            .collect::<Vec<_>>()
            .into(),
        effective_from: value.effective_from / 1_000_000,
    })
}
#[cfg(feature = "grpc-source")]
pub fn grpc_change(change: source::SourceChange, source_id: &str) -> Result<SourceChange> {
    ComponentId::try_new(source_id)?;
    ensure!(
        change.source_id == source_id,
        "event source ID does not match configured sourceId"
    );
    if let Some(timestamp) = &change.timestamp {
        ensure!(
            (0..1_000_000_000).contains(&timestamp.nanos),
            "invalid protobuf timestamp"
        );
    }
    let kind = source::ChangeType::try_from(change.r#type)?;
    match (kind, change.change) {
        (
            source::ChangeType::Insert | source::ChangeType::Update,
            Some(source::source_change::Change::Element(element)),
        ) => {
            let element = match element.element.context("node or relation is required")? {
                source::element::Element::Node(node) => Element::Node {
                    metadata: metadata(node.metadata, source_id)?,
                    properties: properties(node.properties)?,
                },
                source::element::Element::Relation(relation) => Element::Relation {
                    metadata: metadata(relation.metadata, source_id)?,
                    properties: properties(relation.properties)?,
                    in_node: reference(&relation.in_node.context("relation in_node is required")?)?,
                    out_node: reference(
                        &relation.out_node.context("relation out_node is required")?,
                    )?,
                },
            };
            Ok(if kind == source::ChangeType::Insert {
                SourceChange::Insert { element }
            } else {
                SourceChange::update(element)
            })
        }
        (source::ChangeType::Delete, Some(source::source_change::Change::Metadata(value))) => {
            Ok(SourceChange::Delete {
                metadata: metadata(Some(value), source_id)?,
            })
        }
        _ => anyhow::bail!("invalid source change type or missing data"),
    }
}
#[cfg(feature = "grpc-source")]
fn properties(value: Option<prost_types::Struct>) -> Result<ElementPropertyMap> {
    let mut properties = ElementPropertyMap::new();
    if let Some(value) = value {
        for (key, value) in value.fields {
            properties.insert(&key, grpc_value(value)?);
        }
    }
    Ok(properties)
}
#[cfg(feature = "grpc-source")]
fn grpc_value(value: prost_types::Value) -> Result<ElementValue> {
    use prost_types::value::Kind;
    Ok(match value.kind {
        None | Some(Kind::NullValue(_)) => ElementValue::Null,
        Some(Kind::BoolValue(value)) => ElementValue::Bool(value),
        Some(Kind::NumberValue(value)) => {
            ensure!(value.is_finite(), "nonfinite protobuf property");
            if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
                ElementValue::Integer(value as i64)
            } else {
                ElementValue::Float(value.into())
            }
        }
        Some(Kind::StringValue(value)) => ElementValue::String(value.into()),
        // Existing gRPC source encodes nested properties as JSON text rather
        // than ElementValue::List/Object. Preserve it for like-for-like inputs.
        Some(kind @ (Kind::ListValue(_) | Kind::StructValue(_))) => ElementValue::String(
            serde_json::to_string(&proto_json(prost_types::Value { kind: Some(kind) })?)?.into(),
        ),
    })
}
#[cfg(feature = "grpc-source")]
fn proto_json(value: prost_types::Value) -> Result<Value> {
    use prost_types::value::Kind;
    Ok(match value.kind {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::BoolValue(value)) => Value::Bool(value),
        Some(Kind::NumberValue(value)) => Value::Number(
            serde_json::Number::from_f64(value).context("nonfinite nested protobuf property")?,
        ),
        Some(Kind::StringValue(value)) => Value::String(value),
        Some(Kind::ListValue(list)) => Value::Array(
            list.values
                .into_iter()
                .map(proto_json)
                .collect::<Result<_>>()?,
        ),
        Some(Kind::StructValue(object)) => Value::Object(
            object
                .fields
                .into_iter()
                .map(|(name, value)| Ok((name, proto_json(value)?)))
                .collect::<Result<_>>()?,
        ),
    })
}

/// The configured output whose stream carried this sink input.
///
/// The output may be a source, transformer or query port: only the configured
/// binding identifies it, never an annotation carried by the data.
pub fn bound_output<'a>(
    input: &InputEnvelope,
    outputs: &'a SinkOutputs,
) -> Result<BoundOutput<'a>> {
    ensure!(input.port.as_str() == "in", "unexpected sink input port");
    Ok(outputs.output_for(input.envelope.system().stream())?)
}

/// The ordered changes of one standard graph-change input, each with its
/// configured output binding.
///
/// Source, transformer and query output decode identically and no query
/// annotation is read. Each notification states the real operation, the
/// canonical element identity and only the images the record carries; a patch
/// is delivered as a patch.
pub fn notifications(
    input: &InputEnvelope,
    outputs: &SinkOutputs,
) -> Result<Vec<ChangeNotification>> {
    let output = bound_output(input, outputs)?;
    Ok(GraphChangeBatch::from_envelope(&input.envelope)?.notifications(output))
}
