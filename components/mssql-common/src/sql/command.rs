// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A command template compiled once and rendered many times.
//!
//! [`super::render_command`] builds a Handlebars registry for every render.
//! A [`CommandTemplate`] compiles its source once (at component construction)
//! and reuses the registry, with the same strict mode and the same `param` and
//! `json` helpers. It also enforces the SQL Server parameter limit for one
//! command, so a command that the server would refuse fails before any
//! effect, with a clear and permanent error.
//!
//! A `CommandTemplate` does not decide which templates are safe. Callers that
//! require parameter-only SQL (the native stored-procedure sink) validate the
//! source first.

use anyhow::{anyhow, Result};
use handlebars::Handlebars;
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};

/// SQL Server accepts at most 2 100 parameters in one RPC request. Tiberius
/// sends a parameterized command through `sp_executesql`, whose statement and
/// parameter-declaration arguments use two of them, so one command may bind at
/// most 2 098 values.
pub const MAX_RPC_PARAMETERS: usize = 2_100;
/// The largest number of `{{param …}}` values one rendered command may bind.
pub const MAX_BOUND_PARAMETERS: usize = MAX_RPC_PARAMETERS - 2;

const NAME: &str = "command";

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
    registry: Handlebars<'static>,
    parameters: Arc<Mutex<Vec<Value>>>,
    empty: bool,
}

impl CommandTemplate {
    /// Compiles `source`. An empty or whitespace-only source renders nothing.
    pub fn compile(source: &str) -> Result<Self> {
        let parameters = Arc::new(Mutex::new(Vec::new()));
        let mut registry = super::template::build_handlebars(Arc::clone(&parameters));
        let empty = source.trim().is_empty();
        if !empty {
            registry
                .register_template_string(NAME, source)
                .map_err(|error| anyhow!("invalid Handlebars command template: {error}"))?;
        }
        Ok(Self {
            registry,
            parameters,
            empty,
        })
    }

    /// Renders one command. Returns `None` when the template is empty or
    /// renders only whitespace (a condition excluded the change).
    ///
    /// Fails on a missing field (strict mode) and with
    /// [`ParameterLimitExceeded`] when the command binds too many values.
    pub fn render(&mut self, context: &Map<String, Value>) -> Result<Option<RenderedCommand>> {
        if self.empty {
            return Ok(None);
        }
        self.buffer()?.clear();
        let rendered = self.registry.render(NAME, context);
        let parameters = std::mem::take(&mut *self.buffer()?);
        let sql =
            rendered.map_err(|error| anyhow!("failed to render command template: {error}"))?;
        if sql.trim().is_empty() {
            return Ok(None);
        }
        if parameters.len() > MAX_BOUND_PARAMETERS {
            return Err(ParameterLimitExceeded {
                count: parameters.len(),
            }
            .into());
        }
        Ok(Some(RenderedCommand { sql, parameters }))
    }

    fn buffer(&self) -> Result<std::sync::MutexGuard<'_, Vec<Value>>> {
        self.parameters
            .lock()
            .map_err(|_| anyhow!("command parameter buffer poisoned"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn compiled_templates_render_repeatedly_with_fresh_parameters() -> Result<()> {
        let mut template =
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
            .render(&Map::new())?
            .is_none());
        let mut conditional =
            CommandTemplate::compile("{{#if after.on}}EXEC p {{param after.id}}{{/if}}")?;
        assert!(conditional
            .render(&context(json!({"after": {"on": false, "id": 1}})))?
            .is_none());
        Ok(())
    }

    #[test]
    fn the_parameter_limit_is_enforced_per_command() -> Result<()> {
        let values: Vec<u64> = (0..MAX_BOUND_PARAMETERS as u64 + 1).collect();
        let mut template =
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
