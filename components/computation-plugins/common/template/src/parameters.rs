// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Parameter-binding SQL templates for the stored-procedure sinks.
//!
//! A command template is SQL text in which every dynamic value goes through
//! the `{{param …}}` helper, which renders the dialect's placeholder (`$1`,
//! `?`, `@P1`) and collects the value to bind. Anything else that could write
//! a value into the SQL text is rejected at compile time: plain `{{x}}`
//! interpolation, raw `{{{x}}}` output, other value helpers such as
//! `{{json x}}` at the top level, partials and decorators. The block helpers
//! `if`, `unless`, `each` and `with` are allowed as control flow; their bodies
//! follow the same rule. A `param` argument may be a sub-expression of a
//! standard helper, for example `{{param (json after)}}` binds the JSON text.
//! Procedure names and other structure must be literal text.

use crate::{missing_path, registry, render_reason, validate, TemplateError};
use handlebars::template::{Template, TemplateElement};
use handlebars::{Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason};
use serde_json::Value;
use std::cell::RefCell;

/// The only helper allowed to emit a value.
pub const PARAMETER_HELPER: &str = "param";

/// Block helpers allowed as control flow.
pub const CONTROL_HELPERS: [&str; 4] = ["if", "unless", "each", "with"];

/// How a dialect writes the placeholder of the `n`th bound value (from 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placeholder {
    /// PostgreSQL: `$1`, `$2`, …
    Dollar,
    /// MySQL: `?` for every value.
    Question,
    /// SQL Server: `@P1`, `@P2`, …
    AtP,
}

impl Placeholder {
    fn write(self, index: usize) -> String {
        match self {
            Self::Dollar => format!("${index}"),
            Self::Question => "?".to_owned(),
            Self::AtP => format!("@P{index}"),
        }
    }
}

/// One rendered command: SQL text with placeholders and the values to bind,
/// in placeholder order.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundCommand {
    pub sql: String,
    pub parameters: Vec<Value>,
}

thread_local! {
    /// The values collected by `{{param …}}` during the current render.
    /// Rendering is synchronous and never re-entrant, so a per-thread buffer
    /// keeps concurrent renders on other threads separate.
    static PARAMETERS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
}

const NAME: &str = "command";

/// A compiled, parameter-only command template.
pub struct ParameterTemplate {
    registry: Handlebars<'static>,
    empty: bool,
}

impl ParameterTemplate {
    /// Compiles `source` under the parameter-only rule. An empty or
    /// whitespace-only source compiles to a template that renders nothing.
    pub fn compile(source: &str, placeholder: Placeholder) -> Result<Self, TemplateError> {
        let mut registry = registry();
        registry.register_helper(
            PARAMETER_HELPER,
            Box::new(
                move |helper: &Helper,
                      _: &Handlebars,
                      _: &handlebars::Context,
                      _: &mut RenderContext,
                      out: &mut dyn Output|
                      -> HelperResult {
                    let value = helper
                        .param(0)
                        .ok_or(RenderErrorReason::ParamNotFoundForIndex("param", 0))?;
                    if value.is_value_missing() {
                        return Err(RenderErrorReason::Other(format!(
                            "`param` read a field the change does not carry: {}",
                            missing_path(value)
                        ))
                        .into());
                    }
                    let index = PARAMETERS.with(|buffer| {
                        let mut buffer = buffer.borrow_mut();
                        buffer.push(value.value().clone());
                        buffer.len()
                    });
                    out.write(&placeholder.write(index))?;
                    Ok(())
                },
            ),
        );
        let empty = source.trim().is_empty();
        if !empty {
            let template = validate::compile(source)?;
            ensure_parameter_only(&template)?;
            registry.register_template(NAME, template);
        }
        Ok(Self { registry, empty })
    }

    /// Whether the source was empty, so nothing is ever rendered.
    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// Renders the command. The SQL text is returned as rendered; a condition
    /// that excludes the change renders only whitespace.
    pub fn render(&self, context: &Value) -> Result<BoundCommand, TemplateError> {
        if self.empty {
            return Ok(BoundCommand {
                sql: String::new(),
                parameters: Vec::new(),
            });
        }
        PARAMETERS.with(|buffer| buffer.borrow_mut().clear());
        let rendered = self.registry.render(NAME, context);
        let parameters = PARAMETERS.with(|buffer| std::mem::take(&mut *buffer.borrow_mut()));
        let sql = rendered.map_err(|error| TemplateError::Render(render_reason(&error)))?;
        Ok(BoundCommand { sql, parameters })
    }
}

/// Parses `source` and applies the parameter-only rule. An empty or
/// whitespace-only source is accepted (an unconfigured operation).
pub fn validate_parameter_only(source: &str) -> Result<(), TemplateError> {
    if source.trim().is_empty() {
        return Ok(());
    }
    ensure_parameter_only(&validate::compile(source)?)
}

fn ensure_parameter_only(template: &Template) -> Result<(), TemplateError> {
    for element in &template.elements {
        match element {
            TemplateElement::RawString(_) | TemplateElement::Comment(_) => {}
            TemplateElement::Expression(helper) => {
                if helper.name.as_name() != Some(PARAMETER_HELPER) {
                    return Err(TemplateError::Unsupported(format!(
                        "unsafe template interpolation `{}`: only the `{{{{param …}}}}` helper may \
                         emit values into stored-procedure SQL, so they are bound as parameters; \
                         write the procedure name and other structure as literal text",
                        validate::describe(&helper.name)
                    )));
                }
                if helper.params.len() != 1 || !helper.hash.is_empty() {
                    return Err(TemplateError::Unsupported(
                        "`{{param}}` takes exactly one value".into(),
                    ));
                }
                validate::parameter(&helper.params[0])?;
            }
            TemplateElement::HtmlExpression(_) => {
                return Err(TemplateError::Unsupported(
                    "raw interpolation (`{{{ … }}}`) is not allowed in stored-procedure \
                     templates; use `{{param …}}` so the value is bound as a parameter"
                        .into(),
                ));
            }
            TemplateElement::HelperBlock(helper) => {
                let block = helper.name.as_name();
                if !block.is_some_and(|block| CONTROL_HELPERS.contains(&block)) {
                    return Err(TemplateError::Unsupported(format!(
                        "block helper `{}` is not allowed in stored-procedure templates; only {} \
                         may be used as control flow",
                        validate::describe(&helper.name),
                        CONTROL_HELPERS.join(", ")
                    )));
                }
                helper.params.iter().try_for_each(validate::parameter)?;
                helper.hash.values().try_for_each(validate::parameter)?;
                for inner in helper.template.iter().chain(helper.inverse.iter()) {
                    ensure_parameter_only(inner)?;
                }
            }
            _ => {
                return Err(TemplateError::Unsupported(
                    "partials and decorators are not allowed in stored-procedure templates".into(),
                ));
            }
        }
    }
    Ok(())
}
