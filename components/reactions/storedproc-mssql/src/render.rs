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

//! Handlebars-based rendering of stored procedure commands.
//!
//! A command template is a Handlebars string that renders to a SQL batch
//! (typically an `EXEC`/`CALL` statement). Two helpers are provided:
//!
//! - `{{param <path>}}` binds the value at `<path>` as a positional SQL
//!   parameter and renders the matching placeholder (`@P1`, `@P2`, …). This is
//!   the safe way to pass row data into a procedure: values are bound through
//!   the driver, never interpolated into the SQL text, so they cannot alter the
//!   statement regardless of their contents.
//! - `{{json <path>}}` renders the value at `<path>` as a JSON string. Use it
//!   as the argument to `{{param}}` — e.g. `{{param (json after)}}` — to bind an
//!   object as a JSON string parameter.
//!
//!   **Warning:** `{{json}}` writes its result into the template output as
//!   plain text, and JSON does not escape SQL metacharacters (for example the
//!   single quote in `O'Brien`). Using it directly inside a SQL literal, e.g.
//!   `EXEC proc '{{json after}}'`, interpolates unescaped data into the SQL
//!   text and reintroduces the injection risk this module exists to prevent.
//!   Always bind it instead: `EXEC proc {{param (json after)}}`.
//!
//! Rendering runs in Handlebars strict mode, so a template that references a
//! field which is absent from the current row fails to render rather than
//! silently binding an empty value. Both the `param` and `json` helpers also
//! reject a missing field explicitly, since Handlebars strict mode does not
//! apply to values passed as helper arguments. The caller treats a render
//! failure as a skipped event (logged) instead of executing a command built
//! from missing data.

use anyhow::{anyhow, Result};
use handlebars::{
    Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason,
};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};

use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::reactions::common::OperationType;

/// Uppercase wire string for an operation, matching the developer guide.
fn operation_str(operation: OperationType) -> &'static str {
    match operation {
        OperationType::Add => "ADD",
        OperationType::Update => "UPDATE",
        OperationType::Delete => "DELETE",
    }
}

/// Build the standard template context for a single diff item.
///
/// Populates the developer-guide-required keys for every render: `query_name`,
/// `query_id`, `operation`, `timestamp`, and `metadata`, plus `before` /
/// `after` / `data` as applicable to the operation. Returns `None` for
/// [`ResultDiff::Noop`], which produces no command.
pub(crate) fn build_context(
    query_result: &QueryResult,
    diff: &ResultDiff,
) -> Option<(OperationType, Map<String, Value>)> {
    let mut context = Map::new();

    let query_name = Value::String(query_result.query_id.clone());
    context.insert("query_name".to_string(), query_name.clone());
    context.insert("query_id".to_string(), query_name);
    context.insert(
        "timestamp".to_string(),
        Value::String(query_result.timestamp.to_rfc3339()),
    );
    context.insert(
        "metadata".to_string(),
        serde_json::to_value(&query_result.metadata).unwrap_or_else(|_| Value::Object(Map::new())),
    );

    let operation = match diff {
        ResultDiff::Add { data, .. } => {
            context.insert("after".to_string(), data.clone());
            OperationType::Add
        }
        ResultDiff::Delete { data, .. } => {
            context.insert("before".to_string(), data.clone());
            OperationType::Delete
        }
        ResultDiff::Update {
            data,
            before,
            after,
            ..
        } => {
            context.insert("before".to_string(), before.clone());
            context.insert("after".to_string(), after.clone());
            context.insert("data".to_string(), data.clone());
            OperationType::Update
        }
        ResultDiff::Aggregation { before, after, .. } => {
            if let Some(before) = before {
                context.insert("before".to_string(), before.clone());
            }
            context.insert("after".to_string(), after.clone());
            OperationType::Update
        }
        ResultDiff::Noop => return None,
    };

    context.insert(
        "operation".to_string(),
        Value::String(operation_str(operation).to_string()),
    );

    Some((operation, context))
}

