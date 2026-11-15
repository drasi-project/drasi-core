// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Template compilation, route resolution and change formatting.
//!
//! Every template is compiled once, when the renderer is built (construction
//! or an in-place update), and registered under a private name. Each compiled
//! template is then checked statically: every helper call must name a
//! supported helper, and partials and decorators are rejected. A static check
//! covers every branch (`{{#if}}`/`{{else}}`), unlike a trial render.

use crate::config::{LogFormat, LogSinkConfig, OperationTemplates, RenderFailurePolicy};
use anyhow::{bail, Context, Result};
use drasi_computation_contracts::{ChangeNotification, ChangeRoute, SinkOutputs};
use handlebars::template::{Parameter, Template, TemplateElement};
use handlebars::Handlebars;
use serde_json::Value;
use std::collections::BTreeMap;

/// Helpers a template may call: the Handlebars built-ins plus `json`.
/// The list is fixed, so optional Handlebars features enabled elsewhere in a
/// build never change which templates are accepted.
pub const SUPPORTED_HELPERS: &[&str] = &[
    "if", "unless", "each", "with", "lookup", "raw", "log", "eq", "ne", "gt", "gte", "lt", "lte",
    "and", "or", "not", "len", "json",
];

const OPERATIONS: [&str; 3] = ["added", "updated", "deleted"];

fn group(route: ChangeRoute) -> usize {
    match route {
        ChangeRoute::Added => 0,
        ChangeRoute::Updated => 1,
        ChangeRoute::Deleted => 2,
    }
}

/// What one operation group of one output renders with.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Selection {
    /// A registered template name.
    Template(String),
    /// No template, or an empty one: the configured `format`.
    Plain,
}

/// The outcome of rendering one change.
#[derive(Debug)]
pub(crate) struct Rendered {
    pub message: String,
    /// The render error when a template failed and `fallback` applied.
    pub fallback: Option<String>,
}

/// Compiled templates and the resolved per-output selections.
pub(crate) struct Renderer {
    handlebars: Handlebars<'static>,
    by_alias: BTreeMap<String, [Selection; 3]>,
    default: [Selection; 3],
    format: LogFormat,
    policy: RenderFailurePolicy,
}

impl Renderer {
    /// Compiles and validates every template and resolves every route
    /// against the bound outputs.
    pub fn new(config: &LogSinkConfig, outputs: &SinkOutputs) -> Result<Self> {
        let mut renderer = Self::compile(config)?;
        let resolved = resolve_routes(config.routes.keys().map(String::as_str), outputs.aliases())?;
        for alias in outputs.aliases() {
            let route = resolved.get(alias).and_then(|route| {
                config
                    .routes
                    .get_key_value(route)
                    .map(|(name, templates)| (name.as_str(), templates))
            });
            renderer
                .by_alias
                .insert(alias.to_owned(), selections(route, config));
        }
        Ok(renderer)
    }

    /// Compiles and validates templates only, for planning without inputs.
    pub fn compile(config: &LogSinkConfig) -> Result<Self> {
        let mut handlebars = Handlebars::new();
        // Log lines are not HTML: never escape rendered values.
        handlebars.register_escape_fn(handlebars::no_escape);
        handlebars.register_helper("json", Box::new(json_helper));
        let named = config
            .routes
            .iter()
            .map(|(route, templates)| (Some(route.as_str()), templates))
            .chain(
                config
                    .default_template
                    .iter()
                    .map(|templates| (None, templates)),
            );
        for (route, templates) in named {
            for (operation, spec) in OPERATIONS.iter().zip(templates.specs()) {
                let Some(spec) = spec.filter(|spec| !spec.template.is_empty()) else {
                    continue;
                };
                let label = match route {
                    Some(route) => format!("route '{route}' {operation}"),
                    None => format!("defaultTemplate {operation}"),
                };
                let template = Template::compile(&spec.template)
                    .with_context(|| format!("invalid log template in {label}"))?;
                validate_template(&template)
                    .with_context(|| format!("invalid log template in {label}"))?;
                handlebars.register_template(&template_name(route, operation), template);
            }
        }
        Ok(Self {
            handlebars,
            by_alias: BTreeMap::new(),
            default: selections(None, config),
            format: config.format,
            policy: config.render_failure_policy,
        })
    }

