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
//! `tokio-postgres`. A whole object (for example `{{param after}}`) is bound as
//! a single JSONB parameter.
//!
//! To make this safe by construction, `{{param …}}` is the **only** interpolation
//! that may emit into the SQL string. [`validate_template`] rejects any other
//! value-emitting construct at configuration time: bare expansions
//! (`{{after.id}}`), raw/unescaped output (`{{{after.id}}}`), and any other helper
//! (including partials and decorators). This prevents an operator from
//! accidentally inlining untrusted row data into SQL text. Static structure such
//! as the procedure name must be written literally in the template.
//!
//! ## Standard context keys
//!
//! Every render is given the guide's standard keys: `query_id`, `query_name`,
//! `operation`, `timestamp`, `metadata`, and `before` / `after` / `data` as
//! applicable to the operation type.

use anyhow::{anyhow, Result};
use handlebars::template::{Parameter, TemplateElement};
use handlebars::{Handlebars, Template};
use serde_json::{Map, Value};
use std::cell::RefCell;

use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::reactions::common::OperationType;

/// The outcome of resolving an inbound diff into a render context.
pub(crate) struct RenderInput {
    /// Operation type used to select the template.
    pub operation: OperationType,
    /// The Handlebars context object.
    pub context: Value,
}

/// Compile a single template string and verify that it contains no unsafe
/// interpolation. An empty string is valid (meaning "no command configured").
///
/// Only the `{{param …}}` helper may emit into the rendered SQL string, so that
/// row values are always bound as positional parameters rather than inlined as
/// SQL text. Any other value-emitting construct (bare `{{expr}}`, raw
/// `{{{expr}}}`, another helper such as `{{json expr}}`, partials, or
/// decorators) is rejected here at configuration time. Block helpers such as
/// `{{#if}}` are allowed for control flow, and their inner templates are checked
/// recursively.
pub(crate) fn validate_template(template: &str) -> Result<()> {
    if template.is_empty() {
        return Ok(());
    }
    let compiled =
        Template::compile(template).map_err(|e| anyhow!("invalid Handlebars template: {e}"))?;
    ensure_param_only(&compiled)
}

/// Recursively verify that every value-emitting element of a compiled template
/// is a `{{param …}}` helper.
fn ensure_param_only(template: &Template) -> Result<()> {
    for element in &template.elements {
        match element {
            // Literal SQL text and comments never emit dynamic values.
            TemplateElement::RawString(_) | TemplateElement::Comment(_) => {}
            // `{{ … }}` — allowed only when it is the `param` helper.
            TemplateElement::Expression(ht) => {
                if helper_name(&ht.name).as_deref() != Some("param") {
                    return Err(anyhow!(
                        "unsafe template interpolation `{}`: only the `{{{{param …}}}}` helper may \
                         emit values into stored-procedure SQL so they are bound as parameters; \
                         write the procedure name and other structure as literal text",
                        describe(&ht.name)
                    ));
                }
            }
            // `{{{ … }}}` / `{{& … }}` — raw, unescaped output is never allowed.
            TemplateElement::HtmlExpression(_) => {
                return Err(anyhow!(
                    "raw/unescaped interpolation (`{{{{{{ … }}}}}}`) is not allowed in \
                     stored-procedure templates; use the `{{{{param …}}}}` helper so the value is \
                     bound as a SQL parameter"
                ));
            }
            // Block helpers (`{{#if}}`, `{{#each}}`, …) are control flow only;
            // validate their inner templates recursively.
            TemplateElement::HelperBlock(ht) => {
                if let Some(inner) = &ht.template {
                    ensure_param_only(inner)?;
                }
                if let Some(inverse) = &ht.inverse {
                    ensure_param_only(inverse)?;
                }
            }
            // Partials and decorators can pull in or rewrite arbitrary content.
            _ => {
                return Err(anyhow!(
                    "partials and decorators are not allowed in stored-procedure templates"
                ));
            }
        }
    }
    Ok(())
}

/// Extract a helper/expression name when it is a simple identifier.
fn helper_name(param: &Parameter) -> Option<String> {
    match param {
        Parameter::Name(name) => Some(name.clone()),
        _ => None,
    }
}

/// Best-effort human-readable rendering of an expression head for error messages.
fn describe(param: &Parameter) -> String {
    match param {
        Parameter::Name(name) => name.clone(),
        Parameter::Path(path) => format!("{path:?}"),
        Parameter::Literal(value) => value.to_string(),
        Parameter::Subexpression(_) => "<subexpression>".to_string(),
    }
}

