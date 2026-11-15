// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Command templates compiled once at construction.
//!
//! Templates pass the shared parameter-only rule
//! (`drasi_reaction_common::sql_template`), so every value is a `{{param …}}`
//! that renders as a `?` placeholder and is bound positionally. Rendering
//! runs in strict mode: a referenced field that the change does not carry is
//! a render failure, which is permanent for that change (D-5).

use super::config::MySqlStoredProcConfig;
use anyhow::{anyhow, Context, Result};
use handlebars::{
    Context as RenderState, Handlebars, Helper, HelperResult, Output, RenderContext,
    RenderErrorReason,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// Compiled command templates keyed by their source text.
pub struct Commands {
    registry: Handlebars<'static>,
    names: HashMap<String, String>,
    parameters: Arc<Mutex<Vec<Value>>>,
}

/// One rendered command.
pub struct Rendered {
    pub sql: String,
    pub parameters: Vec<Value>,
}

impl Commands {
    pub fn compile(config: &MySqlStoredProcConfig) -> Result<Self> {
        let parameters = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Handlebars::new();
        registry.set_strict_mode(true);
        let buffer = parameters.clone();
        registry.register_helper(
            "param",
            Box::new(
                move |helper: &Helper<'_>,
                      _: &Handlebars<'_>,
                      _: &RenderState,
                      _: &mut RenderContext<'_, '_>,
                      out: &mut dyn Output|
                      -> HelperResult {
                    let value = helper.param(0).ok_or_else(|| {
                        RenderErrorReason::Other("`param` requires one argument".into())
                    })?;
                    if value.is_value_missing() {
                        return Err(RenderErrorReason::Other(
                            "`param` referenced a field that the change does not carry".into(),
                        )
                        .into());
                    }
                    buffer
                        .lock()
                        .map_err(|_| RenderErrorReason::Other("parameter buffer poisoned".into()))?
                        .push(value.value().clone());
                    out.write("?")?;
                    Ok(())
                },
            ),
        );
        let mut names = HashMap::new();
        for (index, template) in config.templates().enumerate() {
            if names.contains_key(template) {
                continue;
            }
            let name = format!("command-{index}");
            registry
                .register_template_string(&name, template)
                .with_context(|| format!("invalid MySQL command template: {template}"))?;
            names.insert(template.to_owned(), name);
        }
        Ok(Self {
            registry,
            names,
            parameters,
        })
    }

    /// Renders `template` (a configured template) against `context`.
    pub fn render(&self, template: &str, context: &Value) -> Result<Rendered> {
        let name = self
            .names
            .get(template)
            .context("MySQL command template was not compiled")?;
        let mut buffer = self
            .parameters
            .lock()
            .map_err(|_| anyhow!("parameter buffer poisoned"))?;
        buffer.clear();
        drop(buffer);
        let sql = self.registry.render(name, context);
        let parameters = std::mem::take(
            &mut *self
                .parameters
                .lock()
                .map_err(|_| anyhow!("parameter buffer poisoned"))?,
        );
        Ok(Rendered {
            sql: sql?.trim().to_owned(),
            parameters,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compiled(template: &str) -> Result<(Commands, String)> {
        let config = MySqlStoredProcConfig::parse(json!({
            "database": "d", "user": "u",
            "defaultTemplate": {"added": {"template": template}}
        }))?;
        Ok((Commands::compile(&config)?, template.to_owned()))
    }

    #[test]
    fn parameters_bind_in_order_and_never_enter_sql() -> Result<()> {
        let (commands, template) =
            compiled("CALL add({{param after.name}}, {{param after.id}}, {{param after.name}})")?;
        let hostile = "x'); DROP TABLE users; --";
        let rendered = commands.render(&template, &json!({"after": {"id": 7, "name": hostile}}))?;
        assert_eq!(rendered.sql, "CALL add(?, ?, ?)");
        assert_eq!(
            rendered.parameters,
            [json!(hostile), json!(7), json!(hostile)]
        );
        let again = commands.render(&template, &json!({"after": {"id": 8, "name": "n"}}))?;
        assert_eq!(again.parameters, [json!("n"), json!(8), json!("n")]);
        Ok(())
    }

    #[test]
    fn missing_fields_fail_and_conditions_can_exclude() -> Result<()> {
        let (commands, template) = compiled("CALL x({{param after.missing}})")?;
        assert!(commands.render(&template, &json!({"after": {}})).is_err());
        let (commands, template) =
            compiled("{{#if after.active}}CALL x({{param after.id}}){{/if}}")?;
        assert_eq!(
            commands
                .render(&template, &json!({"after": {"id": 1, "active": false}}))?
                .sql,
            ""
        );
        Ok(())
    }
}
