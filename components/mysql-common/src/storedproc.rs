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

//! Handlebars command renderer for the MySQL stored procedure reaction.
//!
//! A command template is a Handlebars string such as
//! `CALL add_user({{param after.id}}, {{param after.name}})`. Rendering a
//! template produces the final SQL text (with `?` placeholders) together with
//! the ordered list of values to bind positionally. Values referenced with the
//! `{{param ...}}` helper are **never** substituted into the SQL text; the
//! helper records each referenced value and emits a `?` placeholder so the
//! executor can bind it as a parameter. This keeps `{{param ...}}` safe against
//! SQL injection regardless of the data flowing through it.
//!
//! Two helpers are registered on every renderer:
//!
//! - `{{param <path>}}` — binds the value at `<path>` (for example
//!   `after.id`) as a positional parameter and writes `?`. Objects and arrays
//!   are bound whole and serialized to JSON by the executor. Prefer this for
//!   any untrusted value.
//! - `{{json <path>}}` — writes the JSON serialization of the value at
//!   `<path>` **inline into the SQL text**. Because the result is interpolated
//!   rather than bound, it is **not** injection- or quoting-safe (a value
//!   containing a `'` can break out of a surrounding string literal). Only use
//!   it for trusted values; prefer `{{param <path>}}` for source data.

use anyhow::{anyhow, Result};
use handlebars::{
    Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason,
};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[cfg(feature = "native")]
pub mod config;
#[cfg(feature = "native")]
pub mod runtime;

/// The outcome of rendering a command template.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedCommand {
    /// SQL text with `?` placeholders for each bound parameter.
    pub sql: String,
    /// Parameter values to bind positionally, in placeholder order.
    pub params: Vec<Value>,
}

/// Renders stored procedure command templates with positional parameter
/// binding.
///
/// The renderer is stateless: each call to [`CommandRenderer::render`] builds
/// its own Handlebars instance and parameter buffer, so renders never share
/// mutable state and cannot interfere with one another even if invoked
/// concurrently.
pub struct CommandRenderer;

pub fn resolve_template<'a>(
    routes: &'a std::collections::HashMap<String, drasi_reaction_common::QueryConfig>,
    default: Option<&'a drasi_reaction_common::QueryConfig>,
    query: &str,
    operation: drasi_reaction_common::OperationType,
) -> Option<&'a drasi_reaction_common::TemplateSpec> {
    let select = |config: &'a drasi_reaction_common::QueryConfig| {
        use drasi_reaction_common::OperationType;
        (match operation {
            OperationType::Add => config.added.as_ref(),
            OperationType::Update => config.updated.as_ref(),
            OperationType::Delete => config.deleted.as_ref(),
        })
        .filter(|template| !template.template.trim().is_empty())
    };
    routes
        .get(query)
        .and_then(select)
        .or_else(|| {
            routes
                .get(query.rsplit('.').next().unwrap_or(query))
                .and_then(select)
        })
        .or_else(|| default.and_then(select))
}

pub fn validate_routes(
    routes: &std::collections::HashMap<String, drasi_reaction_common::QueryConfig>,
    queries: &[String],
) -> Result<()> {
    for key in routes.keys() {
        anyhow::ensure!(
            queries
                .iter()
                .any(|id| id == key || id.rsplit('.').next() == Some(key)),
            "route key '{key}' does not match any configured query"
        );
    }
    Ok(())
}

/// Native SQL permits control flow but only positional parameters may emit
/// values. The legacy renderer's separately documented behavior is unchanged.
pub fn validate_parameterized_template(source: &str) -> Result<()> {
    use handlebars::template::{Parameter, TemplateElement};
    fn validate(template: &handlebars::Template) -> Result<()> {
        for element in &template.elements {
            match element {
                TemplateElement::RawString(_) | TemplateElement::Comment(_) => {}
                TemplateElement::Expression(helper) => {
                    anyhow::ensure!(
                        matches!(&helper.name, Parameter::Name(name) if name == "param"),
                        "native MySQL SQL values must use the positional param helper"
                    );
                }
                TemplateElement::HelperBlock(helper) => {
                    if let Some(inner) = &helper.template {
                        validate(inner)?;
                    }
                    if let Some(inverse) = &helper.inverse {
                        validate(inverse)?;
                    }
                }
                _ => anyhow::bail!(
                    "native MySQL SQL does not allow raw output, partials or decorators"
                ),
            }
        }
        Ok(())
    }
    if source.trim().is_empty() {
        return Ok(());
    }
    validate(&handlebars::Template::compile(source)?)
}