    /// Renders one change. Under `strict`, a render failure is an error;
    /// under `fallback`, the change is formatted with `format` and the error
    /// is returned for a warning.
    pub fn render(&self, notification: &ChangeNotification) -> Result<Rendered> {
        let context = notification.to_json()?;
        let index = group(notification.change().route());
        let selection = self
            .by_alias
            .get(notification.alias())
            .map_or(&self.default[index], |selections| &selections[index]);
        let Selection::Template(name) = selection else {
            return Ok(Rendered {
                message: format_plain(self.format, &context)?,
                fallback: None,
            });
        };
        match self.handlebars.render(name, &context) {
            Ok(message) => Ok(Rendered {
                message,
                fallback: None,
            }),
            Err(error) => match self.policy {
                RenderFailurePolicy::Strict => Err(anyhow::Error::new(error).context(format!(
                    "log template for output '{}' failed to render",
                    notification.alias()
                ))),
                RenderFailurePolicy::Fallback => Ok(Rendered {
                    message: format_plain(self.format, &context)?,
                    fallback: Some(error.to_string()),
                }),
            },
        }
    }
}

fn template_name(route: Option<&str>, operation: &str) -> String {
    match route {
        Some(route) => format!("route:{route}:{operation}"),
        None => format!("default:{operation}"),
    }
}

/// Per operation group: the route's template if it defines one, otherwise
/// the default template's, otherwise `Plain`. An empty template is `Plain`.
fn selections(
    route: Option<(&str, &OperationTemplates)>,
    config: &LogSinkConfig,
) -> [Selection; 3] {
    let route_specs = route.map(|(_, templates)| templates.specs());
    let default_specs = config
        .default_template
        .as_ref()
        .map(OperationTemplates::specs);
    std::array::from_fn(|index| {
        let from_route = route_specs
            .and_then(|specs| specs[index])
            .map(|spec| (route.map(|(name, _)| name), spec));
        let chosen = from_route.or_else(|| {
            default_specs
                .and_then(|specs| specs[index])
                .map(|spec| (None, spec))
        });
        match chosen {
            Some((route, spec)) if !spec.template.is_empty() => {
                Selection::Template(template_name(route, OPERATIONS[index]))
            }
            _ => Selection::Plain,
        }
    })
}

/// Resolves each output alias to at most one route, using the same rule for
/// validation and lookup:
///
/// 1. a route named exactly as the alias wins;
/// 2. otherwise a route `r` whose dotted suffix `.r` ends the alias matches;
///    two or more such routes for one alias are ambiguous and rejected.
///
/// Every route must be selected for at least one alias, so a route that
/// matches nothing, or is always shadowed by an exact route, is rejected.
pub(crate) fn resolve_routes<'a>(
    routes: impl Iterator<Item = &'a str> + Clone,
    aliases: impl Iterator<Item = &'a str>,
) -> Result<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    for alias in aliases {
        if routes.clone().any(|route| route == alias) {
            resolved.insert(alias.to_owned(), alias.to_owned());
            continue;
        }
        let mut matching = routes.clone().filter(|route| {
            alias.len() > route.len()
                && alias.ends_with(route)
                && alias.as_bytes()[alias.len() - route.len() - 1] == b'.'
        });
        match (matching.next(), matching.next()) {
            (None, _) => {}
            (Some(route), None) => {
                resolved.insert(alias.to_owned(), route.to_owned());
            }
            (Some(first), Some(second)) => bail!(
                "log routes '{first}' and '{second}' both match output '{alias}' by suffix; add a route named '{alias}'"
            ),
        }
    }
    for route in routes {
        if !resolved.values().any(|selected| selected == route) {
            bail!("log route '{route}' does not select any incoming output");
        }
    }
    Ok(resolved)
}

