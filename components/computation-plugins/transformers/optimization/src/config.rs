// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optimization configuration: the declarative model, published schema,
//! defaults and structural validation.

use anyhow::{bail, ensure, Context, Result};
use drasi_computation_contracts::{ConfigurationConstraint, StreamId};
use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption, ConfigType};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const DEFAULT_STATUS_LABEL: &str = "OptimizationStatus";
pub(crate) const DEFAULT_SCENARIO_LABEL: &str = "OptimizationScenario";
/// Pseudo-properties that address a record's own identity.
pub(crate) const ID_PROPERTY: &str = "$id";
pub(crate) const NAMESPACE_PROPERTY: &str = "$namespace";
/// Output element ids owned by the status and scenario elements.
const RESERVED_IDS: [&str; 2] = ["status", "scenario"];

/// Configuration of `drasi.standard/optimization`. Unknown keys are rejected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OptimizationConfig {
    /// Stream identifier of this transformer's own volatile output.
    pub stream: StreamId,
    /// Namespace of emitted elements; the component id when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
    #[serde(default = "default_time_limit_ms")]
    pub time_limit_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario_time_limit_ms: Option<u64>,
    #[serde(default = "default_max_records")]
    pub max_records: u64,
    #[serde(default = "default_max_variables")]
    pub max_variables: u64,
    #[serde(default = "default_max_constraints")]
    pub max_constraints: u64,
    #[serde(default = "default_max_scenarios")]
    pub max_scenarios: u64,
    #[serde(default = "default_tolerance")]
    pub tolerance: f64,
    #[serde(default = "default_status_label")]
    pub status_label: String,
    #[serde(default = "default_scenario_label")]
    pub scenario_label: String,
    pub sets: Vec<SetDef>,
    pub variables: Vec<VariableDef>,
    #[serde(default)]
    pub constraints: Vec<ConstraintDef>,
    #[serde(default)]
    pub objectives: Vec<ObjectiveDef>,
    #[serde(default)]
    pub scenarios: Vec<ScenarioDef>,
    /// Copies a set's first member onto the status and scenario rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProvenanceDef>,
    /// Conditions on the inputs; when one does not hold the model is not
    /// solved and the status is `unknown`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<PreconditionDef>,
    /// Lists of solution rows copied onto the status row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summaries: Vec<SummaryDef>,
}

/// Bounds on the number of members of `set` that pass `where`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreconditionDef {
    pub name: String,
    pub set: String,
    #[serde(default, rename = "where", skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
}

/// The nonzero variables of one family (that pass `where`), as a list on the status row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SummaryDef {
    pub name: String,
    pub variable: String,
    #[serde(default, rename = "where", skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<Condition>,
    /// Member properties of each entry; the variable's output properties when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<Vec<String>>,
}

/// Identifies the inputs a result was solved from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProvenanceDef {
    pub set: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<Vec<String>>,
}

fn default_debounce_ms() -> u64 {
    100
}
fn default_max_delay_ms() -> u64 {
    1_000
}
fn default_time_limit_ms() -> u64 {
    2_000
}
fn default_max_records() -> u64 {
    100_000
}
fn default_max_variables() -> u64 {
    100_000
}
fn default_max_constraints() -> u64 {
    100_000
}
fn default_max_scenarios() -> u64 {
    64
}
fn default_tolerance() -> f64 {
    1e-6
}
fn default_status_label() -> String {
    DEFAULT_STATUS_LABEL.into()
}
fn default_scenario_label() -> String {
    DEFAULT_SCENARIO_LABEL.into()
}
fn default_scale() -> f64 {
    1.0
}
fn is_one(value: &f64) -> bool {
    *value == 1.0
}

/// An index set: input records, or an expansion of an earlier set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetDef {
    pub name: String,
    /// Records carrying this label. Records form only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Records in this namespace (the producing component or source). Records form only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Expand each member of this earlier set. Expansion form only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expand: Option<ExpandDef>,
    /// Member filter, evaluated on the (expanded) member.
    #[serde(default, rename = "where", skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<Condition>,
    /// Properties forming the member key; `[$namespace, $id]` when omitted.
    /// Records form only: expanded members are keyed by parent key and index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpandDef {
    /// Nonnegative integer property of the parent giving the member count.
    pub count: String,
    /// Property added to each expanded member holding its index `0..count`.
    pub index: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VariableType {
    Binary,
    Integer,
    Continuous,
}

