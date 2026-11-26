// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Geometry transformer configuration, validation and update planning.

use anyhow::{bail, ensure, Context, Result};
use drasi_computation_contracts::StreamId;
use drasi_computation_plugin_sdk::{transformer, ConfigDefinition, ConfigOption, ConfigType};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const DEFAULT_EPSILON: f64 = 1e-7;
pub(crate) const DEFAULT_ERROR_LABEL: &str = "GeometryError";
const DEFAULT_MAX_ELEMENTS: usize = 10_000;
const DEFAULT_MAX_VERTICES: usize = 4_096;
const DEFAULT_MAX_OUTPUTS: usize = 100_000;
const LIMIT_CEILING: usize = 10_000_000;

/// The complete, normalized configuration of one geometry transformer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GeometryConfig {
    pub stream: StreamId,
    pub sets: Vec<SetConfig>,
    pub relations: Vec<RelationConfig>,
    #[serde(default = "default_epsilon")]
    pub epsilon: f64,
    #[serde(default)]
    pub ring_closure: RingClosure,
    #[serde(default)]
    pub invalid_geometry: InvalidGeometry,
    #[serde(default = "default_error_label")]
    pub error_label: String,
    #[serde(default)]
    pub limits: Limits,
}

/// A named role: the input elements carrying `label` that match `where`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SetConfig {
    pub name: String,
    pub label: String,
    #[serde(default, rename = "where", skip_serializing_if = "BTreeMap::is_empty")]
    pub filter: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub geometry: GeometrySource,
}

/// Where and how a member's geometry is read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum GeometrySource {
    /// `property` holds `[x, y]` or `{x, y}`, or `x` and `y` name two numeric properties.
    Point {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        property: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<String>,
    },
    /// An array of at least two coordinates.
    LineString { property: String },
    /// One ring of coordinates, or an array of rings (exterior first, then holes).
    Polygon { property: String },
    /// A GeoJSON geometry or Feature object (or its JSON text).
    GeoJson { property: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Predicate {
    WithinDistance,
    Intersects,
    Contains,
    Within,
}

/// The withinDistance threshold: `constant` plus the sums of the named
/// numeric properties of the left and right members.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Threshold {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub constant: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub left: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub right: Vec<String>,
}

/// Names of the generated properties of each relation record; `null` omits one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OutputProperties {
    #[serde(default = "default_id_property")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<String>,
}

