// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Output templates, compiled once at construction by the shared native
//! template engine (strict, unescaped, shared helper set).
//!
//! Routing matches the shared reaction protocol and its route validation: the
//! output alias, then the alias's last dotted segment, then
//! `defaultTemplate`; a route that has no template for the operation falls
//! through to the next candidate. Unlike the legacy renderer, a render
//! failure is an error for the caller's failure policy, never a silent
//! fallback to the canonical payload (D-5).

use anyhow::{Context, Result};
use drasi_computation_contracts::{ChangeNotification, ChangeRoute};
use drasi_computation_template_common::{change_context, Templates};
use drasi_grpc_common::output::config::{GrpcQueryConfig, OutputTemplates};
use drasi_reaction_common::TemplateSpec;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

type Spec = TemplateSpec<drasi_grpc_common::output::config::GrpcTemplateExtension>;

/// One operation's compiled body and metadata templates.
struct Compiled {
    /// Body template source; `None` for an empty body template.
    body: Option<String>,
    /// Metadata name (as configured) and template source.
    metadata: Vec<(String, String)>,
}

#[derive(Default)]
struct CompiledRoute {
    added: Option<Compiled>,
    updated: Option<Compiled>,
    deleted: Option<Compiled>,
}

impl CompiledRoute {
    fn for_route(&self, route: ChangeRoute) -> Option<&Compiled> {
        match route {
            ChangeRoute::Added => self.added.as_ref(),
            ChangeRoute::Updated => self.updated.as_ref(),
            ChangeRoute::Deleted => self.deleted.as_ref(),
        }
    }
}

/// The rendered parts of one change. Both are empty when no template applies.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Rendered {
    pub body: Option<Value>,
    pub metadata: BTreeMap<String, String>,
}

pub(crate) struct Renderer {
    templates: Templates,
    routes: HashMap<String, CompiledRoute>,
    default: Option<CompiledRoute>,
}

impl Renderer {
    /// Compiles every template; `None` when nothing would ever render.
    pub fn new(templates: Option<&OutputTemplates>) -> Result<Option<Self>> {
        let Some(templates) = templates.filter(|templates| templates.has_renderable_templates())
        else {
            return Ok(None);
        };
        let mut engine = Templates::new();
        let mut compile = |query: &GrpcQueryConfig| -> Result<CompiledRoute> {
            let mut one = |spec: &Option<Spec>| -> Result<Option<Compiled>> {
                let Some(spec) = spec else { return Ok(None) };
                let mut register = |template: &str| -> Result<String> {
                    engine.compile(template)?;
                    Ok(template.to_owned())
                };
                let body = if spec.template.trim().is_empty() {
                    None
                } else {
                    Some(register(&spec.template)?)
                };
                let mut metadata = Vec::new();
                for (key, template) in &spec.extension.metadata {
                    metadata.push((key.clone(), register(template)?));
                }
                metadata.sort();
                Ok(Some(Compiled { body, metadata }))
            };
            Ok(CompiledRoute {
                added: one(&query.added)?,
                updated: one(&query.updated)?,
                deleted: one(&query.deleted)?,
            })
        };
        let default = templates
            .default_template
            .as_ref()
            .map(&mut compile)
            .transpose()?;
        let mut routes = HashMap::new();
        for (key, query) in &templates.routes {
            routes.insert(key.clone(), compile(query)?);
        }
        Ok(Some(Self {
            templates: engine,
            routes,
            default,
        }))
    }

    fn resolve(&self, alias: &str, route: ChangeRoute) -> Option<&Compiled> {
        if let Some(found) = self.routes.get(alias).and_then(|r| r.for_route(route)) {
            return Some(found);
        }
        if let Some(segment) = alias.rsplit('.').next().filter(|segment| *segment != alias) {
            if let Some(found) = self.routes.get(segment).and_then(|r| r.for_route(route)) {
                return Some(found);
            }
        }
        self.default.as_ref().and_then(|r| r.for_route(route))
    }

    /// Renders the selected templates against the change's standard context
    /// (the canonical change JSON). A body must render valid JSON.
    pub fn render(&self, notification: &ChangeNotification) -> Result<Rendered> {
        let Some(compiled) = self.resolve(notification.alias(), notification.change().route())
        else {
            return Ok(Rendered::default());
        };
        let context = change_context(notification)?;
        let body = match &compiled.body {
            Some(source) => Some(
                self.templates
                    .render_json(source, &context)
                    .context("body template")?,
            ),
            None => None,
        };
        let mut metadata = BTreeMap::new();
        for (key, source) in &compiled.metadata {
            let value = self
                .templates
                .render(source, &context)
                .with_context(|| format!("metadata template {key} failed to render"))?;
            metadata.insert(key.clone(), value);
        }
        Ok(Rendered { body, metadata })
    }
}