/// Register the `{{json}}` helper: renders its argument as a JSON string.
///
/// **Warning:** this helper writes the serialized value into the template
/// output *as plain text*. JSON does not escape SQL metacharacters (e.g. the
/// single quote in `O'Brien`), so using `{{json <path>}}` directly inside a SQL
/// literal reintroduces SQL injection. Always combine it with `{{param}}` so the
/// value is bound through the driver rather than interpolated:
///
/// ```text
/// SAFE:   EXEC proc {{param (json after)}}
/// UNSAFE: EXEC proc '{{json after}}'
/// ```
fn register_json_helper(handlebars: &mut Handlebars<'static>) {
    handlebars.register_helper(
        "json",
        Box::new(
            |h: &Helper,
             _: &Handlebars,
             _: &Context,
             _: &mut RenderContext,
             out: &mut dyn Output|
             -> HelperResult {
                let value = h
                    .param(0)
                    .ok_or(RenderErrorReason::ParamNotFoundForIndex("json", 0))?;
                // Strict binding: a referenced field that is absent from the
                // current row must fail the render rather than serializing to
                // the string "null" (which would either be interpolated into
                // the SQL text for `{{json missing}}` or bound as the literal
                // string "null" for `{{param (json missing)}}`). Handlebars
                // strict mode does not apply to values passed as helper
                // arguments, so enforce it here as the `param` helper does.
                if value.is_value_missing() {
                    return Err(RenderErrorReason::Other(format!(
                        "json references a field that is missing from the current row: {}",
                        value
                            .context_path()
                            .map(|p| p.join("."))
                            .unwrap_or_else(|| "<unknown>".to_string())
                    ))
                    .into());
                }
                let rendered =
                    serde_json::to_string(value.value()).unwrap_or_else(|_| "null".to_string());
                out.write(&rendered)?;
                Ok(())
            },
        ),
    );
}

/// Register the `{{param}}` helper against a shared, ordered parameter buffer.
///
/// Each invocation appends the resolved argument value to `params` and writes
/// the matching positional placeholder (`@P{n}`) to the output.
fn register_param_helper(handlebars: &mut Handlebars<'static>, params: Arc<Mutex<Vec<Value>>>) {
    handlebars.register_helper(
        "param",
        Box::new(
            move |h: &Helper,
                  _: &Handlebars,
                  _: &Context,
                  _: &mut RenderContext,
                  out: &mut dyn Output|
                  -> HelperResult {
                let param = h
                    .param(0)
                    .ok_or(RenderErrorReason::ParamNotFoundForIndex("param", 0))?;
                // Strict binding: a referenced field that is absent from the
                // current row must fail the render rather than silently binding
                // a missing/empty value. (Handlebars strict mode does not apply
                // to values passed as helper arguments, so enforce it here.)
                if param.is_value_missing() {
                    return Err(RenderErrorReason::Other(format!(
                        "param references a field that is missing from the current row: {}",
                        param
                            .context_path()
                            .map(|p| p.join("."))
                            .unwrap_or_else(|| "<unknown>".to_string())
                    ))
                    .into());
                }
                let value = param.value().clone();
                let mut buffer = params
                    .lock()
                    .map_err(|e| RenderErrorReason::Other(format!("param buffer poisoned: {e}")))?;
                buffer.push(value);
                // MS SQL / tiberius positional placeholders are 1-based `@P1`, `@P2`, …
                let placeholder = format!("@P{}", buffer.len());
                out.write(&placeholder)?;
                Ok(())
            },
        ),
    );
}

/// Build a strict-mode Handlebars registry with the `json` and `param` helpers.
///
/// The `param` helper appends to `params` as a side effect of rendering, so a
/// fresh buffer (and therefore a fresh registry) is used for every render.
fn build_handlebars(params: Arc<Mutex<Vec<Value>>>) -> Handlebars<'static> {
    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);
    register_json_helper(&mut handlebars);
    register_param_helper(&mut handlebars, params);
    handlebars
}

/// Compile a command template, treating an empty template as valid (no-op).
///
/// Called at construction time so misconfiguration fails before the reaction
/// starts rather than on the first inbound event.
pub(crate) fn validate_template(template: &str) -> Result<()> {
    if template.is_empty() {
        return Ok(());
    }
    handlebars::Template::compile(template)
        .map(|_| ())
        .map_err(|e| anyhow!("invalid Handlebars command template: {e}"))
}

