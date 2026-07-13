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

//! Handlebars rendering for stored-procedure command templates.
//!
//! The reaction uses the [`handlebars`] engine (as mandated by the Reaction
//! Developer Guide) to render each configured command template against a
//! standard context built from the inbound `QueryResult` and diff.
//!
//! ## SQL-injection safety
//!
//! Unlike a typical body-emitting reaction, a stored-procedure reaction must
//! not inline row values directly into the SQL string. Instead, argument values
//! are referenced with the custom `{{param <expr>}}` helper. At render time this
//! helper resolves the value, appends it to a positional parameter buffer, and
//! emits a `$N` placeholder. The rendered SQL therefore contains only bind
//! placeholders (`$1`, `$2`, …) and the collected values are bound safely by
//! `tokio-postgres`.
//!
//! Ordinary Handlebars expansions (`{{after.id}}`, `{{json after}}`) are still
//! available for literal SQL text — for example a dynamically chosen procedure
//! name — but such values are **not** parameterized and must never carry
//! untrusted input.
//!
//! ## Standard context keys
//!
//! Every render is given the guide's standard keys: `query_id`, `query_name`,
//! `operation`, `timestamp`, `metadata`, and `before` / `after` / `data` as
//! applicable to the operation type.

use anyhow::{anyhow, Result};
use handlebars::Handlebars;
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};

use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::reactions::common::OperationType;

/// The outcome of resolving an inbound diff into a render context.
pub(crate) struct RenderInput {
    /// Operation type used to select the template.
    pub operation: OperationType,
    /// The Handlebars context object.
    pub context: Value,
}

/// Register the canonical `{{json arg}}` helper on a Handlebars registry.
///
/// This lets templates embed arbitrary JSON values without hand-crafting them.
fn register_json_helper(handlebars: &mut Handlebars<'static>) {
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
}

/// Compile a single template string, treating an empty string as valid
/// (meaning "no command configured").
pub(crate) fn validate_template(template: &str) -> Result<()> {
    if template.is_empty() {
        return Ok(());
    }
    handlebars::Template::compile(template)
        .map_err(|e| anyhow!("invalid Handlebars template: {e}"))?;
    Ok(())
}

/// Render a stored-procedure command template into a SQL string and the ordered
/// list of positional bind parameters collected from `{{param …}}` helpers.
///
/// Returns an error if the template fails to render (for example, a referenced
/// field is missing under strict mode). Callers should log the error and skip
/// the event rather than executing partial SQL.
pub(crate) fn render_command(template: &str, context: &Value) -> Result<(String, Vec<Value>)> {
    let params: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);
    register_json_helper(&mut handlebars);

    let params_for_helper = Arc::clone(&params);
    handlebars.register_helper(
        "param",
        Box::new(
            move |h: &handlebars::Helper,
                  _: &Handlebars,
                  _: &handlebars::Context,
                  _: &mut handlebars::RenderContext,
                  out: &mut dyn handlebars::Output|
                  -> handlebars::HelperResult {
                let value = match h.param(0) {
                    Some(v) if !v.is_value_missing() => v.value().clone(),
                    _ => {
                        return Err(handlebars::RenderErrorReason::Other(
                            "`param` helper requires a value; referenced field is missing"
                                .to_string(),
                        )
                        .into());
                    }
                };
                let mut buf = params_for_helper
                    .lock()
                    .expect("param buffer mutex poisoned");
                buf.push(value);
                let placeholder = format!("${}", buf.len());
                out.write(&placeholder)?;
                Ok(())
            },
        ),
    );

    let rendered = handlebars
        .render_template(template, context)
        .map_err(|e| anyhow!("failed to render command template: {e}"))?;

    let collected = params.lock().expect("param buffer mutex poisoned").clone();
    Ok((rendered, collected))
}

