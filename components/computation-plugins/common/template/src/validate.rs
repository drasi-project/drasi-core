// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Static checks over a compiled template. A static check covers every
//! branch (`{{#if}}`/`{{else}}`), unlike a trial render.

use crate::{TemplateError, HELPERS};
use handlebars::template::{HelperTemplate, Parameter, Template, TemplateElement};

pub(crate) fn compile(source: &str) -> Result<Template, TemplateError> {
    Template::compile(source).map_err(|error| TemplateError::Syntax(error.to_string()))
}

/// Text mode: every helper call names a helper in [`HELPERS`]; partials and
/// decorators are rejected.
pub(crate) fn text(template: &Template) -> Result<(), TemplateError> {
    template.elements.iter().try_for_each(element)
}

fn element(element: &TemplateElement) -> Result<(), TemplateError> {
    match element {
        TemplateElement::RawString(_) | TemplateElement::Comment(_) => Ok(()),
        TemplateElement::Expression(helper) | TemplateElement::HtmlExpression(helper) => {
            let lookup = !helper.block && helper.params.is_empty() && helper.hash.is_empty();
            if lookup {
                // A bare `{{name}}` is a value lookup, or a sub-expression.
                parameter(&helper.name)?;
            } else {
                helper_name(&helper.name)?;
            }
            parts(helper)
        }
        TemplateElement::HelperBlock(helper) => {
            helper_name(&helper.name)?;
            parts(helper)
        }
        TemplateElement::PartialExpression(_) | TemplateElement::PartialBlock(_) => Err(
            TemplateError::Unsupported("template partials are not supported".into()),
        ),
        TemplateElement::DecoratorExpression(_) | TemplateElement::DecoratorBlock(_) => Err(
            TemplateError::Unsupported("template decorators are not supported".into()),
        ),
    }
}

/// The parameters, hash values and nested templates of a helper.
pub(crate) fn parts(helper: &HelperTemplate) -> Result<(), TemplateError> {
    helper.params.iter().try_for_each(parameter)?;
    helper.hash.values().try_for_each(parameter)?;
    for template in helper.template.iter().chain(helper.inverse.iter()) {
        text(template)?;
    }
    Ok(())
}

/// Helper arguments: values, or sub-expressions that call a known helper.
pub(crate) fn parameter(parameter: &Parameter) -> Result<(), TemplateError> {
    match parameter {
        Parameter::Subexpression(expression) => match expression.as_element() {
            TemplateElement::Expression(helper)
            | TemplateElement::HtmlExpression(helper)
            | TemplateElement::HelperBlock(helper) => {
                helper_name(&helper.name)?;
                parts(helper)
            }
            _ => Err(TemplateError::Unsupported(
                "unsupported template sub-expression".into(),
            )),
        },
        Parameter::Name(_) | Parameter::Path(_) | Parameter::Literal(_) => Ok(()),
    }
}

fn helper_name(name: &Parameter) -> Result<(), TemplateError> {
    match name.as_name() {
        Some(name) if HELPERS.contains(&name) => Ok(()),
        Some(name) => Err(TemplateError::Unsupported(format!(
            "unknown template helper '{name}'; available helpers: {}",
            HELPERS.join(", ")
        ))),
        None => Err(TemplateError::Unsupported(
            "a helper must be called by name".into(),
        )),
    }
}

/// A short description of an expression head for error messages.
pub(crate) fn describe(parameter: &Parameter) -> String {
    match parameter {
        Parameter::Name(name) => name.clone(),
        Parameter::Path(path) => format!("{path:?}"),
        Parameter::Literal(value) => value.to_string(),
        Parameter::Subexpression(_) => "<sub-expression>".to_owned(),
    }
}
