// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Context, Result};
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::reactions::common::{OperationType, TemplateRouting};
use handlebars::Handlebars;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::config::{Action, DurableTaskReactionConfig, TargetPolicy};

/// A fully rendered operation. Prepared before any RPC in an emission is sent.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Dispatch {
    Start {
        instance_id: String,
        orchestration: String,
        version: String,
        input: String,
        command_id: String,
        on_conflict: TargetPolicy,
    },
    Event {
        instance_id: String,
        event_name: String,
        input: String,
        on_missing: TargetPolicy,
        on_terminal: TargetPolicy,
    },
    Terminate {
        instance_id: String,
        output: String,
        recursive: bool,
        on_missing: TargetPolicy,
    },
}

pub(crate) fn handlebars() -> Handlebars<'static> {
    let mut registry = Handlebars::new();
    registry.set_strict_mode(true);
    registry.register_escape_fn(handlebars::no_escape);
    handlebars::handlebars_helper!(json: |value: Value| value.to_string());
    registry.register_helper("json", Box::new(json));
    registry
}

pub(crate) fn prepare(
    config: &DurableTaskReactionConfig,
    reaction_id: &str,
    emission: &QueryResult,
    registry: &Handlebars<'_>,
) -> Result<Vec<Dispatch>> {
    let mut operations = Vec::new();
    for (index, diff) in emission.results.iter().enumerate() {
        let (operation, before, after, data, signature) = match diff {
            ResultDiff::Noop => continue,
            ResultDiff::Add {
                data,
                row_signature,
            } => (OperationType::Add, None, Some(data), None, row_signature),
            ResultDiff::Update {
                before,
                after,
                data,
                row_signature,
                ..
            } => (
                OperationType::Update,
                Some(before),
                Some(after),
                Some(data),
                row_signature,
            ),
            ResultDiff::Delete {
                data,
                row_signature,
            } => (OperationType::Delete, Some(data), None, None, row_signature),
            ResultDiff::Aggregation {
                before,
                after,
                row_signature,
            } => (
                OperationType::Update,
                before.as_ref(),
                Some(after),
                None,
                row_signature,
            ),
        };
        let Some(spec) = config.get_template_spec(&emission.query_id, operation) else {
            continue;
        };
        if matches!(spec.extension, Action::Ignore) {
            continue;
        }
        let operation = operation.as_str().to_uppercase();
        let identity = serde_json::to_vec(&(
            &config.producer_id,
            reaction_id,
            &emission.query_id,
            emission.sequence,
            index,
            signature,
            &operation,
        ))?;
        let command_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, &identity).to_string();
        let mut envelope = json!({
            "commandId": command_id,
            "queryId": emission.query_id,
            "sequenceId": emission.sequence,
            "timestamp": emission.timestamp.to_rfc3339(),
            "operation": operation,
        });
        if let Some(before) = before {
            envelope["before"] = before.clone();
        }
        if let Some(after) = after {
            envelope["after"] = after.clone();
        }
        if !emission.metadata.is_empty() {
            envelope["metadata"] = json!(emission.metadata);
        }
        let mut context = json!({
            "query_id": emission.query_id,
            "query_name": emission.query_id,
            "sequence_id": emission.sequence,
            "command_id": command_id,
            "operation": operation,
            "timestamp": emission.timestamp.to_rfc3339(),
            "metadata": emission.metadata,
        });
        if let Some(before) = before {
            context["before"] = before.clone();
        }
        if let Some(after) = after {
            context["after"] = after.clone();
        }
        if let Some(data) = data {
            context["data"] = data.clone();
        }
        let input = if spec.template.is_empty() {
            envelope.to_string()
        } else {
            match registry
                .render_template(&spec.template, &context)
                .context("render body")
                .and_then(|text| serde_json::from_str::<Value>(&text).context("body must be JSON"))
            {
                Ok(body) => body.to_string(),
                Err(error) => {
                    log::warn!(
                            "[{reaction_id}] Body template failed for query={} seq={} diff={index}: {error:#}; using canonical envelope",
                            emission.query_id, emission.sequence
                        );
                    envelope.to_string()
                }
            }
        };
        let render = |text: &str| -> Result<String> {
            registry
                .render_template(text, &context)
                .context("render dispatch field")
        };
        let required = |text: &str, field: &str| -> Result<String> {
            let value = render(text)?;
            ensure!(!value.trim().is_empty(), "rendered {field} is empty");
            ensure!(
                !value.chars().any(char::is_control),
                "{field} contains control characters"
            );
            Ok(value)
        };
        let instance = |text: &str| -> Result<String> {
            let value = required(text, "instanceId")?;
            ensure!(
                !value.starts_with('@'),
                "instanceId must not use the durable-entity @ prefix"
            );
            Ok(value)
        };
        operations.push(match &spec.extension {
            Action::Start {
                orchestration,
                version,
                instance_id,
                on_conflict,
            } => Dispatch::Start {
                instance_id: match instance_id {
                    Some(template) => instance(template)?,
                    None => format!("drasi-{command_id}"),
                },
                orchestration: required(orchestration, "orchestration")?,
                version: render(version)?,
                input,
                command_id,
                on_conflict: *on_conflict,
            },
            Action::RaiseEvent {
                instance_id,
                event_name,
                on_missing,
                on_terminal,
            } => Dispatch::Event {
                instance_id: instance(instance_id)?,
                event_name: required(event_name, "eventName")?,
                input,
                on_missing: *on_missing,
                on_terminal: *on_terminal,
            },
            Action::Terminate {
                instance_id,
                recursive,
                on_missing,
            } => Dispatch::Terminate {
                instance_id: instance(instance_id)?,
                output: input,
                recursive: *recursive,
                on_missing: *on_missing,
            },
            Action::Ignore => unreachable!("ignore filtered before rendering"),
        });
    }
    Ok(operations)
}
