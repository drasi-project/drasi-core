// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Parameter-only SQL rendering, routing and typed binding shared by both
//! stored-procedure delivery paths.

mod template;
pub use template::{build_render_input, validate_template, CompiledTemplate, RenderInput};

use anyhow::{Context, Result};
use bytes::BytesMut;
use drasi_computation_contracts::{
    BoundOutput, ChangeEnvelope, ChangeOperation, ChangeRoute, GraphChangeBatch,
};
use drasi_reaction_common::{OperationType, QueryConfig, TemplateSpec};
use rust_decimal::Decimal;
use serde_json::Value;
use std::{collections::HashMap, error::Error};
use tokio_postgres::types::{to_sql_checked, IsNull, ToSql, Type};

pub fn resolve_template<'a>(
    routes: &'a HashMap<String, QueryConfig>,
    default: Option<&'a QueryConfig>,
    query_id: &str,
    operation: OperationType,
) -> Option<&'a TemplateSpec> {
    let select = |config: &'a QueryConfig| match operation {
        OperationType::Add => config.added.as_ref(),
        OperationType::Update => config.updated.as_ref(),
        OperationType::Delete => config.deleted.as_ref(),
    };
    routes
        .get(query_id)
        .and_then(select)
        .or_else(|| {
            routes
                .get(query_id.rsplit('.').next().unwrap_or(query_id))
                .and_then(select)
        })
        .or_else(|| default.and_then(select))
}

/// The template route and context for one operation of standard graph changes.
#[derive(Debug, Clone)]
pub struct ChangeRenderInput {
    /// The configured output alias that selects the route.
    pub alias: String,
    pub operation: OperationType,
    pub context: Value,
}

/// Template context for one operation of standard graph changes.
///
/// A source, transformer or query produces the same data, so the context is
/// built from the operation's records alone and is named by the configured
/// `output` binding (alias, real component and port). It states `operation` (`ADD`, `REPLACE`, `PATCH` or
/// `DELETE`), the canonical `identity`, `sequence`, an RFC 3339 `timestamp` when
/// known, and only the `element`, `before`, `after` and `removedProperties`
/// that the record carries. A patch's `after` has only the properties it sets;
/// a template that reads an absent field fails strictly.
pub fn build_native_render_input(
    envelope: &ChangeEnvelope,
    operation: &ChangeOperation,
    output: BoundOutput<'_>,
) -> Result<ChangeRenderInput> {
    let notification = GraphChangeBatch::operation_notification(envelope, operation, output)?;
    Ok(ChangeRenderInput {
        alias: output.alias.to_owned(),
        operation: match notification.change().route() {
            ChangeRoute::Added => OperationType::Add,
            ChangeRoute::Updated => OperationType::Update,
            ChangeRoute::Deleted => OperationType::Delete,
        },
        context: notification.to_json()?,
    })
}

#[derive(Debug, Clone)]
pub enum SqlParam {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Json(Value),
}

impl From<Value> for SqlParam {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(value),
            Value::Number(value) => {
                if let Some(integer) = value.as_i64() {
                    Self::Int(integer)
                } else if value.is_u64() {
                    Self::Text(value.to_string())
                } else if let Some(float) = value.as_f64() {
                    Self::Float(float)
                } else {
                    Self::Text(value.to_string())
                }
            }
            Value::String(value) => Self::Text(value),
            value => Self::Json(value),
        }
    }
}

impl ToSql for SqlParam {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        match self {
            Self::Null => Ok(IsNull::Yes),
            Self::Bool(value) => value.to_sql_checked(ty, out),
            Self::Int(value) => match *ty {
                Type::INT2 => i16::try_from(*value)?.to_sql_checked(ty, out),
                Type::INT4 => i32::try_from(*value)?.to_sql_checked(ty, out),
                Type::INT8 => value.to_sql_checked(ty, out),
                Type::FLOAT4 => (*value as f32).to_sql_checked(ty, out),
                Type::FLOAT8 => (*value as f64).to_sql_checked(ty, out),
                Type::NUMERIC => Decimal::from(*value).to_sql_checked(ty, out),
                Type::TEXT | Type::VARCHAR => value.to_string().to_sql_checked(ty, out),
                _ => value.to_sql_checked(ty, out),
            },
            Self::Float(value) => match *ty {
                Type::FLOAT4 => {
                    let narrowed = *value as f32;
                    if value.is_finite() && !narrowed.is_finite() {
                        return Err("SQL float exceeds the target float4 range".into());
                    }
                    narrowed.to_sql_checked(ty, out)
                }
                Type::FLOAT8 => value.to_sql_checked(ty, out),
                Type::NUMERIC => value
                    .to_string()
                    .parse::<Decimal>()?
                    .to_sql_checked(ty, out),
                Type::TEXT | Type::VARCHAR => value.to_string().to_sql_checked(ty, out),
                _ => value.to_sql_checked(ty, out),
            },
            Self::Text(value) => match *ty {
                Type::INT2 => value.parse::<i16>()?.to_sql_checked(ty, out),
                Type::INT4 => value.parse::<i32>()?.to_sql_checked(ty, out),
                Type::INT8 => value.parse::<i64>()?.to_sql_checked(ty, out),
                Type::FLOAT4 => value.parse::<f32>()?.to_sql_checked(ty, out),
                Type::FLOAT8 => value.parse::<f64>()?.to_sql_checked(ty, out),
                Type::NUMERIC => value.parse::<Decimal>()?.to_sql_checked(ty, out),
                _ => value.to_sql_checked(ty, out),
            },
            Self::Json(value) => value.to_sql_checked(ty, out),
        }
    }

    fn accepts(ty: &Type) -> bool {
        matches!(
            *ty,
            Type::BOOL
                | Type::INT2
                | Type::INT4
                | Type::INT8
                | Type::FLOAT4
                | Type::FLOAT8
                | Type::NUMERIC
                | Type::TEXT
                | Type::VARCHAR
                | Type::JSON
                | Type::JSONB
        ) || <String as ToSql>::accepts(ty)
    }

    to_sql_checked!();
}