/// Build the standard Handlebars context for a single diff, resolving the
/// operation type. Returns `None` for `ResultDiff::Noop`.
pub(crate) fn build_render_input(
    query_result: &QueryResult,
    diff: &ResultDiff,
) -> Option<RenderInput> {
    let timestamp = query_result.timestamp.to_rfc3339();
    let metadata = if query_result.metadata.is_empty() {
        Value::Object(Map::new())
    } else {
        Value::Object(query_result.metadata.clone().into_iter().collect())
    };

    let (operation, op_str, before, after, data) = match diff {
        ResultDiff::Add { data, .. } => (OperationType::Add, "ADD", None, Some(data.clone()), None),
        ResultDiff::Delete { data, .. } => (
            OperationType::Delete,
            "DELETE",
            Some(data.clone()),
            None,
            None,
        ),
        ResultDiff::Update {
            data,
            before,
            after,
            ..
        } => (
            OperationType::Update,
            "UPDATE",
            Some(before.clone()),
            Some(after.clone()),
            Some(data.clone()),
        ),
        ResultDiff::Aggregation { before, after, .. } => (
            OperationType::Update,
            "UPDATE",
            before.clone(),
            Some(after.clone()),
            None,
        ),
        ResultDiff::Noop => return None,
    };

    let mut context = Map::new();
    let query_name = Value::String(query_result.query_id.clone());
    context.insert("query_name".to_string(), query_name.clone());
    context.insert("query_id".to_string(), query_name);
    context.insert("operation".to_string(), Value::String(op_str.to_string()));
    context.insert("timestamp".to_string(), Value::String(timestamp));
    context.insert("metadata".to_string(), metadata);
    if let Some(before) = before {
        context.insert("before".to_string(), before);
    }
    if let Some(after) = after {
        context.insert("after".to_string(), after);
    }
    if let Some(data) = data {
        context.insert("data".to_string(), data);
    }

    Some(RenderInput {
        operation,
        context: Value::Object(context),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn param_helper_collects_positional_bindings() {
        let ctx = json!({ "after": { "id": 1, "name": "Alice" } });
        let (sql, params) = render_command(
            "CALL add_user({{param after.id}}, {{param after.name}})",
            &ctx,
        )
        .unwrap();
        assert_eq!(sql, "CALL add_user($1, $2)");
        assert_eq!(params, vec![json!(1), json!("Alice")]);
    }

    #[test]
    fn param_helper_repeats_values_positionally() {
        let ctx = json!({ "after": { "id": 7 } });
        let (sql, params) =
            render_command("CALL touch({{param after.id}}, {{param after.id}})", &ctx).unwrap();
        assert_eq!(sql, "CALL touch($1, $2)");
        assert_eq!(params, vec![json!(7), json!(7)]);
    }

    #[test]
    fn json_helper_inlines_serialized_value() {
        let ctx = json!({ "after": { "a": 1, "b": [2, 3] } });
        let (sql, params) = render_command("CALL ingest({{param after}})", &ctx).unwrap();
        assert_eq!(sql, "CALL ingest($1)");
        assert_eq!(params, vec![json!({ "a": 1, "b": [2, 3] })]);
    }

    #[test]
    fn missing_field_fails_render_under_strict_mode() {
        let ctx = json!({ "after": { "id": 1 } });
        let result = render_command("CALL add_user({{param after.missing}})", &ctx);
        assert!(result.is_err());
    }

    #[test]
    fn no_params_renders_bare_call() {
        let ctx = json!({ "after": {} });
        let (sql, params) = render_command("CALL refresh_view()", &ctx).unwrap();
        assert_eq!(sql, "CALL refresh_view()");
        assert!(params.is_empty());
    }

    #[test]
    fn validate_template_accepts_empty() {
        assert!(validate_template("").is_ok());
    }

    #[test]
    fn validate_template_rejects_unclosed_expression() {
        assert!(validate_template("CALL x({{param after.id)").is_err());
    }

    #[test]
    fn build_render_input_populates_add_context() {
        use chrono::Utc;
        use std::collections::HashMap;

        let qr = QueryResult {
            query_id: "src.user-query".to_string(),
            sequence: 1,
            timestamp: Utc::now(),
            metadata: HashMap::new(),
            profiling: None,
            results: vec![ResultDiff::Add {
                data: json!({ "id": 1 }),
                row_signature: 0,
            }],
        };
        let input = build_render_input(&qr, &qr.results[0]).unwrap();
        assert_eq!(input.operation, OperationType::Add);
        let ctx = input.context.as_object().unwrap();
        assert_eq!(ctx.get("operation").unwrap(), &json!("ADD"));
        assert_eq!(ctx.get("query_id").unwrap(), &json!("src.user-query"));
        assert_eq!(ctx.get("query_name").unwrap(), &json!("src.user-query"));
        assert!(ctx.contains_key("after"));
        assert!(!ctx.contains_key("before"));
    }

    #[test]
    fn build_render_input_skips_noop() {
        use chrono::Utc;
        use std::collections::HashMap;

        let qr = QueryResult {
            query_id: "q".to_string(),
            sequence: 1,
            timestamp: Utc::now(),
            metadata: HashMap::new(),
            profiling: None,
            results: vec![ResultDiff::Noop],
        };
        assert!(build_render_input(&qr, &qr.results[0]).is_none());
    }
}
