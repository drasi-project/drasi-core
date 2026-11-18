// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Command templates compiled once at construction.
//!
//! Templates are compiled by the shared native template engine
//! (`drasi-computation-template-common`) under its parameter-only rule, so
//! every value is a `{{param …}}` that renders as a `?` placeholder and is
//! bound positionally. Rendering is strict: a referenced field that the
//! change does not carry is a render failure, which is permanent for that
//! change (D-5).

use super::config::MySqlStoredProcConfig;
use anyhow::{Context, Result};
use drasi_computation_template_common::{ParameterTemplate, Placeholder};
use serde_json::Value;
use std::collections::HashMap;

/// Compiled command templates keyed by their source text.
pub struct Commands {
    templates: HashMap<String, ParameterTemplate>,
}

/// One rendered command.
pub struct Rendered {
    pub sql: String,
    pub parameters: Vec<Value>,
}

impl Commands {
    pub fn compile(config: &MySqlStoredProcConfig) -> Result<Self> {
        let mut templates = HashMap::new();
        for template in config.templates() {
            if templates.contains_key(template) {
                continue;
            }
            let compiled = ParameterTemplate::compile(template, Placeholder::Question)
                .with_context(|| format!("invalid MySQL command template: {template}"))?;
            templates.insert(template.to_owned(), compiled);
        }
        Ok(Self { templates })
    }

    /// Renders `template` (a configured template) against `context`.
    pub fn render(&self, template: &str, context: &Value) -> Result<Rendered> {
        let rendered = self
            .templates
            .get(template)
            .context("MySQL command template was not compiled")?
            .render(context)?;
        Ok(Rendered {
            sql: rendered.sql.trim().to_owned(),
            parameters: rendered.parameters,
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
