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
//! the ordered list of values to bind positionally. Field values are **never**
//! substituted into the SQL text; the `{{param ...}}` helper records each
//! referenced value and emits a `?` placeholder so the executor can bind it as
//! a parameter. This keeps the reaction safe against SQL injection regardless
//! of the data flowing through it.
//!
//! Two helpers are registered on every renderer:
//!
//! - `{{param <path>}}` — binds the value at `<path>` (for example
//!   `after.id`) as a positional parameter and writes `?`. Objects and arrays
//!   are bound whole and serialized to JSON by the executor.
//! - `{{json <path>}}` — writes the JSON serialization of the value at
//!   `<path>` inline. Useful when embedding a JSON literal inside a larger
//!   string that is itself bound with `{{param}}`.

use anyhow::{anyhow, Result};
use handlebars::{
    Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason,
};
use serde_json::Value;
use std::sync::{Arc, Mutex};

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
pub struct CommandRenderer {
    handlebars: Handlebars<'static>,
    params: Arc<Mutex<Vec<Value>>>,
}

impl CommandRenderer {
    /// Create a new renderer with the `param` and `json` helpers registered.
    pub fn new() -> Self {
        let mut handlebars = Handlebars::new();
        // Missing fields must fail the render (so we can fall back / skip)
        // rather than silently binding an empty value.
        handlebars.set_strict_mode(true);

        let params: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

        // {{param <path>}} — bind the referenced value positionally, emit `?`.
        let buffer = params.clone();
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
                    buffer
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
                    if let Some(param) = h.param(0) {
                        let serialized = serde_json::to_string(param.value())
                            .map_err(|e| RenderErrorReason::Other(e.to_string()))?;
                        out.write(&serialized)?;
                    }
                    Ok(())
                },
            ),
        );

        Self { handlebars, params }
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
        // Reset the binding buffer before rendering. `render_template` is
        // synchronous and the `param` helper is the only other writer, so no
        // await point separates the clear from the drain below.
        self.params
            .lock()
            .expect("parameter buffer mutex poisoned")
            .clear();

        let sql = self
            .handlebars
            .render_template(template, context)
            .map_err(|e| anyhow!("failed to render stored procedure command template: {e}"))?;

        let params =
            std::mem::take(&mut *self.params.lock().expect("parameter buffer mutex poisoned"));

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