pub async fn execute_parameters(
    client: &(impl tokio_postgres::GenericClient + Sync),
    command: &str,
    parameters: Vec<Value>,
) -> Result<u64> {
    let parameters = parameters
        .into_iter()
        .map(SqlParam::from)
        .collect::<Vec<_>>();
    let references = parameters
        .iter()
        .map(|parameter| parameter as &(dyn ToSql + Sync))
        .collect::<Vec<_>>();
    Ok(client.execute(command, &references).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{
        ComponentId, PortId, QueryChangeCodec, QueryOutputMetadata, ResultNodeProjector,
        SinkOutput, SinkOutputs, StreamId, SystemMetadata,
    };
    use drasi_core_types::evaluation::{
        context::QueryPartEvaluationContext, variable_value::VariableValue,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn parameter_binding_preserves_strings_unsigned_integers_and_numeric_wire_format() {
        let mut bytes = BytesMut::new();
        SqlParam::from(json!("007"))
            .to_sql(&Type::TEXT, &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..], b"007");
        bytes.clear();
        SqlParam::from(json!(u64::MAX))
            .to_sql(&Type::TEXT, &mut bytes)
            .unwrap();
        assert_eq!(&bytes[..], u64::MAX.to_string().as_bytes());
        bytes.clear();
        SqlParam::Int(42)
            .to_sql(&Type::NUMERIC, &mut bytes)
            .unwrap();
        let mut expected = BytesMut::new();
        Decimal::from(42)
            .to_sql(&Type::NUMERIC, &mut expected)
            .unwrap();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn parameter_binding_rejects_truncation_and_incompatible_types() {
        for (parameter, target) in [
            (SqlParam::Int(i64::MAX), Type::INT4),
            (SqlParam::Int(32768), Type::INT2),
            (SqlParam::from(json!(u64::MAX)), Type::INT8),
            (SqlParam::Text("not-a-number".into()), Type::INT4),
            (SqlParam::Float(f64::MAX), Type::FLOAT4),
            (SqlParam::Bool(true), Type::INT4),
        ] {
            assert!(parameter.to_sql(&target, &mut BytesMut::new()).is_err());
        }
    }

    fn test_outputs(alias: &str, component: &str, stream: &str) -> Result<SinkOutputs> {
        Ok(SinkOutputs::new(BTreeMap::from([(
            alias.to_owned(),
            SinkOutput {
                component: ComponentId::try_new(component)?,
                port: PortId::try_new("out")?,
                stream: StreamId::try_new(stream)?,
            },
        )]))?)
    }

    fn row(value: &str) -> BTreeMap<Box<str>, VariableValue> {
        BTreeMap::from([
            ("value".into(), VariableValue::String(value.into())),
            ("nullable".into(), VariableValue::Null),
        ])
    }

    #[test]
    fn native_sql_context_is_the_same_for_query_and_unannotated_graph_changes() -> Result<()> {
        let evaluations = [
            QueryPartEvaluationContext::Adding {
                after: row("added"),
                row_signature: 1,
            },
            QueryPartEvaluationContext::Updating {
                before: row("before"),
                after: row("after"),
                row_signature: 2,
            },
            QueryPartEvaluationContext::Removing {
                before: row("deleted"),
                row_signature: 3,
            },
            QueryPartEvaluationContext::Aggregation {
                before: None,
                after: row("aggregate"),
                grouping_keys: vec!["value".into()],
                default_before: true,
                default_after: false,
                row_signature: 4,
            },
        ];
        let annotated = QueryChangeCodec::encode_evaluation(
            None,
            &ComponentId::try_new("source.query")?,
            SystemMetadata::new(StreamId::try_new("query/out")?, 1),
            &ResultNodeProjector::try_new("test/postgres-common-sql/query/out", None)?,
            &evaluations,
            QueryOutputMetadata {
                query_id: "source.query".into(),
                source_id: Some("postgres".into()),
                timestamp: chrono::DateTime::from_timestamp(10, 123_000_000).unwrap(),
                metadata: HashMap::new(),
                profiling: None,
            },
        )?
        .context("query output")?;
        let plain = ChangeEnvelope::new(
            annotated.id().clone(),
            annotated.changes().clone(),
            (**annotated.system()).clone(),
        );
        let outputs = test_outputs("q", "source.query", "query/out")?;
        let query_output = outputs.output_for(&StreamId::try_new("query/out")?)?;
        // operation, before value, after value, identity key
        let expected = [
            ("ADD", None, Some("added"), "0000000000000001"),
            ("REPLACE", Some("before"), Some("after"), "0000000000000002"),
            ("DELETE", Some("deleted"), None, "0000000000000003"),
            ("REPLACE", None, Some("aggregate"), "0000000000000004"),
        ];
        for (operation, (name, before, after, key)) in
            plain.changes().operations().iter().zip(expected)
        {
            let native = build_native_render_input(&plain, operation, query_output)?;
            let with_annotations = build_native_render_input(&annotated, operation, query_output)?;
            assert_eq!(native.context, with_annotations.context);
            assert_eq!(native.alias, "q");
            assert_eq!(
                native.context["output"],
                json!({"alias": "q", "component": "source.query", "port": "out"})
            );
            assert!(native.context.get("producer").is_none());
            assert_eq!(native.context["operation"], name);
            assert_eq!(native.context["identity"]["id"], key);
            assert_eq!(
                native.context["identity"]["namespace"],
                "test/postgres-common-sql/query/out"
            );
            // An unknown before image is absent, never a null row.
            assert_eq!(native.context["before"]["value"], json!(before));
            assert_eq!(native.context["after"]["value"], json!(after));
            assert_eq!(native.context.get("before").is_some(), before.is_some());
            assert!(native.context.get("row_signature").is_none());
            assert_eq!(native.context["element"]["kind"], "node");
            assert_eq!(native.context["element"]["labels"][0], "Result");
        }
        Ok(())
    }

    #[test]
    fn native_sql_context_delivers_source_replacements_patches_and_deletions() -> Result<()> {
        use drasi_computation_contracts::GraphChangeCodec;
        use drasi_core_types::models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
            SourceChange,
        };
        use std::{collections::BTreeSet, sync::Arc};
        let metadata = ElementMetadata {
            reference: ElementReference::new("db", "7"),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: 2_000,
        };
        let element = |name: &str| {
            let mut properties = ElementPropertyMap::new();
            properties.insert("name", ElementValue::String(name.into()));
            properties.insert("age", ElementValue::Null);
            Element::Node {
                metadata: metadata.clone(),
                properties,
            }
        };
        let envelope = GraphChangeCodec::encode_changes(
            &[
                SourceChange::Replace {
                    element: element("all"),
                },
                SourceChange::Update {
                    element: element("some"),
                    removed_properties: BTreeSet::from(["nickname".to_owned()]),
                },
                SourceChange::Delete {
                    metadata: metadata.clone(),
                },
            ],
            StreamId::try_new("db/out")?,
            3,
            chrono::DateTime::from_timestamp(10, 0),
        )?;
        let outputs = test_outputs("db", "facilities-db", "db/out")?;
        let db_output = outputs.output_for(&StreamId::try_new("db/out")?)?;
        let inputs = envelope
            .changes()
            .operations()
            .iter()
            .map(|operation| build_native_render_input(&envelope, operation, db_output))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(inputs[0].context["operation"], "REPLACE");
        assert_eq!(inputs[0].context["after"]["name"], "all");
        assert_eq!(inputs[1].context["operation"], "PATCH");
        assert_eq!(inputs[1].context["after"]["name"], "some");
        // A null value that a patch sets is kept apart from a removed property.
        assert!(inputs[1].context["after"]["age"].is_null());
        assert_eq!(inputs[1].context["removedProperties"][0], "nickname");
        assert!(inputs[0].context.get("removedProperties").is_none());
        assert_eq!(inputs[2].context["operation"], "DELETE");
        assert_eq!(
            inputs[2].context["identity"],
            json!({"namespace": "db", "id": "7"})
        );
        assert!(
            inputs[2].context.get("before").is_none(),
            "identity-only delete"
        );
        assert!(inputs[2].context.get("after").is_none());
        assert_eq!(
            inputs
                .iter()
                .map(|input| input.operation)
                .collect::<Vec<_>>(),
            [
                OperationType::Update,
                OperationType::Update,
                OperationType::Delete
            ]
        );
        assert!(inputs.iter().all(|input| input.alias == "db"));
        Ok(())
    }
}