/// Render a command template against a context, returning the rendered SQL and
/// the ordered list of bound parameter values (in `@P1`, `@P2`, … order).
pub(crate) fn render_command(
    template: &str,
    context: &Map<String, Value>,
) -> Result<(String, Vec<Value>)> {
    let params = Arc::new(Mutex::new(Vec::new()));
    let handlebars = build_handlebars(Arc::clone(&params));

    let command = handlebars
        .render_template(template, context)
        .map_err(|e| anyhow!("failed to render command template: {e}"))?;

    // The registry (and its captured clone of `params`) is dropped at the end
    // of this function; extract the collected values before it goes out of use.
    let bound = params
        .lock()
        .map_err(|e| anyhow!("param buffer poisoned: {e}"))?
        .clone();

    Ok((command, bound))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn query_result(query_id: &str, diff: ResultDiff) -> QueryResult {
        QueryResult::new(
            query_id.to_string(),
            1,
            chrono::Utc::now(),
            vec![diff],
            HashMap::new(),
        )
    }

    #[test]
    fn add_context_has_after_and_standard_keys() {
        let qr = query_result(
            "source.sensors",
            ResultDiff::Add {
                data: json!({"id": "s1", "temp": 20.5}),
                row_signature: 0,
            },
        );
        let (op, ctx) = build_context(&qr, &qr.results[0]).unwrap();
        assert_eq!(op, OperationType::Add);
        assert_eq!(ctx["operation"], json!("ADD"));
        assert_eq!(ctx["query_name"], json!("source.sensors"));
        assert_eq!(ctx["query_id"], json!("source.sensors"));
        assert_eq!(ctx["after"], json!({"id": "s1", "temp": 20.5}));
        assert!(ctx.contains_key("timestamp"));
        assert!(ctx.contains_key("metadata"));
        assert!(!ctx.contains_key("before"));
    }

    #[test]
    fn update_context_has_before_after_data() {
        let qr = query_result(
            "q",
            ResultDiff::Update {
                data: json!({"op": "u"}),
                before: json!({"id": 1, "v": "old"}),
                after: json!({"id": 1, "v": "new"}),
                grouping_keys: None,
                row_signature: 0,
            },
        );
        let (op, ctx) = build_context(&qr, &qr.results[0]).unwrap();
        assert_eq!(op, OperationType::Update);
        assert_eq!(ctx["operation"], json!("UPDATE"));
        assert_eq!(ctx["before"], json!({"id": 1, "v": "old"}));
        assert_eq!(ctx["after"], json!({"id": 1, "v": "new"}));
        assert_eq!(ctx["data"], json!({"op": "u"}));
    }

    #[test]
    fn delete_context_has_before() {
        let qr = query_result(
            "q",
            ResultDiff::Delete {
                data: json!({"id": 7}),
                row_signature: 0,
            },
        );
        let (op, ctx) = build_context(&qr, &qr.results[0]).unwrap();
        assert_eq!(op, OperationType::Delete);
        assert_eq!(ctx["before"], json!({"id": 7}));
        assert!(!ctx.contains_key("after"));
    }

    #[test]
    fn noop_context_is_none() {
        let qr = query_result("q", ResultDiff::Noop);
        assert!(build_context(&qr, &qr.results[0]).is_none());
    }

    #[test]
    fn aggregation_with_before_has_before_after_and_no_data() {
        let qr = query_result(
            "q",
            ResultDiff::Aggregation {
                before: Some(json!({"key": "a", "count": 1})),
                after: json!({"key": "a", "count": 2}),
                row_signature: 0,
            },
        );
        let (op, ctx) = build_context(&qr, &qr.results[0]).unwrap();
        // An aggregation is reported as an UPDATE.
        assert_eq!(op, OperationType::Update);
        assert_eq!(ctx["operation"], json!("UPDATE"));
        assert_eq!(ctx["before"], json!({"key": "a", "count": 1}));
        assert_eq!(ctx["after"], json!({"key": "a", "count": 2}));
        // Unlike Update, an Aggregation carries no raw `data` payload.
        assert!(!ctx.contains_key("data"));
    }

    #[test]
    fn aggregation_without_before_omits_before_key() {
        let qr = query_result(
            "q",
            ResultDiff::Aggregation {
                before: None,
                after: json!({"key": "a", "count": 1}),
                row_signature: 0,
            },
        );
        let (op, ctx) = build_context(&qr, &qr.results[0]).unwrap();
        assert_eq!(op, OperationType::Update);
        assert_eq!(ctx["after"], json!({"key": "a", "count": 1}));
        // With no prior value the `before` key is absent, so a template that
        // references `before` fails to render (strict mode) rather than binding
        // a null. Confirm that intended behaviour.
        assert!(!ctx.contains_key("before"));
        assert!(render_command("EXEC agg {{param after.count}}", &ctx).is_ok());
        assert!(render_command("EXEC agg {{param before.count}}", &ctx).is_err());
    }

    #[test]
    fn render_binds_positional_params_in_order() {
        let ctx = build_context(
            &query_result(
                "q",
                ResultDiff::Add {
                    data: json!({"id": 42, "name": "Alice", "email": "a@example.com"}),
                    row_signature: 0,
                },
            ),
            &ResultDiff::Add {
                data: json!({"id": 42, "name": "Alice", "email": "a@example.com"}),
                row_signature: 0,
            },
        )
        .unwrap()
        .1;

        let (sql, params) = render_command(
            "EXEC add_user {{param after.id}}, {{param after.name}}",
            &ctx,
        )
        .unwrap();
        assert_eq!(sql, "EXEC add_user @P1, @P2");
        assert_eq!(params, vec![json!(42), json!("Alice")]);
    }

    #[test]
    fn render_repeated_param_binds_each_occurrence() {
        let (_op, ctx) = build_context(
            &query_result(
                "q",
                ResultDiff::Add {
                    data: json!({"id": 1, "name": "Bob"}),
                    row_signature: 0,
                },
            ),
            &ResultDiff::Add {
                data: json!({"id": 1, "name": "Bob"}),
                row_signature: 0,
            },
        )
        .unwrap();

        let (sql, params) = render_command(
            "EXEC upd {{param after.id}}, {{param after.name}}, {{param after.id}}",
            &ctx,
        )
        .unwrap();
        assert_eq!(sql, "EXEC upd @P1, @P2, @P3");
        assert_eq!(params, vec![json!(1), json!("Bob"), json!(1)]);
    }

    #[test]
    fn render_json_helper_embeds_object() {
        let (_op, ctx) = build_context(
            &query_result(
                "q",
                ResultDiff::Add {
                    data: json!({"id": 1, "tags": ["a", "b"]}),
                    row_signature: 0,
                },
            ),
            &ResultDiff::Add {
                data: json!({"id": 1, "tags": ["a", "b"]}),
                row_signature: 0,
            },
        )
        .unwrap();

        // `{{param (json after)}}` binds the object serialized to a JSON string.
        let (sql, params) = render_command("EXEC ingest {{param (json after)}}", &ctx).unwrap();
        assert_eq!(sql, "EXEC ingest @P1");
        assert_eq!(params, vec![json!("{\"id\":1,\"tags\":[\"a\",\"b\"]}")]);
    }

    #[test]
    fn render_missing_field_errors_in_strict_mode() {
        let (_op, ctx) = build_context(
            &query_result(
                "q",
                ResultDiff::Add {
                    data: json!({"id": 1}),
                    row_signature: 0,
                },
            ),
            &ResultDiff::Add {
                data: json!({"id": 1}),
                row_signature: 0,
            },
        )
        .unwrap();

        let result = render_command("EXEC add_user {{param after.missing}}", &ctx);
        assert!(result.is_err());
    }

    #[test]
    fn render_json_missing_field_errors_in_strict_mode() {
        let (_op, ctx) = build_context(
            &query_result(
                "q",
                ResultDiff::Add {
                    data: json!({"id": 1}),
                    row_signature: 0,
                },
            ),
            &ResultDiff::Add {
                data: json!({"id": 1}),
                row_signature: 0,
            },
        )
        .unwrap();

        // Direct use must not interpolate the string "null" into the SQL text.
        assert!(render_command("EXEC ingest {{json after.missing}}", &ctx).is_err());
        // Combined use must not bind the literal string "null" as a parameter.
        assert!(render_command("EXEC ingest {{param (json after.missing)}}", &ctx).is_err());
    }

    #[test]
    fn validate_rejects_malformed_template() {
        assert!(validate_template("EXEC p {{param after.id}").is_err());
        assert!(validate_template("EXEC p {{param after.id}}").is_ok());
        assert!(validate_template("").is_ok());
    }
}
