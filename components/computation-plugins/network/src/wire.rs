// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Network-only conversion. Graph events stay typed until the transport boundary;
//! no legacy SourceEventWrapper, QueryResult or adapter queue is constructed.

use crate::{
    config::OutputFormat,
    proto::{reaction, source},
};
use anyhow::{ensure, Context, Result};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use drasi_lib::computation::v1::{
    ChangeOperation, ComponentId, InputEnvelope, QueryChangeCodec, QueryRowKind, Record,
    RecordImage, StreamId, UpdateSemantics,
};
pub use drasi_source_http::{HttpElement, HttpSourceChange};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

fn element_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && !value.chars().any(char::is_control),
        "element ID must be nonempty without control characters"
    );
    Ok(())
}
fn labels(values: &[String]) -> Result<()> {
    for value in values {
        ensure!(
            !value.is_empty() && !value.chars().any(char::is_control),
            "invalid element label"
        );
    }
    Ok(())
}
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
    drasi_source_http::convert_http_to_source_change(&change, source_id)
}

fn reference(value: &source::ElementReference) -> Result<ElementReference> {
    ComponentId::try_new(value.source_id.as_str())?;
    element_id(&value.element_id)?;
    Ok(ElementReference::new(&value.source_id, &value.element_id))
}
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
                SourceChange::Update { element }
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
fn properties(value: Option<prost_types::Struct>) -> Result<ElementPropertyMap> {
    let mut properties = ElementPropertyMap::new();
    if let Some(value) = value {
        for (key, value) in value.fields {
            properties.insert(&key, grpc_value(value)?);
        }
    }
    Ok(properties)
}
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Operation {
    Add,
    Update,
    Delete,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub operation: Operation,
    pub query_id: String,
    pub sequence_id: u64,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub before: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub after: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<Value>,
    #[serde(skip)]
    pub row_signature: u64,
}

fn row(record: &Record, query_id: &str) -> Result<drasi_lib::computation::v1::DecodedQueryRow> {
    ensure!(
        record.image() == RecordImage::Full,
        "query output requires full row images"
    );
    let row = QueryChangeCodec::decode_row(record)?;
    ensure!(
        row.query_id == query_id
            && record.identity().value().as_ref() == row.signature.to_be_bytes(),
        "query row identity does not match configured queryId"
    );
    Ok(row)
}
pub fn notifications(
    input: &InputEnvelope,
    query_id: &str,
    stream: &StreamId,
) -> Result<Vec<Notification>> {
    let envelope = &input.envelope;
    ensure!(
        input.port.as_str() == "in" && envelope.system().stream() == stream,
        "unexpected sink input port/stream"
    );
    ensure!(
        envelope.changes().schema() == QueryChangeCodec::schema().descriptor(),
        "expected query-row schema"
    );
    ensure!(
        !QueryChangeCodec::is_snapshot(envelope) && !QueryChangeCodec::is_progress_only(envelope),
        "native network sink does not implement snapshot/recovery control"
    );
    let metadata = QueryChangeCodec::metadata(envelope)?;
    ensure!(
        metadata.query_id == query_id,
        "query output metadata does not match configured queryId"
    );
    let sequence = QueryChangeCodec::query_sequence(envelope)?;
    let mut output = Vec::with_capacity(envelope.changes().operations().len());
    for operation in envelope.changes().operations() {
        let (kind, signature, before, after) = match operation {
            ChangeOperation::Added { after, .. } => {
                let row = row(after, query_id)?;
                ensure!(
                    row.kind == QueryRowKind::Row,
                    "aggregation must be a query update"
                );
                (
                    Operation::Add,
                    row.signature,
                    None,
                    Some(QueryChangeCodec::row_values_to_json(&row.values)),
                )
            }
            ChangeOperation::Updated {
                before,
                after,
                semantics: UpdateSemantics::Replace,
                ..
            } => {
                let after = row(after, query_id)?;
                let before = before
                    .as_ref()
                    .map(|record| row(record, query_id))
                    .transpose()?;
                ensure!(
                    before
                        .as_ref()
                        .is_none_or(|before| before.kind == after.kind
                            && before.signature == after.signature),
                    "inconsistent query before/after identity"
                );
                ensure!(
                    before.is_some() || matches!(after.kind, QueryRowKind::Aggregation { .. }),
                    "query update requires before image"
                );
                (
                    Operation::Update,
                    after.signature,
                    before
                        .as_ref()
                        .map(|before| QueryChangeCodec::row_values_to_json(&before.values)),
                    Some(QueryChangeCodec::row_values_to_json(&after.values)),
                )
            }
            ChangeOperation::Deleted {
                identity,
                before: Some(before),
                ..
            } => {
                ensure!(
                    before.reference() == identity,
                    "query delete identity mismatch"
                );
                let row = row(before, query_id)?;
                (
                    Operation::Delete,
                    row.signature,
                    Some(QueryChangeCodec::row_values_to_json(&row.values)),
                    None,
                )
            }
            _ => anyhow::bail!("unsupported query operation or missing before image"),
        };
        output.push(Notification {
            operation: kind,
            query_id: metadata.query_id.clone(),
            sequence_id: sequence,
            timestamp: metadata.timestamp.to_rfc3339(),
            before,
            after,
            metadata: (!metadata.metadata.is_empty())
                .then(|| Value::Object(metadata.metadata.clone().into_iter().collect())),
            row_signature: signature,
        });
    }
    Ok(output)
}

pub fn proto_item(
    notification: &Notification,
    format: OutputFormat,
) -> Result<reaction::QueryResultItem> {
    use drasi_reaction_grpc::helpers::convert_json_to_proto_struct;
    let timestamp = chrono::DateTime::parse_from_rfc3339(&notification.timestamp)?;
    let payload = if matches!(format, OutputFormat::CanonicalJson) {
        let mut canonical = serde_json::to_value(notification)?;
        canonical["rowSignature"] = Value::from(notification.row_signature);
        Some(convert_json_to_proto_struct(&canonical))
    } else {
        None
    };
    Ok(reaction::QueryResultItem {
        item_type: match notification.operation {
            Operation::Add => reaction::QueryResultItemType::Add,
            Operation::Update => reaction::QueryResultItemType::Update,
            Operation::Delete => reaction::QueryResultItemType::Delete,
        } as i32,
        row_signature: notification.row_signature,
        before: notification
            .before
            .as_ref()
            .map(convert_json_to_proto_struct),
        after: notification
            .after
            .as_ref()
            .map(convert_json_to_proto_struct),
        sequence: notification.sequence_id,
        timestamp: Some(prost_types::Timestamp {
            seconds: timestamp.timestamp(),
            nanos: timestamp.timestamp_subsec_nanos() as i32,
        }),
        metadata: notification
            .metadata
            .as_ref()
            .map(convert_json_to_proto_struct),
        payload,
    })
}