/// A constant or a member property, multiplied by `scale`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Quantity {
    Constant(f64),
    Property(PropertyQuantity),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PropertyQuantity {
    pub property: String,
    #[serde(default = "default_scale", skip_serializing_if = "is_one")]
    pub scale: f64,
    /// Used when the property is missing or null; otherwise that is a model error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<f64>,
    /// Which record supplies the property in a term: the variable's `member`
    /// or the constraint's `group`. Ignored for bounds and right-hand sides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<Source>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    Member,
    Group,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VariableOutput {
    /// Emit solution rows for this family.
    #[serde(default = "default_true")]
    pub emit: bool,
    /// Label of solution rows; the variable name when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Member properties copied onto solution rows; all when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<Vec<String>>,
    /// Also emit rows whose value is zero.
    #[serde(default)]
    pub include_zero: bool,
}

fn default_true() -> bool {
    true
}

/// A variable family: one variable per member of `over`, or a single scalar.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VariableDef {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: VariableType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub over: Option<String>,
    #[serde(default, rename = "where", skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<Condition>,
    /// Lower bound; 0 when omitted. Not allowed for binary variables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lower: Option<Quantity>,
    /// Upper bound; unbounded when omitted. Not allowed for binary variables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper: Option<Quantity>,
    #[serde(default)]
    pub output: VariableOutput,
}

/// One linear term: a coefficient times each matching variable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TermDef {
    pub variable: String,
    /// Variable-member property to group property; all must be equal.
    #[serde(default, rename = "match", skip_serializing_if = "BTreeMap::is_empty")]
    pub matches: BTreeMap<String, String>,
    #[serde(default, rename = "where", skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coefficient: Option<Quantity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelOp {
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "==")]
    Eq,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConstraintDef {
    pub name: String,
    /// One constraint per member of this set (the group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_each: Option<String>,
    /// One constraint per distinct value tuple of these variable-member properties.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<Vec<String>>,
    /// Filters `forEach` members.
    #[serde(default, rename = "where", skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<Condition>,
    pub terms: Vec<TermDef>,
    pub op: RelOp,
    /// Right-hand side: a constant or a group property.
    pub rhs: Quantity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Sense {
    Minimize,
    Maximize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectiveDef {
    pub name: String,
    pub sense: Sense,
    pub terms: Vec<TermDef>,
}

/// One what-if analysis per distinct value of `set.property`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScenarioDef {
    pub name: String,
    pub set: String,
    pub property: String,
    /// Sets whose members with `removeProperty == value` are removed; `[set]` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_from: Option<Vec<String>>,
    /// Property compared in `removeFrom` sets; `property` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_property: Option<String>,
    /// Optimize the objectives too, rather than checking feasibility only.
    #[serde(default)]
    pub optimize: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareOp {
    #[serde(rename = "==")]
    Eq,
    #[serde(rename = "!=")]
    Ne,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "in")]
    In,
    #[serde(rename = "notIn")]
    NotIn,
    #[serde(rename = "exists")]
    Exists,
    #[serde(rename = "notExists")]
    NotExists,
}

