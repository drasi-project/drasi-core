// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Template compilation and rendering. Every template is compiled once, at
//! construction, and its helper names are checked there; rendering a change
//! never falls back to another body, URL or header (D-5).

use anyhow::{ensure, Context, Result};
use drasi_http_common::reaction::{config::HttpOutputTemplates, process::build_handlebars};
use handlebars::template::{HelperTemplate, Parameter, TemplateElement};
use serde_json::Value;

/// Helpers available to templates: the Handlebars built-ins and `json`.
pub const HELPERS: &[&str] = &[
    "if", "unless", "each", "with", "lookup", "raw", "log", "eq", "ne", "gt", "gte", "lt", "lte",
    "and", "or", "not", "len", "json",
];

/// A registry holding every configured template, keyed by its source text.
pub(crate) struct Renderer {
    registry: handlebars::Handlebars<'static>,
}

impl Renderer {
    /// Compiles every body, URL and header template.
    pub fn new(templates: Option<&HttpOutputTemplates>) -> Result<Self> {
        let mut renderer = Self {
            registry: build_handlebars(),
        };
        let Some(templates) = templates else {
            return Ok(renderer);
        };
        let configs = templates
            .default_template
            .iter()
            .chain(templates.routes.values());
        for config in configs {
            for spec in [&config.added, &config.updated, &config.deleted]
                .into_iter()
                .flatten()
            {
                renderer.compile(&spec.template).context("body template")?;
                renderer
                    .compile(&spec.extension.url)
                    .context("url template")?;
                for (name, value) in &spec.extension.headers {
                    renderer
                        .compile(value)
                        .with_context(|| format!("header {name} template"))?;
                }
            }
        }
        Ok(renderer)
    }

    fn compile(&mut self, source: &str) -> Result<()> {
        if source.is_empty() || self.registry.has_template(source) {
            return Ok(());
        }
        let template = handlebars::Template::compile(source)
            .map_err(|error| anyhow::anyhow!("invalid Handlebars template: {error}"))?;
        check_elements(&template.elements)?;
        self.registry.register_template(source, template);
        Ok(())
    }

    /// Renders a template compiled at construction.
    pub fn render(&self, source: &str, context: &Value) -> Result<String> {
        self.registry
            .render(source, context)
            .map_err(|error| anyhow::anyhow!("template render failed: {error}"))
    }
}

fn check_elements(elements: &[TemplateElement]) -> Result<()> {
    for element in elements {
        match element {
            TemplateElement::RawString(_) | TemplateElement::Comment(_) => {}
            TemplateElement::Expression(helper)
            | TemplateElement::HtmlExpression(helper)
            | TemplateElement::HelperBlock(helper) => check_helper(helper, false)?,
            TemplateElement::DecoratorExpression(_)
            | TemplateElement::DecoratorBlock(_)
            | TemplateElement::PartialExpression(_)
            | TemplateElement::PartialBlock(_) => {
                anyhow::bail!("template partials and decorators are not supported")
            }
        }
    }
    Ok(())
}

/// A helper call (block, parameters, hash or subexpression) must name a
/// registered helper; a bare `{{name}}` is a value lookup.
fn check_helper(helper: &HelperTemplate, subexpression: bool) -> Result<()> {
    let call =
        subexpression || helper.block || !helper.params.is_empty() || !helper.hash.is_empty();
    if call {
        let name = helper.name.as_name().unwrap_or_default();
        ensure!(
            HELPERS.contains(&name),
            "unknown template helper '{name}'; available helpers: {}",
            HELPERS.join(", ")
        );
    }
    for parameter in helper.params.iter().chain(helper.hash.values()) {
        check_parameter(parameter)?;
    }
    for template in helper.template.iter().chain(helper.inverse.iter()) {
        check_elements(&template.elements)?;
    }
    Ok(())
}

fn check_parameter(parameter: &Parameter) -> Result<()> {
    if let Parameter::Subexpression(expression) = parameter {
        match expression.as_element() {
            TemplateElement::Expression(helper)
            | TemplateElement::HtmlExpression(helper)
            | TemplateElement::HelperBlock(helper) => check_helper(helper, true)?,
            _ => anyhow::bail!("unsupported template subexpression"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn templates(body: &str) -> HttpOutputTemplates {
        serde_json::from_value(json!({"defaultTemplate":{"added":{"template":body}}}))
            .expect("templates")
    }

    #[test]
    fn known_helpers_compile_and_unknown_helpers_reject() -> Result<()> {
        for good in [
            "{{json after}}",
            "{{#if after}}{{after.id}}{{else}}none{{/if}}",
            "{{#each after}}{{@key}}={{this}}{{/each}}",
            "{{lookup after \"id\"}} {{#if (eq operation \"ADD\")}}a{{/if}}",
            "{{operation}} {{{raw_value}}}",
        ] {
            Renderer::new(Some(&templates(good))).with_context(|| good.to_owned())?;
        }
        for bad in [
            "{{missing after}}",
            "{{#missing after}}x{{/missing}}",
            "{{#if (missing after)}}x{{/if}}",
            "{{> partial}}",
            "{{#if}",
        ] {
            assert!(Renderer::new(Some(&templates(bad))).is_err(), "{bad}");
        }
        Ok(())
    }

    #[test]
    fn renders_compiled_templates() -> Result<()> {
        let source = "{\"id\":{{json after.id}}}";
        let renderer = Renderer::new(Some(&templates(source)))?;
        assert_eq!(
            renderer.render(source, &json!({"after":{"id":"a"}}))?,
            "{\"id\":\"a\"}"
        );
        Ok(())
    }
}
