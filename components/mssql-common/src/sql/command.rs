// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A command template compiled once and rendered many times.
//!
//! A [`CommandTemplate`] is compiled once (at component construction) by the
//! shared native template engine (`drasi-computation-template-common`) under
//! its parameter-only rule: strict rendering, the shared helper set, and
//! `{{param …}}` as the only way to emit a value, bound as `@P1…@Pn`. It also
//! enforces the SQL Server parameter limit for one command, so a command that
//! the server would refuse fails before any effect, with a clear and
//! permanent error.
//!
//! The legacy reaction keeps [`super::render_command`].

use anyhow::Result;
use drasi_computation_template_common::{ParameterTemplate, Placeholder};
use serde_json::Value;

/// SQL Server accepts at most 2 100 parameters in one RPC request. Tiberius
/// sends a parameterized command through `sp_executesql`, whose statement and
/// parameter-declaration arguments use two of them, so one command may bind at
/// most 2 098 values.
pub const MAX_RPC_PARAMETERS: usize = 2_100;
/// The largest number of `{{param …}}` values one rendered command may bind.
pub const MAX_BOUND_PARAMETERS: usize = MAX_RPC_PARAMETERS - 2;

/// A rendered command exceeded [`MAX_BOUND_PARAMETERS`]. Rendering the same
/// change again produces the same count, so this failure is permanent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterLimitExceeded {
    /// Values the command tried to bind.
    pub count: usize,
}

impl std::fmt::Display for ParameterLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the rendered command binds {} parameters; SQL Server accepts at most {} \
             (2 100 per request, two of which carry the statement); split the procedure \
             call or pass a JSON document with {{{{param (json …)}}}}",
            self.count, MAX_BOUND_PARAMETERS
        )
    }
}

impl std::error::Error for ParameterLimitExceeded {}

/// One rendered command: SQL text with `@P1…@Pn` placeholders and its values.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedCommand {
    pub sql: String,
    pub parameters: Vec<Value>,
}

/// A compiled, reusable command template.
pub struct CommandTemplate {
    template: ParameterTemplate,
}

impl CommandTemplate {
    /// Compiles `source` under the parameter-only rule. An empty or
    /// whitespace-only source renders nothing.
    pub fn compile(source: &str) -> Result<Self> {
        Ok(Self {
            template: ParameterTemplate::compile(source, Placeholder::AtP)?,
        })
    }

    /// Renders one command. Returns `None` when the template is empty or
    /// renders only whitespace (a condition excluded the change).
    ///
    /// Fails on a missing field (strict mode) and with
    /// [`ParameterLimitExceeded`] when the command binds too many values.
    pub fn render(&self, context: &Value) -> Result<Option<RenderedCommand>> {
        if self.template.is_empty() {
            return Ok(None);
        }
        let rendered = self.template.render(context)?;
        if rendered.sql.trim().is_empty() {
            return Ok(None);
        }
        if rendered.parameters.len() > MAX_BOUND_PARAMETERS {
            return Err(ParameterLimitExceeded {
                count: rendered.parameters.len(),
            }
            .into());
        }
        Ok(Some(RenderedCommand {
            sql: rendered.sql,
            parameters: rendered.parameters,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context(value: Value) -> Value {
        value
    }

    #[test]
    fn compiled_templates_render_repeatedly_with_fresh_parameters() -> Result<()> {
        let template =
            CommandTemplate::compile("EXEC p {{param after.id}}, {{param (json after)}}")?;
        for id in [1, 2] {
            let rendered = template
                .render(&context(json!({"after": {"id": id}})))?
                .expect("command");
            assert_eq!(rendered.sql, "EXEC p @P1, @P2");
            assert_eq!(
                rendered.parameters,
                vec![json!(id), json!(format!("{{\"id\":{id}}}"))]
            );
        }
        assert!(template.render(&context(json!({"after": {}}))).is_err());
        let rendered = template
            .render(&context(json!({"after": {"id": 3}})))?
            .expect("command");
        assert_eq!(
            rendered.parameters.len(),
            2,
            "a failed render leaks nothing"
        );
        Ok(())
    }

    #[test]
    fn empty_and_excluded_commands_render_nothing() -> Result<()> {
        assert!(CommandTemplate::compile("  ")?
            .render(&json!({}))?
            .is_none());
        let conditional =
            CommandTemplate::compile("{{#if after.on}}EXEC p {{param after.id}}{{/if}}")?;
        assert!(conditional
            .render(&context(json!({"after": {"on": false, "id": 1}})))?
            .is_none());
        Ok(())
    }

    #[test]
    fn the_parameter_limit_is_enforced_per_command() -> Result<()> {
        let values: Vec<u64> = (0..MAX_BOUND_PARAMETERS as u64 + 1).collect();
        let template =
            CommandTemplate::compile("EXEC p {{#each after.values}}{{param this}},{{/each}}")?;
        let error = template
            .render(&context(json!({"after": {"values": values}})))
            .expect_err("over the limit");
        assert_eq!(
            error.downcast_ref::<ParameterLimitExceeded>(),
            Some(&ParameterLimitExceeded {
                count: MAX_BOUND_PARAMETERS + 1
            })
        );
        let rendered = template
            .render(&context(
                json!({"after": {"values": &values[..MAX_BOUND_PARAMETERS]}}),
            ))?
            .expect("command");
        assert_eq!(rendered.parameters.len(), MAX_BOUND_PARAMETERS);
        Ok(())
    }
}