pub fn build_context(
    query_id: &str,
    operation: &str,
    timestamp: &str,
    metadata: &std::collections::HashMap<String, Value>,
    before: Option<&Value>,
    after: Option<&Value>,
    data: Option<&Value>,
) -> Value {
    let mut context = serde_json::Map::from_iter([
        ("query_id".into(), Value::from(query_id)),
        ("query_name".into(), Value::from(query_id)),
        ("operation".into(), Value::from(operation)),
        ("timestamp".into(), Value::from(timestamp)),
        ("metadata".into(), serde_json::json!(metadata)),
    ]);
    if let Some(before) = before {
        context.insert("before".into(), before.clone());
    }
    if let Some(after) = after {
        context.insert("after".into(), after.clone());
    }
    if let Some(data) = data {
        context.insert("data".into(), data.clone());
    }
    Value::Object(context)
}

pub fn parameter(value: &Value) -> mysql_async::Value {
    use mysql_async::Value as Parameter;
    match value {
        Value::Null => Parameter::NULL,
        Value::Bool(value) => Parameter::Int(i64::from(*value)),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Parameter::Int(value)
            } else if let Some(value) = value.as_u64() {
                Parameter::UInt(value)
            } else if let Some(value) = value.as_f64() {
                Parameter::Double(value)
            } else {
                Parameter::Bytes(value.to_string().into_bytes())
            }
        }
        Value::String(value) => Parameter::Bytes(value.as_bytes().to_vec()),
        value => Parameter::Bytes(value.to_string().into_bytes()),
    }
}

/// Build a Handlebars instance whose `param` helper appends bound values to
/// `params`.
///
/// The `param` buffer is threaded through an `Arc<Mutex<..>>` because
/// Handlebars requires helper closures to be `Send + Sync + 'static`; the
/// buffer is created per render, so the lock is never actually contended.
fn build_handlebars(params: Arc<Mutex<Vec<Value>>>) -> Handlebars<'static> {
    let mut handlebars = Handlebars::new();
    // Missing fields must fail the render (so we can fall back / skip)
    // rather than silently binding an empty value.
    handlebars.set_strict_mode(true);

    // {{param <path>}} — bind the referenced value positionally, emit `?`.
    handlebars.register_helper(
        "param",
        Box::new(
            move |h: &Helper<'_>,
                  _: &Handlebars<'_>,
                  _: &Context,
                  _: &mut RenderContext<'_, '_>,
                  out: &mut dyn Output|
                  -> HelperResult {
                let param = h.param(0).ok_or_else(|| {
                    RenderErrorReason::Other("`param` helper requires one argument".to_string())
                })?;
                if param.is_value_missing() {
                    return Err(RenderErrorReason::Other(
                        "`param` helper referenced a field that is not present in the query result"
                            .to_string(),
                    )
                    .into());
                }
                params
                    .lock()
                    .expect("parameter buffer mutex poisoned")
                    .push(param.value().clone());
                out.write("?")?;
                Ok(())
            },
        ),
    );

    // {{json <path>}} — write the JSON serialization inline.
    handlebars.register_helper(
        "json",
        Box::new(
            |h: &Helper<'_>,
             _: &Handlebars<'_>,
             _: &Context,
             _: &mut RenderContext<'_, '_>,
             out: &mut dyn Output|
             -> HelperResult {
                let param = h.param(0).ok_or_else(|| {
                    RenderErrorReason::Other("`json` helper requires one argument".to_string())
                })?;
                if param.is_value_missing() {
                    return Err(RenderErrorReason::Other(
                        "`json` helper referenced a field that is not present in the query result"
                            .to_string(),
                    )
                    .into());
                }
                let serialized = serde_json::to_string(param.value())
                    .map_err(|e| RenderErrorReason::Other(e.to_string()))?;
                out.write(&serialized)?;
                Ok(())
            },
        ),
    );

    handlebars
}

impl CommandRenderer {
    /// Create a new renderer.
    pub fn new() -> Self {
        Self
    }

    /// Validate a command template at construction time.
    ///
    /// An empty template is treated as "not configured" and accepted.
    pub fn validate_template(template: &str) -> Result<()> {
        if template.trim().is_empty() {
            return Ok(());
        }
        handlebars::Template::compile(template)
            .map_err(|e| anyhow!("invalid stored procedure command template: {e}"))?;
        Ok(())
    }

