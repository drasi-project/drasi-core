// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{ensure, Context, Result};
use drasi_lib::{
    channels::{QueryResult, ResultDiff},
    reactions::common::{OperationType, TemplateRouting},
};
use handlebars::Handlebars;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{Action, TemporalReactionConfig};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Command {
    pub request_id: String,
    pub action: Action,
    pub payload: Option<Value>,
    pub resolved_run_id: Option<String>,
}

pub(crate) fn engine() -> Handlebars<'static> {
    let mut engine = Handlebars::new();
    engine.set_strict_mode(true);
    engine.register_escape_fn(handlebars::no_escape);
    handlebars::handlebars_helper!(json_value: |value: Value| value.to_string());
    engine.register_helper("json", Box::new(json_value));
    engine
}

pub(crate) fn routing(config: &TemporalReactionConfig, query: &str) -> Result<Value> {
    Ok(json!({
        "endpoint": config.endpoint,
        "namespace": config.namespace,
        "actions": [
            serde_json::to_value(config.output_templates.get_template_spec(query, OperationType::Add))?,
            serde_json::to_value(config.output_templates.get_template_spec(query, OperationType::Update))?,
            serde_json::to_value(config.output_templates.get_template_spec(query, OperationType::Delete))?
        ]
    }))
}

pub(crate) fn commands(
    config: &TemporalReactionConfig,
    result: &QueryResult,
) -> Result<Vec<Command>> {
    let engine = engine();
    let mut commands = Vec::new();
    for diff in &result.results {
        let (op, name, before, after, data) = match diff {
            ResultDiff::Add { data, .. } => {
                (OperationType::Add, "ADD", None, Some(data), Some(data))
            }
            ResultDiff::Update {
                before,
                after,
                data,
                ..
            } => (
                OperationType::Update,
                "UPDATE",
                Some(before),
                Some(after),
                Some(data),
            ),
            ResultDiff::Delete { data, .. } => (
                OperationType::Delete,
                "DELETE",
                Some(data),
                None,
                Some(data),
            ),
            ResultDiff::Aggregation { before, after, .. } => (
                OperationType::Update,
                "UPDATE",
                before.as_ref(),
                Some(after),
                Some(after),
            ),
            ResultDiff::Noop => continue,
        };
        let Some(spec) = config
            .output_templates
            .get_template_spec(&result.query_id, op)
        else {
            continue;
        };
        let id = uuid::Uuid::new_v4().to_string();
        let mut envelope = json!({
            "queryId": result.query_id, "operation": name,
            "sequenceId": result.sequence, "timestamp": result.timestamp.to_rfc3339(),
        });
        if let Some(row) = before {
            envelope["before"] = row.clone();
        }
        if let Some(row) = after {
            envelope["after"] = row.clone();
        }
        if !result.metadata.is_empty() {
            envelope["metadata"] = json!(result.metadata);
        }
        let context = json!({
            "query_id": result.query_id, "query_name": result.query_id,
            "operation": name, "sequence_id": result.sequence,
            "timestamp": result.timestamp.to_rfc3339(), "metadata": result.metadata,
            "before": before, "after": after, "data": data, "row": after.or(before),
            "request_id": id,
        });
        let mut rendered = serde_json::to_value(&spec.extension)?;
        for (field, template, required) in spec.extension.strings() {
            let value = engine
                .render_template(template, &context)
                .with_context(|| {
                    format!(
                        "query '{}' {name}: failed to render {field}",
                        result.query_id
                    )
                })?;
            ensure!(
                !required || !value.trim().is_empty(),
                "rendered {field} must not be empty"
            );
            rendered[field] = Value::String(value);
        }
        let action: Action = serde_json::from_value(rendered)?;
        ensure!(
            action.workflow_id().len() <= 1000,
            "workflowId exceeds 1000 bytes"
        );
        let payload = match &action {
            Action::Cancel { .. } => None,
            Action::Terminate { .. } if spec.template.is_empty() => None,
            _ => Some(if spec.template.is_empty() {
                envelope
            } else {
                match engine
                    .render_template(&spec.template, &context)
                    .map_err(anyhow::Error::from)
                    .and_then(|s| serde_json::from_str(&s).context("payload is not valid JSON"))
                {
                    Ok(payload) => payload,
                    Err(error) => {
                        log::warn!("Temporal payload template failed for query '{}' sequence {}: {error:#}; using the standard diff envelope",
                            result.query_id, result.sequence);
                        envelope
                    }
                }
            }),
        };
        commands.push(Command {
            request_id: id,
            action,
            payload,
            resolved_run_id: None,
        });
    }
    Ok(commands)
}
