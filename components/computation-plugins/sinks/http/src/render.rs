// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Template compilation and rendering. Every template is compiled once, at
//! construction, by the shared native template engine; rendering a change
//! never falls back to another body, URL or header (D-5).

use anyhow::{Context, Result};
use drasi_computation_template_common::Templates;
use drasi_http_common::reaction::config::HttpOutputTemplates;
use serde_json::Value;

/// Every configured template, compiled by the shared native template engine
/// (`drasi-computation-template-common`): strict, unescaped, with the shared
/// helper set checked statically.
pub(crate) struct Renderer {
    templates: Templates,
}

impl Renderer {
    /// Compiles every body, URL and header template.
    pub fn new(templates: Option<&HttpOutputTemplates>) -> Result<Self> {
        let mut renderer = Self {
            templates: Templates::new(),
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
                renderer
                    .templates
                    .compile(&spec.template)
                    .context("body template")?;
                renderer
                    .templates
                    .compile(&spec.extension.url)
                    .context("url template")?;
                for (name, value) in &spec.extension.headers {
                    renderer
                        .templates
                        .compile(value)
                        .with_context(|| format!("header {name} template"))?;
                }
            }
        }
        Ok(renderer)
    }

    /// Renders a template compiled at construction.
    pub fn render(&self, source: &str, context: &Value) -> Result<String> {
        Ok(self.templates.render(source, context)?)
    }

    /// Renders a body template compiled at construction as JSON.
    pub fn render_json(&self, source: &str, context: &Value) -> Result<Value> {
        Ok(self.templates.render_json(source, context)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn templates(spec: Value) -> HttpOutputTemplates {
        serde_json::from_value(json!({"defaultTemplate":{"added":spec}})).expect("templates")
    }

    #[test]
    fn body_url_and_header_templates_are_all_checked_at_construction() {
        for spec in [
            json!({"template": "{{lowercase after.id}}"}),
            json!({"template": "", "url": "/x/{{#if after}}{{upper after.id}}{{/if}}"}),
            json!({"template": "", "headers": {"x-id": "{{> partial}}"}}),
        ] {
            assert!(
                Renderer::new(Some(&templates(spec.clone()))).is_err(),
                "{spec}"
            );
        }
    }

    #[test]
    fn renders_with_the_shared_engine_semantics() -> Result<()> {
        let body = "{\"name\":{{json after.name}}}";
        let header = "{{after.name}}";
        let renderer = Renderer::new(Some(&templates(json!({
            "template": body, "headers": {"x-name": header}
        }))))?;
        let context = json!({"after": {"name": "O'Brien"}});
        assert_eq!(
            renderer.render_json(body, &context)?,
            json!({"name": "O'Brien"})
        );
        // Unescaped, and strict about missing fields.
        assert_eq!(renderer.render(header, &context)?, "O'Brien");
        assert!(renderer
            .render(header, &json!({"before": {"name": "a"}}))
            .is_err());
        Ok(())
    }
}