/// Rejects calls to unsupported helpers, partials and decorators anywhere in
/// the template, including inside blocks, `else` branches, sub-expressions
/// and hash arguments.
pub(crate) fn validate_template(template: &Template) -> Result<()> {
    template.elements.iter().try_for_each(validate_element)
}

fn validate_element(element: &TemplateElement) -> Result<()> {
    match element {
        TemplateElement::RawString(_) | TemplateElement::Comment(_) => Ok(()),
        TemplateElement::Expression(helper) | TemplateElement::HtmlExpression(helper) => {
            let name_only = !helper.block && helper.params.is_empty() && helper.hash.is_empty();
            if !name_only {
                validate_helper_name(&helper.name)?;
            } else {
                validate_parameter(&helper.name)?;
            }
            validate_helper_parts(helper)
        }
        TemplateElement::HelperBlock(helper) => {
            validate_helper_name(&helper.name)?;
            validate_helper_parts(helper)
        }
        TemplateElement::PartialExpression(_) | TemplateElement::PartialBlock(_) => {
            bail!("partials are not supported in log templates")
        }
        TemplateElement::DecoratorExpression(_) | TemplateElement::DecoratorBlock(_) => {
            bail!("decorators are not supported in log templates")
        }
    }
}

fn validate_helper_parts(helper: &handlebars::template::HelperTemplate) -> Result<()> {
    helper.params.iter().try_for_each(validate_parameter)?;
    helper.hash.values().try_for_each(validate_parameter)?;
    for template in helper.template.iter().chain(helper.inverse.iter()) {
        validate_template(template)?;
    }
    Ok(())
}

fn validate_helper_name(name: &Parameter) -> Result<()> {
    if let Parameter::Subexpression(_) = name {
        bail!("a helper must be called by name");
    }
    match name.as_name() {
        Some(name) if SUPPORTED_HELPERS.contains(&name) => Ok(()),
        Some(name) => bail!("unknown helper '{name}'"),
        None => bail!("a helper must be called by name"),
    }
}

fn validate_parameter(parameter: &Parameter) -> Result<()> {
    match parameter {
        Parameter::Subexpression(expression) => validate_element(expression.as_element()),
        Parameter::Name(_) | Parameter::Path(_) | Parameter::Literal(_) => Ok(()),
    }
}

/// `{{json value}}`: compact JSON of any value; a missing value is `null`.
/// Matches the legacy log reaction helper.
fn json_helper(
    helper: &handlebars::Helper,
    _: &Handlebars,
    _: &handlebars::Context,
    _: &mut handlebars::RenderContext,
    out: &mut dyn handlebars::Output,
) -> handlebars::HelperResult {
    if let Some(value) = helper.param(0) {
        let text = serde_json::to_string(value.value()).unwrap_or_else(|_| "null".to_owned());
        out.write(&text)?;
    }
    Ok(())
}

/// The no-template representation of one change context.
pub(crate) fn format_plain(format: LogFormat, context: &Value) -> Result<String> {
    match format {
        LogFormat::Json => Ok(serde_json::to_string(context)?),
        LogFormat::Compact => Ok(format_compact(context)),
    }
}

/// Legacy compact form. An image the record does not carry is shown as the
/// element identity `{namespace, id}`. A patch that removes properties
/// appends ` removed [...]`.
fn format_compact(context: &Value) -> String {
    let image = |name: &str| {
        context
            .get(name)
            .unwrap_or_else(|| context.get("identity").unwrap_or(&Value::Null))
            .to_string()
    };
    match context.get("operation").and_then(Value::as_str) {
        Some("ADD") => format!("[ADD] {}", image("after")),
        Some("DELETE") => format!("[DELETE] {}", image("before")),
        _ => {
            let mut line = format!("[UPDATE] {} -> {}", image("before"), image("after"));
            if let Some(removed) = context
                .get("removedProperties")
                .and_then(Value::as_array)
                .filter(|removed| !removed.is_empty())
            {
                line.push_str(&format!(" removed {}", Value::Array(removed.clone())));
            }
            line
        }
    }
}