impl Default for OutputProperties {
    fn default() -> Self {
        Self {
            id: default_id_property(),
            relation: None,
            left_key: None,
            right_key: None,
            distance: None,
            threshold: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RelationConfig {
    pub name: String,
    pub left: String,
    pub right: String,
    pub predicate: Predicate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<Threshold>,
    pub label: String,
    #[serde(default, skip_serializing_if = "OutputProperties::is_default")]
    pub properties: OutputProperties,
    /// Output property name to left member property name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub copy_left: BTreeMap<String, String>,
    /// Output property name to right member property name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub copy_right: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RingClosure {
    /// Rings may be open or repeat their first coordinate.
    #[default]
    Auto,
    /// Rings must repeat their first coordinate (GeoJSON style).
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum InvalidGeometry {
    /// Reject the whole input; nothing is retained or emitted.
    #[default]
    Fail,
    /// Exclude the record from its set, retracting its relations.
    Skip,
    /// Exclude the record and emit an error record describing it.
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Limits {
    #[serde(default = "default_max_elements")]
    pub max_elements: usize,
    #[serde(default = "default_max_vertices")]
    pub max_vertices: usize,
    #[serde(default = "default_max_outputs")]
    pub max_outputs: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_elements: DEFAULT_MAX_ELEMENTS,
            max_vertices: DEFAULT_MAX_VERTICES,
            max_outputs: DEFAULT_MAX_OUTPUTS,
        }
    }
}

fn default_epsilon() -> f64 {
    DEFAULT_EPSILON
}

fn default_error_label() -> String {
    DEFAULT_ERROR_LABEL.into()
}

fn default_id_property() -> Option<String> {
    Some("id".into())
}

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

impl OutputProperties {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn default_max_elements() -> usize {
    DEFAULT_MAX_ELEMENTS
}

fn default_max_vertices() -> usize {
    DEFAULT_MAX_VERTICES
}

fn default_max_outputs() -> usize {
    DEFAULT_MAX_OUTPUTS
}

impl GeometryConfig {
    /// The geometry settings of a configuration, without the shared runtime
    /// options.
    pub(crate) fn parse(value: &serde_json::Value) -> Result<Self> {
        let mut fields = value
            .as_object()
            .context("invalid geometry configuration: expected an object")?
            .clone();
        for (name, _, _) in transformer::configuration_fields() {
            fields.remove(name);
        }
        let config: Self = serde_json::from_value(serde_json::Value::Object(fields))
            .context("invalid geometry configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn set_index(&self, name: &str) -> Option<usize> {
        self.sets.iter().position(|set| set.name == name)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.epsilon.is_finite() && self.epsilon >= 0.0,
            "epsilon must be a finite, non-negative number"
        );
        ensure!(!self.error_label.is_empty(), "errorLabel must not be empty");
        for (name, value) in [
            ("maxElements", self.limits.max_elements),
            ("maxVertices", self.limits.max_vertices),
            ("maxOutputs", self.limits.max_outputs),
        ] {
            ensure!(
                (1..=LIMIT_CEILING).contains(&value),
                "limits.{name} must be between 1 and {LIMIT_CEILING}"
            );
        }
        ensure!(!self.sets.is_empty(), "at least one set is required");
        ensure!(
            !self.relations.is_empty(),
            "at least one relation is required"
        );
        let mut names = BTreeSet::new();
        for set in &self.sets {
            ensure!(!set.name.is_empty(), "set names must not be empty");
            ensure!(
                names.insert(set.name.as_str()),
                "duplicate set name '{}'",
                set.name
            );
            set.validate()
                .with_context(|| format!("set '{}'", set.name))?;
        }
        let mut relations = BTreeSet::new();
        let mut used = BTreeSet::new();
        for relation in &self.relations {
            ensure!(
                !relation.name.is_empty(),
                "relation names must not be empty"
            );
            ensure!(
                relations.insert(relation.name.as_str()),
                "duplicate relation name '{}'",
                relation.name
            );
            relation
                .validate(self)
                .with_context(|| format!("relation '{}'", relation.name))?;
            used.insert(relation.left.as_str());
            used.insert(relation.right.as_str());
        }
        if let Some(unused) = self
            .sets
            .iter()
            .find(|set| !used.contains(set.name.as_str()))
        {
            bail!("set '{}' is not used by any relation", unused.name);
        }
        Ok(())
    }
}

impl SetConfig {
    fn validate(&self) -> Result<()> {
        ensure!(!self.label.is_empty(), "label must not be empty");
        ensure!(
            self.filter.keys().all(|name| !name.is_empty()),
            "where property names must not be empty"
        );
        ensure!(
            self.key.as_deref().is_none_or(|key| !key.is_empty()),
            "key must not be empty"
        );
        match &self.geometry {
            GeometrySource::Point { property, x, y } => match (property, x, y) {
                (Some(property), None, None) => {
                    ensure!(!property.is_empty(), "point property must not be empty")
                }
                (None, Some(x), Some(y)) => ensure!(
                    !x.is_empty() && !y.is_empty() && x != y,
                    "point x and y must name two different properties"
                ),
                _ => bail!("a point geometry needs either `property` or both `x` and `y`"),
            },
            GeometrySource::LineString { property }
            | GeometrySource::Polygon { property }
            | GeometrySource::GeoJson { property } => {
                ensure!(!property.is_empty(), "geometry property must not be empty")
            }
        }
        Ok(())
    }
}

impl RelationConfig {
    fn validate(&self, config: &GeometryConfig) -> Result<()> {
        ensure!(
            config.set_index(&self.left).is_some(),
            "unknown left set '{}'",
            self.left
        );
        ensure!(
            config.set_index(&self.right).is_some(),
            "unknown right set '{}'",
            self.right
        );
        ensure!(!self.label.is_empty(), "label must not be empty");
        ensure!(
            config.invalid_geometry != InvalidGeometry::Report || self.label != config.error_label,
            "label must differ from errorLabel"
        );
        match (&self.predicate, &self.threshold) {
            (Predicate::WithinDistance, Some(threshold)) => {
                ensure!(
                    threshold.constant.is_finite() && threshold.constant >= 0.0,
                    "threshold.constant must be a finite, non-negative number"
                );
                ensure!(
                    threshold
                        .left
                        .iter()
                        .chain(&threshold.right)
                        .all(|name| !name.is_empty()),
                    "threshold property names must not be empty"
                );
            }
            (Predicate::WithinDistance, None) => bail!("withinDistance requires a threshold"),
            (_, Some(_)) => bail!("only withinDistance accepts a threshold"),
            (_, None) => {}
        }
        ensure!(
            self.predicate == Predicate::WithinDistance || self.properties.threshold.is_none(),
            "only withinDistance can emit a threshold property"
        );
        let generated = [
            &self.properties.id,
            &self.properties.relation,
            &self.properties.left_key,
            &self.properties.right_key,
            &self.properties.distance,
            &self.properties.threshold,
        ];
        let mut outputs = BTreeSet::new();
        for name in generated
            .into_iter()
            .flatten()
            .chain(self.copy_left.keys())
            .chain(self.copy_right.keys())
        {
            ensure!(!name.is_empty(), "output property names must not be empty");
            ensure!(
                outputs.insert(name.as_str()),
                "output property '{name}' is produced more than once"
            );
        }
        ensure!(
            self.copy_left
                .values()
                .chain(self.copy_right.values())
                .all(|name| !name.is_empty()),
            "copied property names must not be empty"
        );
        Ok(())
    }
}

/// Whether a configuration change can apply in place, keeping the state.
///
/// * Refused: the output `stream`; consumers bind to it.
/// * In place: `limits` (checked against the retained state) and switching
///   `invalidGeometry` between `fail` and `skip`. Neither changes an already
///   emitted record.
/// * Reconstruction: input bindings and everything that changes which records
///   exist (sets, relations, epsilon, ring closure, `report`, the error
///   label). A transformer cannot emit the resulting difference without its
///   input, so it is rebuilt from that input instead.
pub(crate) fn in_place(
    current: &GeometryConfig,
    proposed: &GeometryConfig,
    inputs_changed: bool,
) -> Result<bool> {
    ensure!(
        current.stream == proposed.stream,
        "geometry updates cannot change the output stream"
    );
    Ok(!inputs_changed && semantic(current) == semantic(proposed))
}

/// The settings that decide which records exist: everything an in-place
/// update cannot change.
fn semantic(config: &GeometryConfig) -> GeometryConfig {
    let mut config = config.clone();
    config.limits = Limits::default();
    if config.invalid_geometry != InvalidGeometry::Report {
        config.invalid_geometry = InvalidGeometry::Fail;
    }
    config
}

/// The configuration identity retained state is reused with.
pub(crate) fn identity(config: &GeometryConfig) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(semantic(config))?)
}

pub(crate) fn definition() -> ConfigDefinition {
    ConfigDefinition::try_new([
        ConfigOption::new(
            "stream",
            ConfigType::String,
            true,
            "Nonempty stream identifier for this transformer's output.",
        ),
        ConfigOption::new(
            "sets",
            ConfigType::Array,
            true,
            "Named roles: {name, label, where?, key?, geometry: {type: point|lineString|polygon|geoJson, property | x+y}}.",
        ),
        ConfigOption::new(
            "relations",
            ConfigType::Array,
            true,
            "Spatial relations between two sets: {name, left, right, predicate: withinDistance|intersects|contains|within, threshold?, label, properties?, copyLeft?, copyRight?}.",
        ),
        ConfigOption::new(
            "epsilon",
            ConfigType::Json,
            false,
            "Non-negative tolerance for distance comparisons, zero-length edges and ring closure.",
        )
        .default(DEFAULT_EPSILON),
        ConfigOption::new(
            "ringClosure",
            ConfigType::String,
            false,
            "Whether polygon rings must repeat their first coordinate.",
        )
        .allowed(["auto".into(), "required".into()])
        .default("auto"),
        ConfigOption::new(
            "invalidGeometry",
            ConfigType::String,
            false,
            "Per-record behaviour for invalid geometry, keys or threshold properties.",
        )
        .allowed(["fail".into(), "skip".into(), "report".into()])
        .default("fail"),
        ConfigOption::new(
            "errorLabel",
            ConfigType::String,
            false,
            "Label of the error records emitted under invalidGeometry: report.",
        )
        .default(DEFAULT_ERROR_LABEL),
        ConfigOption::new(
            "limits",
            ConfigType::Object,
            false,
            "Resource bounds: {maxElements, maxVertices, maxOutputs}.",
        )
        .default(serde_json::json!({
            "maxElements": DEFAULT_MAX_ELEMENTS,
            "maxVertices": DEFAULT_MAX_VERTICES,
            "maxOutputs": DEFAULT_MAX_OUTPUTS,
        })),
    ]
    .into_iter()
    .chain(transformer::configuration_options()))
    .expect("constant geometry configuration")
}
