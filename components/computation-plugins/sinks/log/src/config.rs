// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Log sink configuration: fields, published schema and defaults.
//!
//! See `CONFIGURATION.md` in this crate for the operator reference.

use anyhow::{ensure, Result};
use drasi_computation_contracts::{ConfigurationConstraint, OutputAliases};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Default `log` target used when `target` is omitted.
pub const DEFAULT_TARGET: &str = "drasi.log";

/// One Handlebars template.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplateSpec {
    /// Handlebars source. An empty template selects the no-template `format`.
    pub template: String,
}

impl TemplateSpec {
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
        }
    }
}

/// Templates for the three operation groups. `updated` serves both
/// `REPLACE` and `PATCH`; the context's `operation` tells them apart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationTemplates {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<TemplateSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<TemplateSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<TemplateSpec>,
}

impl OperationTemplates {
    /// The specs in `[added, updated, deleted]` order.
    pub(crate) fn specs(&self) -> [Option<&TemplateSpec>; 3] {
        [
            self.added.as_ref(),
            self.updated.as_ref(),
            self.deleted.as_ref(),
        ]
    }
}

/// Representation of a change that has no template (or an empty one), and of
/// a change whose template failed under the `fallback` policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogFormat {
    /// The complete change context as compact JSON.
    #[default]
    Json,
    /// Legacy one-line form: `[ADD] {after}`, `[UPDATE] {before} -> {after}`,
    /// `[DELETE] {before}`.
    Compact,
}

/// `log` record level. Ignored for `stdout`/`stderr` output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl From<LogLevel> for log::Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Error => log::Level::Error,
            LogLevel::Warn => log::Level::Warn,
            LogLevel::Info => log::Level::Info,
            LogLevel::Debug => log::Level::Debug,
            LogLevel::Trace => log::Level::Trace,
        }
    }
}

/// Where rendered lines go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogOutput {
    /// The process `log` facade, with `level` and `target`.
    #[default]
    Log,
    /// Complete lines written to standard output and flushed per input.
    Stdout,
    /// Complete lines written to standard error and flushed per input.
    Stderr,
}

/// What a template render failure does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RenderFailurePolicy {
    /// Emit the change in the configured `format` instead, with a warning
    /// (legacy behaviour). The input completes.
    #[default]
    Fallback,
    /// Fail the input; nothing from it is emitted.
    Strict,
}

fn default_target() -> String {
    DEFAULT_TARGET.into()
}

/// Configuration of `drasi.standard/log`. Unknown keys are rejected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogSinkConfig {
    /// Aliases for incoming outputs: alias to `{component, port}`.
    #[serde(default)]
    pub outputs: OutputAliases,
    /// Per-output templates keyed by output alias, or by a dotted suffix of
    /// one (`orders` matches `east.orders`).
    #[serde(default)]
    pub routes: BTreeMap<String, OperationTemplates>,
    /// Templates for operations a matching route does not define.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<OperationTemplates>,
    #[serde(default)]
    pub format: LogFormat,
    /// Emit one header line per input before its changes.
    #[serde(default)]
    pub include_header: bool,
    #[serde(default)]
    pub level: LogLevel,
    #[serde(default = "default_target")]
    pub target: String,
    #[serde(default)]
    pub output: LogOutput,
    #[serde(default)]
    pub render_failure_policy: RenderFailurePolicy,
}

impl Default for LogSinkConfig {
    fn default() -> Self {
        Self {
            outputs: OutputAliases::default(),
            routes: BTreeMap::new(),
            default_template: None,
            format: LogFormat::default(),
            include_header: false,
            level: LogLevel::default(),
            target: default_target(),
            output: LogOutput::default(),
            render_failure_policy: RenderFailurePolicy::default(),
        }
    }
}

impl LogSinkConfig {
    /// Parses and checks everything that does not need the graph's inputs.
    /// Template compilation and route resolution happen in
    /// [`crate::render::Renderer`].
    pub fn parse(value: Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        self.outputs.validate()?;
        ensure!(
            !self.target.is_empty() && !self.target.chars().any(char::is_control),
            "log target must be nonempty and contain no control characters"
        );
        for route in self.routes.keys() {
            ensure!(!route.is_empty(), "log route names must be nonempty");
        }
        Ok(())
    }

    /// The published field definitions, defaults and nested schemas.
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        ConfigDefinition::try_new([
            Field::new(
                "outputs",
                Object,
                false,
                "Aliases for incoming outputs: alias to {component, port}. Unaliased outputs are named by their component UUID.",
            )
            .constraint(ConfigurationConstraint::output_aliases())
            .default(json!({})),
            Field::new(
                "routes",
                Object,
                false,
                "Templates per output: route name to {added?, updated?, deleted?}, each {template: string}. A route name is an output alias, or a dotted suffix of one ('orders' matches 'east.orders'); an exact alias wins. Every route must select at least one incoming output.",
            )
            .constraint(constraint(json!({
                "type": "object",
                "propertyNames": {"minLength": 1},
                "additionalProperties": operation_templates_schema(),
            })))
            .default(json!({})),
            Field::new(
                "defaultTemplate",
                Object,
                false,
                "Templates {added?, updated?, deleted?}, each {template: string}, for operations that the output's route does not define.",
            )
            .constraint(constraint(operation_templates_schema()))
            .default_description("none: such changes use `format`"),
            Field::new(
                "format",
                String,
                false,
                "Representation of a change without a template, and of a failed render under the fallback policy: 'json' (full change context) or 'compact' ([ADD] after, [UPDATE] before -> after, [DELETE] before).",
            )
            .allowed([json!("json"), json!("compact")])
            .default("json"),
            Field::new(
                "includeHeader",
                Boolean,
                false,
                "Emit one header line per input naming the output, sequence and change count; change lines are then indented.",
            )
            .default(false),
            Field::new(
                "level",
                String,
                false,
                "Level of each log record when output is 'log'.",
            )
            .allowed(["error", "warn", "info", "debug", "trace"].map(|level| json!(level)))
            .default("info"),
            Field::new(
                "target",
                String,
                false,
                "Target of each log record when output is 'log'.",
            )
            .constraint(constraint(json!({"type": "string", "minLength": 1})))
            .default(DEFAULT_TARGET),
            Field::new(
                "output",
                String,
                false,
                "Where lines go: 'log' (the process log facade), 'stdout' or 'stderr' (complete lines, flushed before the input completes).",
            )
            .allowed([json!("log"), json!("stdout"), json!("stderr")])
            .default("log"),
            Field::new(
                "renderFailurePolicy",
                String,
                false,
                "On a template render failure: 'fallback' emits the change in `format` with a warning; 'strict' fails the input and emits nothing from it.",
            )
            .allowed([json!("fallback"), json!("strict")])
            .default("fallback"),
        ])
        .unwrap_or_else(|error| unreachable!("constant log sink configuration: {error}"))
    }
}

fn template_spec_schema() -> Value {
    json!({
        "type": "object",
        "required": ["template"],
        "properties": {"template": {"type": "string"}},
        "additionalProperties": false,
    })
}

fn operation_templates_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "added": template_spec_schema(),
            "updated": template_spec_schema(),
            "deleted": template_spec_schema(),
        },
        "additionalProperties": false,
    })
}

fn constraint(schema: Value) -> ConfigurationConstraint {
    ConfigurationConstraint::try_new(schema)
        .unwrap_or_else(|error| unreachable!("constant log sink schema: {error}"))
}
