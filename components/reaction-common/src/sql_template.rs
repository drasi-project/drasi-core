// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The single parameter-only rule for stored-procedure command templates.
//!
//! A command template is SQL text in which every dynamic value goes through
//! the `{{param …}}` helper, which each dialect renders as a bound parameter
//! (`$1`, `?`, `@P1`). Anything else that could write a value into the SQL
//! text is rejected at construction: plain `{{x}}` interpolation, raw
//! `{{{x}}}` output, other value helpers such as `{{json x}}`, partials and
//! decorators. The built-in block helpers `if`, `unless`, `each` and `with` are
//! allowed as control flow; their bodies follow the same rule. Procedure names
//! and other structure must be literal text.

use anyhow::{anyhow, Result};
use handlebars::template::{Parameter, TemplateElement};
use handlebars::Template;

/// The only helper allowed to emit a value.
pub const PARAMETER_HELPER: &str = "param";

/// Block helpers allowed as control flow.
pub const CONTROL_HELPERS: [&str; 4] = ["if", "unless", "each", "with"];

/// Parses `source` and applies [`ensure_parameter_only`]. An empty or
/// whitespace-only template is accepted (an unconfigured operation).
pub fn validate_parameter_only(source: &str) -> Result<()> {
    if source.trim().is_empty() {
        return Ok(());
    }
    let template = Template::compile(source)
        .map_err(|error| anyhow!("invalid Handlebars template: {error}"))?;
    ensure_parameter_only(&template)
}

/// Rejects every element of a compiled template that could write a value
/// into the SQL text instead of binding it.
pub fn ensure_parameter_only(template: &Template) -> Result<()> {
    for element in &template.elements {
        match element {
            TemplateElement::RawString(_) | TemplateElement::Comment(_) => {}
            TemplateElement::Expression(helper) => {
                if name(&helper.name) != Some(PARAMETER_HELPER) {
                    return Err(anyhow!(
                        "unsafe template interpolation `{}`: only the `{{{{param …}}}}` helper may \
                         emit values into stored-procedure SQL, so they are bound as parameters; \
                         write the procedure name and other structure as literal text",
                        describe(&helper.name)
                    ));
                }
                if helper.params.is_empty() {
                    return Err(anyhow!("`{{{{param}}}}` requires a value"));
                }
            }
            TemplateElement::HtmlExpression(_) => {
                return Err(anyhow!(
                    "raw interpolation (`{{{{{{ … }}}}}}`) is not allowed in stored-procedure \
                     templates; use `{{{{param …}}}}` so the value is bound as a parameter"
                ));
            }
            TemplateElement::HelperBlock(helper) => {
                let block = name(&helper.name);
                if !block.is_some_and(|block| CONTROL_HELPERS.contains(&block)) {
                    return Err(anyhow!(
                        "block helper `{}` is not allowed in stored-procedure templates; only {} \
                         may be used as control flow",
                        describe(&helper.name),
                        CONTROL_HELPERS.join(", ")
                    ));
                }
                if let Some(inner) = &helper.template {
                    ensure_parameter_only(inner)?;
                }
                if let Some(inverse) = &helper.inverse {
                    ensure_parameter_only(inverse)?;
                }
            }
            _ => {
                return Err(anyhow!(
                    "partials and decorators are not allowed in stored-procedure templates"
                ));
            }
        }
    }
    Ok(())
}

fn name(parameter: &Parameter) -> Option<&str> {
    match parameter {
        Parameter::Name(name) => Some(name),
        _ => None,
    }
}

fn describe(parameter: &Parameter) -> String {
    match parameter {
        Parameter::Name(name) => name.clone(),
        Parameter::Path(path) => format!("{path:?}"),
        Parameter::Literal(value) => value.to_string(),
        Parameter::Subexpression(_) => "<subexpression>".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_bound_parameters_and_control_flow_are_accepted() {
        for safe in [
            "",
            "CALL upsert({{param after.id}}, {{param after.name}})",
            "EXEC dbo.Upsert @Id = {{param after.id}}",
            "{{#if after.active}}CALL on({{param after.id}}){{else}}CALL off({{param after.id}}){{/if}}",
            "{{#each after.tags}}CALL tag({{param this}});{{/each}}",
            "{{! a comment }}CALL p({{param (lookup after \"id\")}})",
        ] {
            validate_parameter_only(safe).unwrap_or_else(|error| panic!("{safe}: {error}"));
        }
        for unsafe_template in [
            "CALL upsert('{{after.name}}')",
            "CALL upsert({{{after.name}}})",
            "CALL upsert({{& after.name}})",
            "EXEC p '{{json after.name}}'",
            "CALL {{after.procedure}}()",
            "{{> partial}}",
            "{{#custom after}}x{{/custom}}",
            "{{#if after.x}}'{{after.name}}'{{/if}}",
            "{{#each after.rows}}{{{this}}}{{/each}}",
            "CALL p({{param}})",
            "{{*inline}}",
        ] {
            assert!(
                validate_parameter_only(unsafe_template).is_err(),
                "{unsafe_template} must be rejected"
            );
        }
        assert!(validate_parameter_only("{{#if}").is_err(), "invalid syntax");
    }
}
