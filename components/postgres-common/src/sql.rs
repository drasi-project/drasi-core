// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Parameter-only SQL rendering, routing and typed binding shared by both
//! stored-procedure delivery paths.

mod template;
pub use template::{build_render_input, validate_template, CompiledTemplate, RenderInput};

use anyhow::{Context, Result};
use bytes::BytesMut;
use drasi_computation_contracts::{
    exchange::{QueryResult, ResultDiff},
    ChangeEnvelope, ChangeOperation, QueryChangeCodec, QueryRowKind, Record, RecordImage,
    UpdateSemantics,
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

pub fn build_native_render_input(
    envelope: &ChangeEnvelope,
    operation: &ChangeOperation,
) -> Result<RenderInput> {
    let metadata = QueryChangeCodec::result_metadata(envelope)?;
    let row = |record: &Record| -> Result<_> {
        anyhow::ensure!(
            record.image() == RecordImage::Full,
            "SQL delivery requires full query rows"
        );
        let row = QueryChangeCodec::decode_row(record)?;
        anyhow::ensure!(
            row.query_id == metadata.query_id,
            "SQL delivery row belongs to another query"
        );
        Ok(row)
    };
    let diff = match operation {
        ChangeOperation::Added { after, .. } => {
            let after = row(after)?;
            anyhow::ensure!(
                after.kind == QueryRowKind::Row,
                "invalid added query row kind"
            );
            ResultDiff::Add {
                data: QueryChangeCodec::row_values_to_json(&after.values),
                row_signature: after.signature,
            }
        }
        ChangeOperation::Updated {
            before,
            after,
            semantics: UpdateSemantics::Replace,
            ..
        } => {
            let after = row(after)?;
            let before = before.as_ref().map(row).transpose()?;
            anyhow::ensure!(
                before
                    .as_ref()
                    .is_none_or(|before| before.query_id == after.query_id
                        && before.signature == after.signature
                        && before.kind == after.kind),
                "SQL delivery update changed row identity or kind"
            );
            match after.kind {
                QueryRowKind::Row => ResultDiff::Update {
                    data: QueryChangeCodec::row_values_to_json(&after.values),
                    before: QueryChangeCodec::row_values_to_json(
                        &before
                            .context("SQL delivery update has no before image")?
                            .values,
                    ),
                    after: QueryChangeCodec::row_values_to_json(&after.values),
                    grouping_keys: None,
                    row_signature: after.signature,
                },
                QueryRowKind::Aggregation { .. } => ResultDiff::Aggregation {
                    before: before.map(|row| QueryChangeCodec::row_values_to_json(&row.values)),
                    after: QueryChangeCodec::row_values_to_json(&after.values),
                    row_signature: after.signature,
                },
            }
        }
        ChangeOperation::Deleted { before, .. } => {
            let before = row(before
                .as_ref()
                .context("SQL delivery delete has no before image")?)?;
            anyhow::ensure!(
                before.kind == QueryRowKind::Row,
                "invalid deleted query row kind"
            );
            ResultDiff::Delete {
                data: QueryChangeCodec::row_values_to_json(&before.values),
                row_signature: before.signature,
            }
        }
        _ => anyhow::bail!("unsupported SQL query operation"),
    };
    let result = QueryResult {
        query_id: metadata.query_id,
        sequence: QueryChangeCodec::query_sequence(envelope)?,
        timestamp: metadata.timestamp,
        results: Vec::new(),
        metadata: metadata.metadata,
        profiling: metadata.profiling,
    };
    build_render_input(&result, &diff).context("SQL query operation has no render context")
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
    use drasi_computation_contracts::{ComponentId, QueryOutputMetadata, StreamId, SystemMetadata};
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

    #[test]
    fn native_sql_context_preserves_every_existing_result_diff_projection() -> Result<()> {
        let row = |value: &str| {
            BTreeMap::from([
                ("value".into(), VariableValue::String(value.into())),
                ("nullable".into(), VariableValue::Null),
            ])
        };
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
        let envelope = QueryChangeCodec::encode_evaluation(
            None,
            &ComponentId::try_new("source.query")?,
            SystemMetadata::new(StreamId::try_new("query/out")?, 1),
            &evaluations,
            QueryOutputMetadata {
                query_id: "source.query".into(),
                source_id: Some("postgres".into()),
                timestamp: chrono::DateTime::from_timestamp(10, 123_000_000).unwrap(),
                metadata: HashMap::from([("custom".into(), json!({"keep": true}))]),
                profiling: None,
            },
        )?
        .context("query output")?;
        let established = QueryChangeCodec::to_legacy_result(&envelope)?;
        assert_eq!(established.results.len(), 4);
        for (operation, diff) in envelope
            .changes()
            .operations()
            .iter()
            .zip(&established.results)
        {
            let native = build_native_render_input(&envelope, operation)?;
            let original = build_render_input(&established, diff).context("result context")?;
            assert_eq!(native.query_id, original.query_id);
            assert_eq!(native.operation, original.operation);
            assert_eq!(native.context, original.context);
        }
        Ok(())
    }
}