/// A member predicate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Condition {
    Compare(Compare),
    InSet(InSet),
    NotInSet(NotInSet),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Compare {
    pub property: String,
    pub op: CompareOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// Compare with another property of the same member instead of `value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_property: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InSet {
    pub in_set: String,
    /// This member's property to the other set member's property.
    #[serde(rename = "match")]
    pub matches: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotInSet {
    pub not_in_set: String,
    #[serde(rename = "match")]
    pub matches: BTreeMap<String, String>,
}

impl OptimizationConfig {
    pub fn parse(value: Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    /// The configuration with every default made explicit, as exposed by `configuration()`.
    pub fn normalized(&self) -> Result<Value> {
        Ok(serde_json::to_value(self)?)
    }

    pub fn scenario_time_limit_ms(&self) -> u64 {
        self.scenario_time_limit_ms.unwrap_or(self.time_limit_ms)
    }

    fn validate(&self) -> Result<()> {
        if let Some(namespace) = &self.namespace {
            ensure!(!namespace.is_empty(), "namespace must be nonempty");
        }
        ensure!(
            self.max_delay_ms >= self.debounce_ms,
            "maxDelayMs must be at least debounceMs"
        );
        ensure!(self.time_limit_ms > 0, "timeLimitMs must be positive");
        ensure!(
            self.scenario_time_limit_ms() > 0,
            "scenarioTimeLimitMs must be positive"
        );
        ensure!(
            self.tolerance.is_finite() && self.tolerance >= 0.0,
            "tolerance must be a nonnegative number"
        );
        ensure!(
            !self.status_label.is_empty() && !self.scenario_label.is_empty(),
            "labels must be nonempty"
        );
        let mut sets = BTreeSet::new();
        for set in &self.sets {
            validate_name("set", &set.name)?;
            match (&set.from, &set.expand) {
                (None, None) => {
                    ensure!(
                        set.label.is_some() || set.namespace.is_some(),
                        "set {} must select records by label or namespace, or expand an earlier set",
                        set.name
                    );
                    if let Some(key) = &set.key {
                        ensure!(!key.is_empty(), "set {} key must be nonempty", set.name);
                    }
                }
                (Some(from), Some(expand)) => {
                    ensure!(
                        set.label.is_none() && set.namespace.is_none() && set.key.is_none(),
                        "expanded set {} cannot select records or declare a key",
                        set.name
                    );
                    ensure!(
                        sets.contains(from.as_str()),
                        "set {} expands {from}, which is not an earlier set",
                        set.name
                    );
                    ensure!(
                        !expand.count.is_empty() && !expand.index.is_empty(),
                        "set {} expansion needs count and index properties",
                        set.name
                    );
                }
                _ => bail!(
                    "set {} must declare both from and expand, or neither",
                    set.name
                ),
            }
            // A set's conditions may refer to earlier sets only.
            validate_conditions(&set.filter, &sets, &format!("set {}", set.name))?;
            ensure!(sets.insert(set.name.as_str()), "duplicate set {}", set.name);
        }
        ensure!(
            !self.variables.is_empty(),
            "at least one variable is required"
        );
        let mut variables = BTreeMap::new();
        for variable in &self.variables {
            validate_name("variable", &variable.name)?;
            ensure!(
                !RESERVED_IDS.contains(&variable.name.as_str()),
                "variable name {} is reserved for status and scenario elements",
                variable.name
            );
            let context = format!("variable {}", variable.name);
            match &variable.over {
                Some(over) => ensure!(
                    sets.contains(over.as_str()),
                    "{context} is over unknown set {over}"
                ),
                None => {
                    ensure!(
                        variable.filter.is_empty(),
                        "{context} is scalar and cannot filter members"
                    );
                    for bound in [&variable.lower, &variable.upper].into_iter().flatten() {
                        ensure!(
                            matches!(bound, Quantity::Constant(_)),
                            "{context} is scalar; its bounds must be constants"
                        );
                    }
                }
            }
            if variable.kind == VariableType::Binary {
                ensure!(
                    variable.lower.is_none() && variable.upper.is_none(),
                    "{context} is binary and cannot declare bounds"
                );
            }
            for bound in [&variable.lower, &variable.upper].into_iter().flatten() {
                validate_quantity(bound, &context)?;
            }
            if let Some(label) = &variable.output.label {
                ensure!(!label.is_empty(), "{context} output label must be nonempty");
            }
            validate_conditions(&variable.filter, &sets, &context)?;
            ensure!(
                variables
                    .insert(variable.name.as_str(), variable.over.is_some())
                    .is_none(),
                "duplicate variable {}",
                variable.name
            );
        }
        let mut names = BTreeSet::new();
        for constraint in &self.constraints {
            validate_name("constraint", &constraint.name)?;
            let context = format!("constraint {}", constraint.name);
            ensure!(
                names.insert(constraint.name.as_str()),
                "duplicate constraint {}",
                constraint.name
            );
            ensure!(
                !(constraint.for_each.is_some() && constraint.group_by.is_some()),
                "{context} cannot declare both forEach and groupBy"
            );
            let grouped = constraint.for_each.is_some() || constraint.group_by.is_some();
            if let Some(for_each) = &constraint.for_each {
                ensure!(
                    sets.contains(for_each.as_str()),
                    "{context} iterates unknown set {for_each}"
                );
            } else {
                ensure!(
                    constraint.filter.is_empty(),
                    "{context} can filter only forEach members; filter terms instead"
                );
            }
            if let Some(group_by) = &constraint.group_by {
                ensure!(!group_by.is_empty(), "{context} groupBy must be nonempty");
            }
            ensure!(!constraint.terms.is_empty(), "{context} needs terms");
            validate_conditions(&constraint.filter, &sets, &context)?;
            validate_quantity(&constraint.rhs, &context)?;
            if !grouped {
                ensure!(
                    matches!(constraint.rhs, Quantity::Constant(_)),
                    "{context} has no group; its rhs must be a constant"
                );
            }
            for term in &constraint.terms {
                validate_term(term, &variables, &sets, grouped, &context)?;
                if constraint.group_by.is_some() {
                    ensure!(
                        term.matches.is_empty(),
                        "{context} groups by member properties; terms cannot match"
                    );
                }
            }
        }
        let mut objectives = BTreeSet::new();
        for objective in &self.objectives {
            validate_name("objective", &objective.name)?;
            ensure!(
                objectives.insert(objective.name.as_str()),
                "duplicate objective {}",
                objective.name
            );
            ensure!(
                !objective.terms.is_empty(),
                "objective {} needs terms",
                objective.name
            );
            for term in &objective.terms {
                validate_term(
                    term,
                    &variables,
                    &sets,
                    false,
                    &format!("objective {}", objective.name),
                )?;
            }
        }
        let mut scenarios = BTreeSet::new();
        for scenario in &self.scenarios {
            validate_name("scenario", &scenario.name)?;
            ensure!(
                scenarios.insert(scenario.name.as_str()),
                "duplicate scenario {}",
                scenario.name
            );
            ensure!(
                sets.contains(scenario.set.as_str()),
                "scenario {} uses unknown set {}",
                scenario.name,
                scenario.set
            );
            ensure!(
                !scenario.property.is_empty(),
                "scenario {} needs a property",
                scenario.name
            );
            for set in scenario.remove_from.iter().flatten() {
                ensure!(
                    sets.contains(set.as_str()),
                    "scenario {} removes from unknown set {set}",
                    scenario.name
                );
            }
            if let Some(remove_from) = &scenario.remove_from {
                ensure!(
                    !remove_from.is_empty(),
                    "scenario {} removeFrom must be nonempty",
                    scenario.name
                );
            }
        }
        if let Some(provenance) = &self.provenance {
            ensure!(
                sets.contains(provenance.set.as_str()),
                "provenance uses unknown set {}",
                provenance.set
            );
        }
        let mut preconditions = BTreeSet::new();
        for precondition in &self.preconditions {
            validate_name("precondition", &precondition.name)?;
            let context = format!("precondition {}", precondition.name);
            ensure!(
                preconditions.insert(precondition.name.as_str()),
                "duplicate precondition {}",
                precondition.name
            );
            ensure!(
                sets.contains(precondition.set.as_str()),
                "{context} uses unknown set {}",
                precondition.set
            );
            ensure!(
                precondition.min.is_some() || precondition.max.is_some(),
                "{context} needs min, max or both"
            );
            if let (Some(min), Some(max)) = (precondition.min, precondition.max) {
                ensure!(min <= max, "{context} min must not exceed max");
            }
            validate_conditions(&precondition.filter, &sets, &context)?;
        }
        let mut summaries = BTreeSet::new();
        for summary in &self.summaries {
            validate_name("summary", &summary.name)?;
            let context = format!("summary {}", summary.name);
            ensure!(
                summaries.insert(summary.name.as_str()),
                "duplicate summary {}",
                summary.name
            );
            let indexed = *variables
                .get(summary.variable.as_str())
                .with_context(|| format!("{context} uses unknown variable {}", summary.variable))?;
            ensure!(
                indexed || (summary.filter.is_empty() && summary.properties.is_none()),
                "{context}: scalar variable {} has no member to filter or copy",
                summary.variable
            );
            validate_conditions(&summary.filter, &sets, &context)?;
        }
        Ok(())
    }

    pub fn definition() -> ConfigDefinition {
        use ConfigType::{Array, Integer, Json, String};
        let array = || constraint(json!({"type": "array", "items": {"type": "object"}}));
        ConfigDefinition::try_new([
            ConfigOption::new(
                "stream",
                String,
                true,
                "Nonempty stream identifier for this transformer's own volatile output.",
            ),
            ConfigOption::new(
                "namespace",
                String,
                false,
                "Namespace of emitted solution, status and scenario elements.",
            )
            .default_description("the component id"),
            ConfigOption::new(
                "debounceMs",
                Integer,
                false,
                "Quiet period after the last input change before a re-solve starts.",
            )
            .minimum(0)
            .default(default_debounce_ms()),
            ConfigOption::new(
                "maxDelayMs",
                Integer,
                false,
                "Longest a pending change waits for a re-solve under continuous input; at least debounceMs.",
            )
            .minimum(0)
            .default(default_max_delay_ms()),
            ConfigOption::new(
                "timeLimitMs",
                Integer,
                false,
                "Solver time budget shared by all objective phases of one solve. Exhausting it reports status 'limit'.",
            )
            .minimum(1)
            .default(default_time_limit_ms()),
            ConfigOption::new(
                "scenarioTimeLimitMs",
                Integer,
                false,
                "Solver time budget of each scenario.",
            )
            .minimum(1)
            .default_description("timeLimitMs"),
            ConfigOption::new(
                "maxRecords",
                Integer,
                false,
                "Largest number of retained input records; more fails the input.",
            )
            .minimum(1)
            .default(default_max_records()),
            ConfigOption::new(
                "maxVariables",
                Integer,
                false,
                "Largest model in variables; larger models report status 'limit' without solving.",
            )
            .minimum(1)
            .default(default_max_variables()),
            ConfigOption::new(
                "maxConstraints",
                Integer,
                false,
                "Largest model in constraints; larger models report status 'limit' without solving.",
            )
            .minimum(1)
            .default(default_max_constraints()),
            ConfigOption::new(
                "maxScenarios",
                Integer,
                false,
                "Largest number of scenario solves per model; the rest report status 'limit'.",
            )
            .minimum(0)
            .default(default_max_scenarios()),
            ConfigOption::new(
                "tolerance",
                Json,
                false,
                "Relative slack when fixing an earlier objective at its optimum in later lexicographic phases.",
            )
            .constraint(constraint(json!({"type": "number", "minimum": 0})))
            .default(default_tolerance()),
            ConfigOption::new(
                "statusLabel",
                String,
                false,
                "Label of the status element.",
            )
            .constraint(constraint(json!({"type": "string", "minLength": 1})))
            .default(DEFAULT_STATUS_LABEL),
            ConfigOption::new(
                "scenarioLabel",
                String,
                false,
                "Label of scenario elements.",
            )
            .constraint(constraint(json!({"type": "string", "minLength": 1})))
            .default(DEFAULT_SCENARIO_LABEL),
            ConfigOption::new(
                "sets",
                Array,
                true,
                "Ordered index sets: {name, label?, namespace?, where?, key?} selects input records; {name, from, expand: {count, index}, where?} expands an earlier set.",
            )
            .constraint(array()),
            ConfigOption::new(
                "variables",
                Array,
                true,
                "Variable families {name, type: binary|integer|continuous, over?, where?, lower?, upper?, output?}. Without over, a single scalar variable.",
            )
            .constraint(array()),
            ConfigOption::new(
                "constraints",
                Array,
                false,
                "Linear constraints {name, forEach? | groupBy?, where?, terms, op: <=|>=|==, rhs}.",
            )
            .constraint(array())
            .default(json!([])),
            ConfigOption::new(
                "objectives",
                Array,
                false,
                "Lexicographic objectives {name, sense: minimize|maximize, terms}. Empty solves for feasibility only.",
            )
            .constraint(array())
            .default(json!([])),
            ConfigOption::new(
                "scenarios",
                Array,
                false,
                "What-if analyses {name, set, property, removeFrom?, removeProperty?, optimize?}: one re-solve per distinct value with matching members removed.",
            )
            .constraint(array())
            .default(json!([])),
            ConfigOption::new(
                "provenance",
                Json,
                false,
                "{set, properties?}: copies the set's first member (all non-$ properties, or the listed ones) onto the status and scenario elements as provenance.",
            )
            .constraint(constraint(json!({"type": "object"}))),
            ConfigOption::new(
                "preconditions",
                Array,
                false,
                "Input checks {name, set, where?, min?, max?} on the number of set members passing where. If one fails the model is not solved: status 'unknown', no solution or scenario rows.",
            )
            .constraint(array())
            .default(json!([])),
            ConfigOption::new(
                "summaries",
                Array,
                false,
                "Solution lists {name, variable, where?, properties?} copied onto the status element under summaries.<name> while optimal.",
            )
            .constraint(array())
            .default(json!([])),
        ])
        .unwrap_or_else(|error| unreachable!("constant optimization configuration: {error}"))
    }
}

fn constraint(schema: Value) -> ConfigurationConstraint {
    ConfigurationConstraint::try_new(schema)
        .unwrap_or_else(|error| unreachable!("constant optimization schema: {error}"))
}

fn validate_name(kind: &str, name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.chars().any(|c| c == '/' || c.is_control()),
        "{kind} name {name:?} must be nonempty without '/' or control characters"
    );
    Ok(())
}

fn validate_quantity(quantity: &Quantity, context: &str) -> Result<()> {
    match quantity {
        Quantity::Constant(value) => {
            ensure!(value.is_finite(), "{context} has a non-finite constant")
        }
        Quantity::Property(property) => {
            ensure!(
                !property.property.is_empty(),
                "{context} has an empty property"
            );
            ensure!(
                property.scale.is_finite() && property.default.is_none_or(f64::is_finite),
                "{context} has a non-finite scale or default"
            );
        }
    }
    Ok(())
}

fn validate_term(
    term: &TermDef,
    variables: &BTreeMap<&str, bool>,
    sets: &BTreeSet<&str>,
    grouped: bool,
    context: &str,
) -> Result<()> {
    let indexed = *variables
        .get(term.variable.as_str())
        .with_context(|| format!("{context} uses unknown variable {}", term.variable))?;
    if !indexed {
        ensure!(
            term.matches.is_empty() && term.filter.is_empty(),
            "{context}: scalar variable {} cannot match or filter",
            term.variable
        );
    }
    if !grouped {
        ensure!(
            term.matches.is_empty(),
            "{context} has no group; its terms cannot match"
        );
    }
    if let Some(coefficient) = &term.coefficient {
        validate_quantity(coefficient, context)?;
        if let Quantity::Property(property) = coefficient {
            let of = property.of.unwrap_or(if indexed {
                Source::Member
            } else {
                Source::Group
            });
            ensure!(
                of == Source::Member || grouped,
                "{context} has no group to supply coefficient {}",
                property.property
            );
            ensure!(
                of == Source::Group || indexed,
                "{context}: scalar variable {} has no member to supply coefficient {}",
                term.variable,
                property.property
            );
        }
    }
    validate_conditions(&term.filter, sets, context)
}

fn validate_conditions(
    conditions: &[Condition],
    sets: &BTreeSet<&str>,
    context: &str,
) -> Result<()> {
    for condition in conditions {
        match condition {
            Condition::Compare(compare) => {
                ensure!(
                    !compare.property.is_empty(),
                    "{context} compares an empty property"
                );
                let unary = matches!(compare.op, CompareOp::Exists | CompareOp::NotExists);
                let operands = usize::from(compare.value.is_some())
                    + usize::from(compare.value_property.is_some());
                ensure!(
                    if unary { operands == 0 } else { operands == 1 },
                    "{context}: {:?} on {} needs {}",
                    compare.op,
                    compare.property,
                    if unary {
                        "no value"
                    } else {
                        "exactly one of value or valueProperty"
                    }
                );
                if matches!(compare.op, CompareOp::In | CompareOp::NotIn) {
                    ensure!(
                        compare.value.as_ref().is_some_and(Value::is_array),
                        "{context}: in/notIn on {} needs an array value",
                        compare.property
                    );
                }
            }
            Condition::InSet(InSet {
                in_set: set,
                matches,
            })
            | Condition::NotInSet(NotInSet {
                not_in_set: set,
                matches,
            }) => {
                ensure!(
                    sets.contains(set.as_str()),
                    "{context} refers to {set}, which is not an earlier set"
                );
                ensure!(
                    !matches.is_empty(),
                    "{context}: membership in {set} needs match properties"
                );
            }
        }
    }
    Ok(())
}
