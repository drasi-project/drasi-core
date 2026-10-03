// Copyright 2025 The Drasi Authors.
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

//! Output rendering for the log reaction.
//!
//! This module turns a single [`ResultDiff`] into the line that is written to
//! the console. The logic is kept separate from the reaction's processing loop
//! so it can be unit-tested directly.

use drasi_lib::channels::ResultDiff;
use drasi_lib::reactions::common::{OperationType, TemplateRouting, TemplateSpec};
use handlebars::Handlebars;
use log::warn;
use serde_json::{json, Map, Value};

use crate::config::LogReactionConfig;

/// Build a Handlebars renderer with the standard `{{json value}}` helper.
///
/// The helper serialises its first argument to a JSON string, which lets
/// templates embed entire objects (e.g. `{{json after}}`).
pub(crate) fn build_handlebars() -> Handlebars<'static> {
    let mut handlebars = Handlebars::new();
    handlebars.register_helper(
        "json",
        Box::new(
            |h: &handlebars::Helper,
             _: &Handlebars,
             _: &handlebars::Context,
             _: &mut handlebars::RenderContext,
             out: &mut dyn handlebars::Output|
             -> handlebars::HelperResult {
                if let Some(value) = h.param(0) {
                    let json_str =
                        serde_json::to_string(value.value()).unwrap_or_else(|_| "null".to_string());
                    out.write(&json_str)?;
                }
                Ok(())
            },
        ),
    );
    handlebars
}

/// Human-readable label for an operation type.
fn op_label(op: OperationType) -> &'static str {
    match op {
        OperationType::Add => "ADD",
        OperationType::Update => "UPDATE",
        OperationType::Delete => "DELETE",
    }
}

/// Select the template spec for an operation from a query config.
fn spec_for_op(config: &crate::config::QueryConfig, op: OperationType) -> Option<&TemplateSpec> {
    match op {
        OperationType::Add => config.added.as_ref(),
        OperationType::Update => config.updated.as_ref(),
        OperationType::Delete => config.deleted.as_ref(),
    }
}

/// Resolve the template spec for a query and operation.
///
/// Resolution order (per the Reaction Developer Guide):
/// 1. an exact route entry for the query id,
/// 2. a route entry matching the last dotted segment of the query id,
/// 3. the default template.
fn resolve_spec<'a>(
    config: &'a LogReactionConfig,
    query_id: &str,
    op: OperationType,
) -> Option<&'a TemplateSpec> {
    if let Some(spec) = config
        .routes()
        .get(query_id)
        .and_then(|qc| spec_for_op(qc, op))
    {
        return Some(spec);
    }

    if let Some(segment) = query_id.rsplit('.').next() {
        if segment != query_id {
            if let Some(spec) = config
                .routes()
                .get(segment)
                .and_then(|qc| spec_for_op(qc, op))
            {
                return Some(spec);
            }
        }
    }

    config.default_template().and_then(|qc| spec_for_op(qc, op))
}

/// Render a single [`ResultDiff`] into the line to log.
///
/// Returns `None` for [`ResultDiff::Noop`], which carries no row data and must
/// be treated as a no-op.
///
/// When a non-empty template applies it is rendered with the standard context
/// keys (`query_id`, `query_name`, `operation`, `timestamp`, `sequence_id`,
/// `metadata`, and the applicable `before`/`after`/`data`). If rendering fails,
/// the reaction falls back to the built-in human-readable line and logs a
/// warning; it never silently substitutes unrelated output.
pub(crate) fn render_diff(
    config: &LogReactionConfig,
    handlebars: &Handlebars,
    query_id: &str,
    timestamp: &str,
    sequence_id: u64,
    metadata: &Value,
    diff: &ResultDiff,
) -> Option<String> {
    let (op, default_line) = match diff {
        ResultDiff::Add { data, .. } => (OperationType::Add, format!("[ADD] {data}")),
        ResultDiff::Delete { data, .. } => (OperationType::Delete, format!("[DELETE] {data}")),
        ResultDiff::Update { before, after, .. } => (
            OperationType::Update,
            format!("[UPDATE] {before} -> {after}"),
        ),
        ResultDiff::Aggregation { before, after, .. } => {
            let before_str = before
                .as_ref()
                .map_or_else(|| "null".to_string(), ToString::to_string);
            (
                OperationType::Update,
                format!("[AGGREGATION] {before_str} -> {after}"),
            )
        }
        ResultDiff::Noop => return None,
    };

    // Standard context keys, populated for every render.
    let mut context = Map::new();
    context.insert("query_id".to_string(), json!(query_id));
    context.insert("query_name".to_string(), json!(query_id));
    context.insert("operation".to_string(), json!(op_label(op)));
    context.insert("timestamp".to_string(), json!(timestamp));
    context.insert("sequence_id".to_string(), json!(sequence_id));
    context.insert("metadata".to_string(), metadata.clone());

    match diff {
        ResultDiff::Add { data, .. } => {
            context.insert("after".to_string(), data.clone());
        }
        ResultDiff::Delete { data, .. } => {
            context.insert("before".to_string(), data.clone());
        }
        ResultDiff::Update {
            before,
            after,
            data,
            ..
        } => {
            context.insert("before".to_string(), before.clone());
            context.insert("after".to_string(), after.clone());
            context.insert("data".to_string(), data.clone());
        }
        ResultDiff::Aggregation { before, after, .. } => {
            if let Some(before) = before {
                context.insert("before".to_string(), before.clone());
            }
            context.insert("after".to_string(), after.clone());
        }
        ResultDiff::Noop => return None,
    }

    // A configured-but-empty template means "use the built-in default".
    let template = resolve_spec(config, query_id, op)
        .map(|spec| spec.template.as_str())
        .filter(|template| !template.is_empty());

    match template {
        Some(template) => match handlebars.render_template(template, &context) {
            Ok(rendered) => Some(rendered),
            Err(e) => {
                warn!(
                    "Template render failed for query '{query_id}' ({op}): {e}; \
                     falling back to default line",
                    op = op_label(op),
                );
                Some(default_line)
            }
        },
        None => Some(default_line),
    }
}
