// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! SQL Server command rendering, routing and parameter binding.

mod template;
pub use template::{build_context, render_command, validate_template};

use drasi_reaction_common::{OperationType, QueryConfig, TemplateSpec};
use serde_json::Value;
use std::collections::HashMap;
use tiberius::Query;

/// Resolve a command by full query ID, last dotted segment, then default.
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

/// Bind values in the renderer's `@P1`, `@P2`, ... order.
pub fn bind_parameters<'a>(query: &mut Query<'a>, parameters: &'a [Value]) {
    for param in parameters {
        match param {
            Value::Null => {
                query.bind(None::<&str>);
            }
            Value::Bool(value) => {
                query.bind(*value);
            }
            Value::Number(value) => {
                if let Some(integer) = value.as_i64() {
                    query.bind(integer);
                } else if let Some(unsigned) = value.as_u64() {
                    // SQL BIGINT cannot represent the full unsigned range.
                    query.bind(unsigned.to_string());
                } else if let Some(float) = value.as_f64() {
                    query.bind(float);
                } else {
                    query.bind(value.to_string());
                }
            }
            Value::String(value) => {
                query.bind(value.as_str());
            }
            Value::Array(_) | Value::Object(_) => {
                query.bind(param.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_falls_back_per_operation() {
        let routes = HashMap::from([
            (
                "source.query".to_owned(),
                QueryConfig {
                    added: Some(TemplateSpec::new("full")),
                    ..Default::default()
                },
            ),
            (
                "query".to_owned(),
                QueryConfig {
                    added: Some(TemplateSpec::new("short")),
                    updated: Some(TemplateSpec::new("update")),
                    ..Default::default()
                },
            ),
        ]);
        let default = QueryConfig {
            added: Some(TemplateSpec::new("default")),
            deleted: Some(TemplateSpec::new("delete")),
            ..Default::default()
        };
        for (query, operation, expected) in [
            ("source.query", OperationType::Add, Some("full")),
            ("other.query", OperationType::Add, Some("short")),
            ("source.query", OperationType::Update, Some("update")),
            ("source.query", OperationType::Delete, Some("delete")),
            ("unmatched", OperationType::Add, Some("default")),
            ("unmatched", OperationType::Update, None),
        ] {
            assert_eq!(
                resolve_template(&routes, Some(&default), query, operation)
                    .map(|spec| spec.template.as_str()),
                expected
            );
        }
    }
}
