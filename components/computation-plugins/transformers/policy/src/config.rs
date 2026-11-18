// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Configuration, its published definition and update classification.

use anyhow::{ensure, Context, Result};
use drasi_computation_contracts::StreamId;
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, UpdateBoundary},
    update_planning::BoundaryPlanner,
    ConfigDefinition, ConfigOption, ConfigType,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

use crate::template::Template;

/// Property names the transformer always owns on decision elements.
pub(crate) const STATUS_PROPERTY: &str = "policy_status";
pub(crate) const ERROR_PROPERTY: &str = "policy_error";
pub(crate) const HASH_PROPERTY: &str = "policy_hash";
pub(crate) const FINGERPRINT_PROPERTY: &str = "policy_input_fingerprint";
pub(crate) const RESERVED: [&str; 4] = [
    STATUS_PROPERTY,
    ERROR_PROPERTY,
    HASH_PROPERTY,
    FINGERPRINT_PROPERTY,
];

/// Roots available to `input` selectors.
pub(crate) const INPUT_ROOTS: &[&str] = &[
    "id",
    "namespace",
    "kind",
    "labels",
    "properties",
    "in_node",
    "out_node",
];
/// Roots available to `output_properties` selectors.
pub(crate) const OUTPUT_ROOTS: &[&str] = &["decision", "input", "element", "status", "error"];

const MIB: u64 = 1024 * 1024;

fn default_rego_version() -> RegoVersion {
    RegoVersion::V1
}
fn default_input() -> Value {
    json!("$properties")
}
fn default_output_label() -> String {
    "PolicyDecision".into()
}
fn default_output_properties() -> Map<String, Value> {
    Map::from_iter([("decision".to_owned(), json!("$decision"))])
}
fn default_true() -> bool {
    true
}
fn default_timeout_ms() -> u64 {
    100
}
fn default_check_interval() -> u32 {
    64
}
fn default_max_input_bytes() -> u64 {
    MIB
}
fn default_max_decision_bytes() -> u64 {
    MIB
}
fn default_max_retained_inputs() -> u64 {
    100_000
}
fn default_max_policy_bytes() -> u64 {
    MIB
}
fn default_max_data_bytes() -> u64 {
    16 * MIB
}

/// One Rego module: a unique name (used in error locations) and its source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Module {
    pub name: String,
    pub source: String,
}

/// Rego language version of every module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RegoVersion {
    V0,
    V1,
}

/// What a per-record evaluation failure does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OnError {
    /// Emit a decision element with `policy_status = "error"` and the message.
    #[default]
    Emit,
    /// Fail the whole input batch (or refuse the update); nothing is emitted.
    Fail,
}

/// The complete, normalized configuration of `drasi.standard/policy`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PolicyConfig {
    pub stream: StreamId,
    pub modules: Vec<Module>,
    #[serde(default)]
    pub data: Map<String, Value>,
    pub query: String,
    #[serde(default = "default_rego_version")]
    pub rego_version: RegoVersion,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "default_input")]
    pub input: Value,
    #[serde(default = "default_output_label")]
    pub output_label: String,
    #[serde(default = "default_output_properties")]
    pub output_properties: Map<String, Value>,
    #[serde(default)]
    pub copy_input_properties: bool,
    #[serde(default = "default_true")]
    pub provenance: bool,
    #[serde(default)]
    pub on_error: OnError,
    #[serde(default)]
    pub strict_builtin_errors: bool,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_check_interval")]
    pub timeout_check_interval: u32,
    #[serde(default = "default_max_input_bytes")]
    pub max_input_bytes: u64,
    #[serde(default = "default_max_decision_bytes")]
    pub max_decision_bytes: u64,
    #[serde(default = "default_max_retained_inputs")]
    pub max_retained_inputs: u64,
    #[serde(default = "default_max_policy_bytes")]
    pub max_policy_bytes: u64,
    #[serde(default = "default_max_data_bytes")]
    pub max_data_bytes: u64,
}

/// Compiled templates derived from a configuration.
pub(crate) struct Templates {
    pub(crate) input: Template,
    pub(crate) output: Vec<(String, Template)>,
}