    /// Render a command template against the supplied context.
    ///
    /// Returns the rendered SQL (with `?` placeholders) and the ordered
    /// parameter values. A render error (for example a referenced field that
    /// is absent from the query result) is returned as an `Err` so the caller
    /// can skip the event rather than execute a malformed command.
    pub fn render(&self, template: &str, context: &Value) -> Result<RenderedCommand> {
        // A fresh buffer and Handlebars instance per render keep all binding
        // state local to this call, so there is no shared mutable state and no
        // reset-between-calls sequencing to get wrong.
        let params: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let handlebars = build_handlebars(params.clone());

        let sql = handlebars
            .render_template(template, context)
            .map_err(|e| anyhow!("failed to render stored procedure command template: {e}"))?;

        let params = std::mem::take(&mut *params.lock().expect("parameter buffer mutex poisoned"));

        Ok(RenderedCommand {
            sql: sql.trim().to_string(),
            params,
        })
    }
}

impl Default for CommandRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Value {
        json!({
            "after": {"id": 1, "name": "Alice", "email": "alice@example.com", "note": null},
            "before": {"id": 7},
            "query_name": "user-query",
            "operation": "ADD"
        })
    }

    #[test]
    fn renders_positional_placeholders_and_binds_values() {
        let r = CommandRenderer::new();
        let out = r
            .render(
                "CALL add_user({{param after.id}}, {{param after.name}})",
                &ctx(),
            )
            .unwrap();
        assert_eq!(out.sql, "CALL add_user(?, ?)");
        assert_eq!(out.params, vec![json!(1), json!("Alice")]);
    }

    #[test]
    fn binds_parameters_in_appearance_order() {
        let r = CommandRenderer::new();
        let out = r
            .render(
                "CALL add_user({{param after.email}}, {{param after.id}}, {{param after.name}})",
                &ctx(),
            )
            .unwrap();
        assert_eq!(out.sql, "CALL add_user(?, ?, ?)");
        assert_eq!(
            out.params,
            vec![json!("alice@example.com"), json!(1), json!("Alice")]
        );
    }

    #[test]
    fn duplicate_parameter_is_bound_twice() {
        let r = CommandRenderer::new();
        let out = r
            .render("CALL touch({{param after.id}}, {{param after.id}})", &ctx())
            .unwrap();
        assert_eq!(out.params, vec![json!(1), json!(1)]);
    }

    #[test]
    fn no_parameters_renders_bare_call() {
        let r = CommandRenderer::new();
        let out = r.render("CALL refresh_view()", &ctx()).unwrap();
        assert_eq!(out.sql, "CALL refresh_view()");
        assert!(out.params.is_empty());
    }

    #[test]
    fn explicit_null_binds_as_null_not_missing() {
        let r = CommandRenderer::new();
        let out = r.render("CALL n({{param after.note}})", &ctx()).unwrap();
        assert_eq!(out.params, vec![json!(null)]);
    }

    #[test]
    fn missing_field_is_a_render_error() {
        let r = CommandRenderer::new();
        let err = r
            .render("CALL x({{param after.missing}})", &ctx())
            .unwrap_err();
        assert!(err.to_string().contains("failed to render"));
    }

    #[test]
    fn object_value_is_bound_whole() {
        let r = CommandRenderer::new();
        let out = r.render("CALL blob({{param after}})", &ctx()).unwrap();
        assert_eq!(out.sql, "CALL blob(?)");
        assert_eq!(out.params.len(), 1);
        assert!(out.params[0].is_object());
    }

    #[test]
    fn json_helper_serializes_inline() {
        let r = CommandRenderer::new();
        let out = r
            .render("CALL doc({{param query_name}}, '{{json after}}')", &ctx())
            .unwrap();
        assert_eq!(out.params, vec![json!("user-query")]);
        assert!(out.sql.contains("\"id\":1"));
        assert!(out.sql.starts_with("CALL doc(?, '"));
    }

    #[test]
    fn json_helper_missing_field_is_a_render_error() {
        let r = CommandRenderer::new();
        let err = r
            .render("CALL doc('{{json after.missing}}')", &ctx())
            .unwrap_err();
        assert!(err.to_string().contains("failed to render"));
    }

    #[test]
    fn render_resets_buffer_between_calls() {
        let r = CommandRenderer::new();
        let first = r.render("CALL a({{param after.id}})", &ctx()).unwrap();
        let second = r.render("CALL b({{param after.name}})", &ctx()).unwrap();
        assert_eq!(first.params, vec![json!(1)]);
        assert_eq!(second.params, vec![json!("Alice")]);
    }

    #[test]
    fn validate_accepts_empty_template() {
        assert!(CommandRenderer::validate_template("").is_ok());
        assert!(CommandRenderer::validate_template("   ").is_ok());
    }

    #[test]
    fn validate_accepts_valid_template() {
        assert!(CommandRenderer::validate_template("CALL add_user({{param after.id}})").is_ok());
    }

    #[test]
    fn validate_rejects_malformed_template() {
        assert!(CommandRenderer::validate_template("CALL add_user({{param after.id}").is_err());
    }
}
