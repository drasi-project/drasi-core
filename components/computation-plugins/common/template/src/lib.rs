// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The one template engine of the native sinks.
//!
//! Every templating sink (HTTP, gRPC, SSE, log and the stored-procedure
//! sinks) renders through this crate, so one template means the same thing
//! everywhere:
//!
//! - **Engine:** Handlebars, compiled once when the sink is constructed or
//!   reconfigured, never per change.
//! - **Helpers:** the fixed set in [`HELPERS`]. Every helper call, in every
//!   branch, block, sub-expression and hash argument, is checked statically at
//!   compile time; partials and decorators are rejected.
//! - **Strictness:** strict mode. Reading a field the change does not carry
//!   (`{{after.name}}` on a delete) is a render error, never an empty string.
//!   `{{json x}}` and `{{param x}}` reject a missing `x` too. Guard optional
//!   parts with `{{#if after}}…{{/if}}`; a block condition may read a missing
//!   value.
//! - **Escaping:** none. No sink emits HTML, so `{{x}}` and `{{{x}}}` both
//!   write the value as it is. Write `{{json x}}` to emit a JSON value
//!   (quoted and escaped) inside a JSON body.
//! - **Context:** [`change_context`]: the change notification (`operation`,
//!   `output`, `sequence`, `timestamp`, `identity`, and `element`, `before`,
//!   `after`, `removedProperties`, `effectiveFrom` as the record carries
//!   them). The stored-procedure sinks add `deliveryId`.
//!
//! [`Templates`] renders text. [`ParameterTemplate`] renders SQL in which
//! every value is bound through `{{param …}}` and nothing else may write a
//! value into the statement.

mod parameters;
mod validate;

pub use parameters::{
    validate_parameter_only, BoundCommand, ParameterTemplate, Placeholder, CONTROL_HELPERS,
    PARAMETER_HELPER,
};

use drasi_computation_contracts::ChangeNotification;
use handlebars::{Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason};
use serde_json::Value;

/// The helpers a template may call: the Handlebars built-ins and `json`.
///
/// The list is fixed, so optional Handlebars features enabled elsewhere in a
/// build never change which templates are accepted.
pub const HELPERS: &[&str] = &[
    "if", "unless", "each", "with", "lookup", "raw", "log", "eq", "ne", "gt", "gte", "lt", "lte",
    "and", "or", "not", "len", "json",
];

/// A template that cannot be compiled or rendered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// The source is not valid Handlebars.
    #[error("invalid Handlebars template: {0}")]
    Syntax(String),
    /// The source uses a helper, partial, decorator or interpolation the
    /// engine does not allow.
    #[error("unsupported template construct: {0}")]
    Unsupported(String),
    /// Rendering failed, for example on a field the change does not carry.
    #[error("template failed to render: {0}")]
    Render(String),
    /// A body that must be JSON did not render valid JSON.
    #[error("template did not render valid JSON: {0}")]
    InvalidJson(String),
    /// The template was never compiled into this set.
    #[error("template was not compiled")]
    NotCompiled,
    /// The change could not be turned into a template context.
    #[error("change context unavailable: {0}")]
    Context(String),
}

/// The template context of one change, identical in every sink.
pub fn change_context(notification: &ChangeNotification) -> Result<Value, TemplateError> {
    notification
        .to_json()
        .map_err(|error| TemplateError::Context(error.to_string()))
}

/// A registry configured with the engine's rules and helpers.
pub(crate) fn registry() -> Handlebars<'static> {
    let mut registry = Handlebars::new();
    registry.set_strict_mode(true);
    registry.register_escape_fn(handlebars::no_escape);
    registry.register_helper("json", Box::new(json_helper));
    registry
}

/// `{{json value}}`: the compact JSON of a value. A missing value fails.
fn json_helper(
    helper: &Helper,
    _: &Handlebars,
    _: &handlebars::Context,
    _: &mut RenderContext,
    out: &mut dyn Output,
) -> HelperResult {
    let value = helper
        .param(0)
        .ok_or(RenderErrorReason::ParamNotFoundForIndex("json", 0))?;
    if value.is_value_missing() {
        return Err(RenderErrorReason::Other(format!(
            "`json` read a field the change does not carry: {}",
            missing_path(value)
        ))
        .into());
    }
    let text = serde_json::to_string(value.value())
        .map_err(|error| RenderErrorReason::Other(error.to_string()))?;
    out.write(&text)?;
    Ok(())
}

pub(crate) fn missing_path(value: &handlebars::PathAndJson) -> String {
    value
        .relative_path()
        .cloned()
        .unwrap_or_else(|| "<value>".to_owned())
}

/// Compiled text templates keyed by their source text.
///
/// Identical sources share one compiled template. An empty source is never
/// compiled; callers treat it as "no template".
pub struct Templates {
    registry: Handlebars<'static>,
}

impl Default for Templates {
    fn default() -> Self {
        Self::new()
    }
}

impl Templates {
    pub fn new() -> Self {
        Self {
            registry: registry(),
        }
    }

    /// Compiles and checks `source`. Compiling a source twice is a no-op.
    pub fn compile(&mut self, source: &str) -> Result<(), TemplateError> {
        if source.is_empty() || self.registry.has_template(source) {
            return Ok(());
        }
        let template = validate::compile(source)?;
        validate::text(&template)?;
        self.registry.register_template(source, template);
        Ok(())
    }

    /// Whether `source` has been compiled into this set.
    pub fn contains(&self, source: &str) -> bool {
        self.registry.has_template(source)
    }

    /// Renders a source compiled earlier.
    pub fn render(&self, source: &str, context: &Value) -> Result<String, TemplateError> {
        if !self.registry.has_template(source) {
            return Err(TemplateError::NotCompiled);
        }
        self.registry
            .render(source, context)
            .map_err(|error| TemplateError::Render(render_reason(&error)))
    }

    /// Renders a source compiled earlier and parses the text as JSON.
    pub fn render_json(&self, source: &str, context: &Value) -> Result<Value, TemplateError> {
        let text = self.render(source, context)?;
        serde_json::from_str(&text).map_err(|error| TemplateError::InvalidJson(error.to_string()))
    }
}

/// The render error's reason without the template name, which is the
/// template source text itself.
pub(crate) fn render_reason(error: &handlebars::RenderError) -> String {
    let reason = error.reason().to_string();
    match (error.line_no, error.column_no) {
        (Some(line), Some(column)) => format!("{reason} (line {line}, column {column})"),
        _ => reason,
    }
}

#[cfg(test)]
mod tests;
