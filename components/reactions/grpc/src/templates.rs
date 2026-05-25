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

//! Handlebars-based output template engine for the gRPC reaction.
//!
//! When a [`TemplateSpec`] is registered for a `(query_id, OperationType)`
//! tuple, the rendered JSON string is parsed into a `serde_json::Value` and
//! used as the `data` field of the emitted `ProtoQueryResultItem`. The
//! `before` and `after` fields always carry the raw payload — only `data`
//! is template-rendered.

use drasi_lib::channels::ResultDiff;
use drasi_lib::reactions::common::{OperationType, TemplateRouting, TemplateSpec};
use handlebars::Handlebars;
use log::warn;
use serde_json::{json, Map, Value};

use crate::config::GrpcReactionConfig;
use crate::helpers::convert_json_to_proto_struct;
use crate::proto::ProtoQueryResultItem;

/// Cached Handlebars renderer.
///
/// Handlebars templates are stateless given the same template string, so
/// we keep a single `Handlebars` instance per runner task and call
/// `render_template` for each event. This avoids per-event allocation of a
/// new engine while still being safe to share across awaits within a single
/// task.
pub(crate) struct TemplateEngine {
    handlebars: Handlebars<'static>,
}

impl TemplateEngine {
    pub(crate) fn new() -> Self {
        let mut handlebars = Handlebars::new();
        // Avoid runtime panics from misspelled placeholders — render a
        // visible marker instead so users notice during testing.
        handlebars.set_strict_mode(false);
        Self { handlebars }
    }

    /// Render the template against the diff, returning the JSON value that
    /// should replace `ProtoQueryResultItem.data`.
    ///
    /// On render failure the function falls back to the original `data`
    /// payload of the diff and logs a warning. Events are never dropped due
    /// to a template error.
    pub(crate) fn render_data(
        &self,
        query_id: &str,
        spec: &TemplateSpec,
        diff: &ResultDiff,
    ) -> Value {
        let context = build_context(diff);
        match self.handlebars.render_template(&spec.template, &context) {
            Ok(rendered) => match serde_json::from_str::<Value>(&rendered) {
                Ok(value) => value,
                Err(parse_err) => {
                    warn!(
                        "Template for query '{query_id}' rendered non-JSON output: {parse_err}; \
                         falling back to raw payload"
                    );
                    raw_data(diff)
                }
            },
            Err(render_err) => {
                warn!(
                    "Template render failed for query '{query_id}': {render_err}; falling back to raw payload"
                );
                raw_data(diff)
            }
        }
    }
}

fn raw_data(diff: &ResultDiff) -> Value {
    match diff {
        ResultDiff::Add { data, .. } | ResultDiff::Delete { data, .. } => data.clone(),
        ResultDiff::Update { data, .. } => data.clone(),
        ResultDiff::Aggregation { .. } | ResultDiff::Noop => {
            serde_json::to_value(diff).unwrap_or(Value::Null)
        }
    }
}

/// Build the Handlebars context map for a single `ResultDiff`.
///
/// The shape (`before` / `after` / `data` / `operation` / `query_id`)
/// matches the SSE and HTTP reactions, so existing template authors can
/// reuse familiar placeholders.
fn build_context(diff: &ResultDiff) -> Map<String, Value> {
    let mut ctx = Map::new();
    match diff {
        ResultDiff::Add { data, .. } => {
            ctx.insert("operation".into(), json!("add"));
            ctx.insert("data".into(), data.clone());
            ctx.insert("after".into(), data.clone());
        }
        ResultDiff::Delete { data, .. } => {
            ctx.insert("operation".into(), json!("delete"));
            ctx.insert("data".into(), data.clone());
            ctx.insert("before".into(), data.clone());
        }
        ResultDiff::Update {
            data,
            before,
            after,
            ..
        } => {
            ctx.insert("operation".into(), json!("update"));
            ctx.insert("data".into(), data.clone());
            ctx.insert("before".into(), before.clone());
            ctx.insert("after".into(), after.clone());
        }
        ResultDiff::Aggregation { before, after, .. } => {
            ctx.insert("operation".into(), json!("aggregation"));
            if let Some(b) = before {
                ctx.insert("before".into(), b.clone());
            }
            ctx.insert("after".into(), after.clone());
        }
        ResultDiff::Noop => {
            ctx.insert("operation".into(), json!("noop"));
        }
    }
    ctx
}

/// Convert an [`OperationType`] back into the type-tag string we emit on
/// `ProtoQueryResultItem.r#type`. Currently exposed only for tests.
#[cfg(test)]
pub(crate) fn op_to_type_tag(op: OperationType) -> &'static str {
    match op {
        OperationType::Add => "ADD",
        OperationType::Update => "UPDATE",
        OperationType::Delete => "DELETE",
    }
}

/// Map a `ResultDiff` to its canonical [`OperationType`] for template
/// lookup. Returns `None` for diffs that do not map to a CRUD operation
/// (Noop, Aggregation) — those bypass template rendering.
pub(crate) fn diff_to_op(diff: &ResultDiff) -> Option<OperationType> {
    match diff {
        ResultDiff::Add { .. } => Some(OperationType::Add),
        ResultDiff::Update { .. } => Some(OperationType::Update),
        ResultDiff::Delete { .. } => Some(OperationType::Delete),
        ResultDiff::Aggregation { .. } | ResultDiff::Noop => None,
    }
}

/// Build the proto item that should be emitted for a single `ResultDiff`.
///
/// If a template is configured for the `(query_id, operation)` tuple, the
/// template is rendered and used as the `data` payload; otherwise the raw
/// diff payload is used. `before` and `after` always carry the raw diff
/// values so downstream consumers can recover the underlying change.
pub(crate) fn build_proto_item(
    cfg: &GrpcReactionConfig,
    engine: Option<&TemplateEngine>,
    query_id: &str,
    diff: &ResultDiff,
) -> ProtoQueryResultItem {
    let (type_tag, raw_data_value, before, after) = match diff {
        ResultDiff::Add { data, .. } => ("ADD", data.clone(), None, None),
        ResultDiff::Delete { data, .. } => ("DELETE", data.clone(), None, None),
        ResultDiff::Update {
            data,
            before,
            after,
            ..
        } => (
            "UPDATE",
            data.clone(),
            Some(before.clone()),
            Some(after.clone()),
        ),
        ResultDiff::Aggregation { before, after, .. } => (
            "aggregation",
            serde_json::to_value(diff).unwrap_or(Value::Null),
            before.clone(),
            Some(after.clone()),
        ),
        ResultDiff::Noop => (
            "noop",
            serde_json::to_value(diff).unwrap_or(Value::Null),
            None,
            None,
        ),
    };

    let data_value = if let (Some(engine), Some(op)) = (engine, diff_to_op(diff)) {
        if let Some(spec) = cfg.get_template_spec(query_id, op) {
            // If `data` template is empty fall back to raw payload — empty
            // templates render an empty string which is not valid JSON.
            if spec.template.trim().is_empty() {
                raw_data_value
            } else {
                engine.render_data(query_id, spec, diff)
            }
        } else {
            raw_data_value
        }
    } else {
        raw_data_value
    };

    ProtoQueryResultItem {
        r#type: type_tag.to_string(),
        data: Some(convert_json_to_proto_struct(&data_value)),
        before: before.as_ref().map(convert_json_to_proto_struct),
        after: after.as_ref().map(convert_json_to_proto_struct),
    }
}