thread_local! {
    /// Per-render buffer of positional parameter values collected by the
    /// `{{param …}}` helper. It is cleared at the start of every render and
    /// drained at the end. Rendering is synchronous and non-reentrant, so a
    /// thread-local buffer lets the helper stay stateless and be shared across a
    /// pre-compiled registry without per-event allocation.
    static PARAM_BUFFER: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
}

/// Register the `{{param …}}` helper onto a registry. The helper resolves its
/// argument, appends the value to the thread-local [`PARAM_BUFFER`], and emits a
/// `$N` positional placeholder.
fn register_param_helper(handlebars: &mut Handlebars<'static>) {
    handlebars.register_helper(
        "param",
        Box::new(
            |h: &handlebars::Helper,
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
                let index = PARAM_BUFFER.with(|buf| {
                    let mut buf = buf.borrow_mut();
                    buf.push(value);
                    buf.len()
                });
                out.write(&format!("${index}"))?;
                Ok(())
            },
        ),
    );
}

/// A validated, pre-compiled command template.
///
/// The Handlebars template is parsed and the `{{param …}}` helper registered
/// exactly once (at construction). Rendering per event then reuses this
/// registry, so the parse and helper allocation costs are not paid per row.
pub(crate) struct CompiledTemplate {
    registry: Handlebars<'static>,
}

/// Name the single template is registered under inside its private registry.
const TEMPLATE_NAME: &str = "command";

impl CompiledTemplate {
    /// Compile and validate a template string. Returns an error if the template
    /// fails to parse or contains an unsafe (non-`param`) interpolation.
    pub(crate) fn compile(template: &str) -> Result<Self> {
        let compiled =
            Template::compile(template).map_err(|e| anyhow!("invalid Handlebars template: {e}"))?;
        ensure_param_only(&compiled)?;

        let mut registry = Handlebars::new();
        registry.set_strict_mode(true);
        register_param_helper(&mut registry);
        registry.register_template(TEMPLATE_NAME, compiled);
        Ok(Self { registry })
    }

    /// Render the template into a SQL string and the ordered list of positional
    /// bind parameters collected from `{{param …}}` helpers.
    ///
    /// Returns an error if the template fails to render (for example, a
    /// referenced field is missing under strict mode). Callers should log the
    /// error and skip the event rather than executing partial SQL.
    pub(crate) fn render(&self, context: &Value) -> Result<(String, Vec<Value>)> {
        PARAM_BUFFER.with(|buf| buf.borrow_mut().clear());
        let rendered = self
            .registry
            .render(TEMPLATE_NAME, context)
            .map_err(|e| anyhow!("failed to render command template: {e}"))?;
        let collected = PARAM_BUFFER.with(|buf| std::mem::take(&mut *buf.borrow_mut()));
        Ok((rendered, collected))
    }
}

/// Render a template string directly. This compiles the template on every call
/// and is intended for tests and one-off use; the runtime path uses a cached
/// [`CompiledTemplate`] so the parse cost is not paid per event.
#[cfg(test)]
pub(crate) fn render_command(template: &str, context: &Value) -> Result<(String, Vec<Value>)> {
    CompiledTemplate::compile(template)?.render(context)
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
    // `query_name` and `query_id` are intentionally aliases of the same value
    // (the query id as received), per the Reaction Developer Guide. Both are
    // provided for symmetry with other Drasi reactions.
    let query_id = Value::String(query_result.query_id.clone());
    context.insert("query_name".to_string(), query_id.clone());
    context.insert("query_id".to_string(), query_id);
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
    fn param_helper_binds_whole_object_as_single_param() {
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
    fn validate_template_accepts_param_helper() {
        assert!(validate_template("CALL add({{param after.id}}, {{param after}})").is_ok());
    }

    #[test]
    fn validate_template_rejects_bare_interpolation() {
        let err = validate_template("CALL add({{after.id}})").unwrap_err();
        assert!(err.to_string().contains("param"));
    }

    #[test]
    fn validate_template_rejects_raw_interpolation() {
        assert!(validate_template("CALL add({{{after.id}}})").is_err());
    }

    #[test]
    fn validate_template_rejects_json_helper() {
        // The `{{json …}}` helper was removed; it must not slip through as an
        // unparameterized interpolation.
        assert!(validate_template("CALL ingest({{json after}})").is_err());
    }

    #[test]
    fn validate_template_rejects_partials_and_decorators() {
        assert!(validate_template("CALL x({{> some_partial}})").is_err());
    }

    #[test]
    fn validate_template_allows_block_helper_with_param() {
        assert!(
            validate_template("{{#if after.active}}CALL touch({{param after.id}}){{/if}}").is_ok()
        );
    }

    #[test]
    fn validate_template_rejects_bare_interpolation_inside_block() {
        assert!(validate_template("{{#if after.active}}CALL touch({{after.id}}){{/if}}").is_err());
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