impl PolicyConfig {
    /// The published field definition.
    pub(crate) fn definition() -> ConfigDefinition {
        use ConfigType::*;
        let bytes = |name: &str, default: u64, maximum: u64, description: &str| {
            ConfigOption::new(name, Integer, false, description)
                .default(default)
                .minimum(1)
                .maximum(maximum)
        };
        ConfigDefinition::try_new([
            ConfigOption::new(
                "stream",
                String,
                true,
                "Nonempty stream identifier for this transformer's own volatile decision output.",
            ),
            ConfigOption::new(
                "modules",
                Array,
                true,
                "Nonempty list of Rego modules, each {name, source}. Names are unique; modules are compiled at construction.",
            ),
            ConfigOption::new(
                "data",
                Object,
                false,
                "Static JSON object loaded as the Rego base `data` document.",
            )
            .default(json!({})),
            ConfigOption::new(
                "query",
                String,
                true,
                "Rule path evaluated for every input, e.g. data.example.decision. Must name a rule in the modules.",
            ),
            ConfigOption::new(
                "rego_version",
                String,
                false,
                "Rego language version of all modules.",
            )
            .default("v1")
            .allowed([json!("v1"), json!("v0")]),
            ConfigOption::new(
                "labels",
                Array,
                false,
                "Element labels to evaluate; elements with none of them are ignored. Empty evaluates every element.",
            )
            .default(json!([])),
            ConfigOption::new(
                "input",
                Json,
                false,
                "JSON template building the Rego input. `$` selects the element document (id, namespace, kind, labels, properties, in_node, out_node); `$$` escapes a literal `$`.",
            )
            .default(default_input()),
            ConfigOption::new(
                "output_label",
                String,
                false,
                "Label of every decision element.",
            )
            .default(default_output_label()),
            ConfigOption::new(
                "output_properties",
                Object,
                false,
                "Output property name to template over decision, input, element, status and error. Unresolved selectors omit the property.",
            )
            .default(Value::Object(default_output_properties())),
            ConfigOption::new(
                "copy_input_properties",
                Boolean,
                false,
                "Copy every input element property onto its decision before output_properties are applied.",
            )
            .default(false),
            ConfigOption::new(
                "provenance",
                Boolean,
                false,
                "Add policy_hash and policy_input_fingerprint to every decision.",
            )
            .default(true),
            ConfigOption::new(
                "on_error",
                String,
                false,
                "Per-record evaluation failure: emit an error decision, or fail the input batch.",
            )
            .default("emit")
            .allowed([json!("emit"), json!("fail")]),
            ConfigOption::new(
                "strict_builtin_errors",
                Boolean,
                false,
                "Builtin errors fail evaluation instead of making the expression undefined.",
            )
            .default(false),
            ConfigOption::new(
                "timeout_ms",
                Integer,
                false,
                "Wall-clock budget of one evaluation in milliseconds.",
            )
            .default(default_timeout_ms())
            .minimum(1)
            .maximum(60_000),
            ConfigOption::new(
                "timeout_check_interval",
                Integer,
                false,
                "Interpreter work units between timeout checks.",
            )
            .default(default_check_interval())
            .minimum(1)
            .maximum(1_000_000),
            bytes(
                "max_input_bytes",
                default_max_input_bytes(),
                64 * MIB,
                "Largest canonical JSON Rego input, in bytes.",
            ),
            bytes(
                "max_decision_bytes",
                default_max_decision_bytes(),
                64 * MIB,
                "Largest canonical JSON decision value, in bytes.",
            ),
            bytes(
                "max_retained_inputs",
                default_max_retained_inputs(),
                10_000_000,
                "Most input elements retained for re-evaluation; more fails the input batch.",
            ),
            bytes(
                "max_policy_bytes",
                default_max_policy_bytes(),
                16 * MIB,
                "Largest total size of all module sources, in bytes.",
            ),
            bytes(
                "max_data_bytes",
                default_max_data_bytes(),
                256 * MIB,
                "Largest canonical JSON data document, in bytes.",
            ),
        ])
        .expect("constant policy configuration")
    }

    /// Parses and validates everything except Rego compilation.
    pub(crate) fn parse(value: Value) -> Result<Self> {
        Self::definition().details().validate(&value)?;
        let config: Self =
            serde_json::from_value(value).context("invalid policy transformer configuration")?;
        config.templates()?;
        ensure!(
            !config.modules.is_empty(),
            "policy configuration needs at least one module"
        );
        let mut names = BTreeSet::new();
        let mut total = 0u64;
        for module in &config.modules {
            ensure!(
                !module.name.trim().is_empty(),
                "policy module names must be nonempty"
            );
            ensure!(
                names.insert(module.name.as_str()),
                "duplicate policy module name `{}`",
                module.name
            );
            total = total.saturating_add(module.source.len() as u64);
        }
        ensure!(
            total <= config.max_policy_bytes,
            "policy modules total {total} bytes, above max_policy_bytes {}",
            config.max_policy_bytes
        );
        ensure!(
            config.query.starts_with("data.") && config.query.len() > 5,
            "query must be a rule path starting with `data.`"
        );
        ensure!(
            !config.output_label.is_empty(),
            "output_label must be nonempty"
        );
        ensure!(
            config.labels.iter().all(|label| !label.is_empty()),
            "labels must be nonempty"
        );
        for name in config.output_properties.keys() {
            ensure!(!name.is_empty(), "output property names must be nonempty");
            ensure!(
                !RESERVED.contains(&name.as_str()),
                "output property `{name}` is reserved for the transformer"
            );
        }
        Ok(config)
    }

    pub(crate) fn templates(&self) -> Result<Templates> {
        Ok(Templates {
            input: Template::compile(&self.input, INPUT_ROOTS, true).context("input template")?,
            output: self
                .output_properties
                .iter()
                .map(|(name, value)| {
                    Ok((
                        name.clone(),
                        Template::compile(value, OUTPUT_ROOTS, false)
                            .with_context(|| format!("output property `{name}`"))?,
                    ))
                })
                .collect::<Result<_>>()?,
        })
    }

    /// Classifies a configuration change by the weakest safe boundary.
    ///
    /// * Replace: `stream` (the producer identity), `labels` (inputs outside
    ///   the old filter were never retained) and changed input bindings.
    /// * Pause (in place): everything else. Retained inputs are re-evaluated
    ///   and only changed decisions are emitted.
    pub(crate) fn plan(&self, proposed: &Self, inputs_changed: bool) -> Assessment {
        let mut planner = BoundaryPlanner::default();
        planner.field("inputs", inputs_changed, UpdateBoundary::Replace);
        planner.field(
            "stream",
            self.stream != proposed.stream,
            UpdateBoundary::Replace,
        );
        planner.field(
            "labels",
            self.labels != proposed.labels,
            UpdateBoundary::Replace,
        );
        planner.field("policy", self != proposed, UpdateBoundary::Pause);
        planner.assessment()
    }
}
