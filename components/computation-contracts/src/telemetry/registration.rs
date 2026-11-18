// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    metric_definition, valid_category, MetricKind, MetricUnit, Result, TelemetryError,
    TelemetryScope, COMPONENT_LABELS, GRAPH_LABELS, RESERVED_LABELS,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

#[derive(Debug, Clone, Copy)]
pub struct RegistrationLimits {
    pub name_bytes: usize,
    pub description_bytes: usize,
    pub labels: usize,
    pub label_key_bytes: usize,
    pub label_value_bytes: usize,
    pub definitions: usize,
    pub series: usize,
}

impl Default for RegistrationLimits {
    fn default() -> Self {
        Self {
            name_bytes: 256,
            description_bytes: 1024,
            labels: 16,
            label_key_bytes: 64,
            label_value_bytes: 1024,
            definitions: 1024,
            series: 16_384,
        }
    }
}

impl RegistrationLimits {
    pub fn validate(&self) -> Result<()> {
        if [
            self.name_bytes,
            self.description_bytes,
            self.labels,
            self.label_key_bytes,
            self.label_value_bytes,
            self.definitions,
            self.series,
        ]
        .contains(&0)
        {
            return Err(TelemetryError::Invalid(
                "registration limits must be positive".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct MetricRegistration {
    name: String,
    kind: MetricKind,
    unit: MetricUnit,
    description: String,
    labels: Vec<(String, String)>,
    framework: bool,
}

impl MetricRegistration {
    pub fn framework(
        name: &str,
        scope: &TelemetryScope,
        additional: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self> {
        let definition = metric_definition(name)
            .ok_or_else(|| TelemetryError::Invalid(format!("unknown framework metric '{name}'")))?;
        if definition.component && scope.component_id().is_none() {
            return Err(TelemetryError::Invalid(format!(
                "'{name}' requires component identity"
            )));
        }
        let mut labels = scope.labels();
        if !definition.component {
            labels.retain(|(key, _)| GRAPH_LABELS.contains(&key.as_str()));
        }
        let additional: Vec<_> = additional.into_iter().collect();
        for (key, value) in &additional {
            if !definition.labels.contains(&key.as_str()) || !valid_category(key, value) {
                return Err(TelemetryError::Invalid(format!(
                    "invalid label '{key}' for '{name}'"
                )));
            }
        }
        labels.extend(additional);
        let registration = Self::canonical(
            name.into(),
            definition.kind,
            definition.unit,
            definition.semantics.into(),
            labels,
            true,
        )?;
        if definition.labels.len()
            + if definition.component {
                COMPONENT_LABELS.len()
            } else {
                GRAPH_LABELS.len()
            }
            != registration.labels.len()
        {
            return Err(TelemetryError::Invalid(format!(
                "missing labels for '{name}'"
            )));
        }
        Ok(registration)
    }

    pub fn plugin_name(plugin_kind: &str, leaf: &str) -> Result<String> {
        if !valid_word(plugin_kind, false) || !valid_word(leaf, true) || leaf.starts_with("drasi.")
        {
            return Err(TelemetryError::Invalid(
                "invalid plugin namespace or metric leaf".into(),
            ));
        }
        Ok(format!("drasi.plugin.{plugin_kind}.{leaf}"))
    }

    pub fn plugin(
        scope: &TelemetryScope,
        plugin_kind: &str,
        leaf: &str,
        kind: MetricKind,
        unit: MetricUnit,
        description: impl Into<String>,
        additional: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self> {
        if scope.component_id().is_none() {
            return Err(TelemetryError::Invalid(
                "plugin metrics require component identity".into(),
            ));
        }
        let name = Self::plugin_name(plugin_kind, leaf)?;
        let mut labels = scope.labels();
        labels.push(("plugin_kind".into(), plugin_kind.into()));
        for (key, value) in additional {
            if RESERVED_LABELS.contains(&key.as_str()) {
                return Err(TelemetryError::Invalid(format!(
                    "reserved metric label '{key}'"
                )));
            }
            labels.push((key, value));
        }
        Self::canonical(name, kind, unit, description.into(), labels, false)
    }

    fn canonical(
        name: String,
        kind: MetricKind,
        unit: MetricUnit,
        description: String,
        mut labels: Vec<(String, String)>,
        framework: bool,
    ) -> Result<Self> {
        labels.sort_by(|left, right| left.0.cmp(&right.0));
        if labels.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(TelemetryError::Invalid("duplicate metric label".into()));
        }
        Ok(Self {
            name,
            kind,
            unit,
            description,
            labels,
            framework,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> MetricKind {
        self.kind
    }

    pub fn unit(&self) -> MetricUnit {
        self.unit
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn labels(&self) -> &[(String, String)] {
        &self.labels
    }

    pub fn validate(&self, limits: &RegistrationLimits) -> Result<()> {
        limits.validate()?;
        if self.name.len() > limits.name_bytes
            || self.description.len() > limits.description_bytes
            || self.labels.len() > limits.labels
            || !valid_word(&self.name, true)
        {
            return Err(TelemetryError::Invalid(
                "invalid metric name, description or label count".into(),
            ));
        }
        for (key, value) in &self.labels {
            if key.len() > limits.label_key_bytes
                || value.len() > limits.label_value_bytes
                || !valid_word(key, false)
                || value.is_empty()
                || value.chars().any(char::is_control)
            {
                return Err(TelemetryError::Invalid(format!(
                    "invalid metric label '{key}'"
                )));
            }
        }
        if self.framework {
            let definition = metric_definition(&self.name)
                .ok_or_else(|| TelemetryError::Invalid("unknown framework metric".into()))?;
            if definition.kind != self.kind || definition.unit != self.unit {
                return Err(TelemetryError::Invalid(
                    "framework metric type/unit mismatch".into(),
                ));
            }
        }
        Ok(())
    }
}

fn valid_word(value: &str, dots: bool) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.bytes().all(|ch| {
            ch.is_ascii_lowercase()
                || ch.is_ascii_digit()
                || ch == b'_'
                || ch == b'-'
                || (dots && ch == b'.')
        })
        && !value.ends_with('.')
        && !value.contains("..")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Schema {
    kind: MetricKind,
    unit: MetricUnit,
    labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Series {
    name: String,
    labels: Vec<(String, String)>,
}

#[derive(Debug, Default)]
struct Registrations {
    definitions: BTreeMap<String, Schema>,
    series: BTreeSet<Series>,
}

/// One host-owned registry per recorder. Share its `Arc` across graphs and SDK
/// integrations; retain definitions as long as that recorder retains instruments.
/// This object neither installs nor calls a recorder.
#[derive(Debug)]
pub struct MetricRegistrationState {
    limits: RegistrationLimits,
    registrations: Mutex<Registrations>,
}

impl Default for MetricRegistrationState {
    fn default() -> Self {
        Self {
            limits: RegistrationLimits::default(),
            registrations: Mutex::new(Registrations::default()),
        }
    }
}

impl MetricRegistrationState {
    pub fn new(limits: RegistrationLimits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            limits,
            registrations: Mutex::new(Registrations::default()),
        })
    }

    pub fn limits(&self) -> RegistrationLimits {
        self.limits
    }

    /// Validates and atomically accepts a definition/series before a caller
    /// invokes its recorder. No lock is held across host recorder callbacks.
    pub fn register(&self, registration: &MetricRegistration) -> Result<()> {
        registration.validate(&self.limits)?;
        let schema = Schema {
            kind: registration.kind,
            unit: registration.unit,
            labels: registration
                .labels
                .iter()
                .map(|(key, _)| key.clone())
                .collect(),
        };
        let series = Series {
            name: registration.name.clone(),
            labels: registration.labels.clone(),
        };
        let mut state = self
            .registrations
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?;
        let new_definition = match state.definitions.get(&registration.name) {
            Some(existing) if existing != &schema => {
                return Err(TelemetryError::Conflict(registration.name.clone()));
            }
            Some(_) => false,
            None => true,
        };
        if new_definition && state.definitions.len() >= self.limits.definitions {
            return Err(TelemetryError::Capacity("instrument definitions"));
        }
        let new_series = !state.series.contains(&series);
        if new_series && state.series.len() >= self.limits.series {
            return Err(TelemetryError::Capacity("metric series"));
        }
        if new_definition {
            state.definitions.insert(registration.name.clone(), schema);
        }
        if new_series {
            state.series.insert(series);
        }
        Ok(())
    }

    pub fn counts(&self) -> Result<(usize, usize)> {
        let state = self
            .registrations
            .lock()
            .map_err(|_| TelemetryError::Poisoned)?;
        Ok((state.definitions.len(), state.series.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ComponentId, ComponentRole, ComponentSemanticKind};
    use std::sync::{Arc, Barrier};

    fn scope(instance: &str) -> TelemetryScope {
        TelemetryScope::graph(instance, "graph").unwrap().component(
            ComponentId::try_new("component").unwrap(),
            ComponentRole::Source,
            ComponentSemanticKind::Source,
        )
    }

    fn custom(instance: &str, leaf: &str) -> MetricRegistration {
        MetricRegistration::plugin(
            &scope(instance),
            "test",
            leaf,
            MetricKind::Counter,
            MetricUnit::One,
            "Test metric",
            [("region".into(), "west".into())],
        )
        .unwrap()
    }

    #[test]
    fn identities_names_and_label_schemas_are_validated() {
        let scope = scope("instance");
        assert!(MetricRegistration::framework(
            "drasi.component.calls",
            &scope,
            [("operation".into(), "transform".into())],
        )
        .is_ok());
        assert!(MetricRegistration::framework(
            "drasi.component.calls",
            &scope,
            [("operation".into(), "payload".into())],
        )
        .is_err());
        assert!(MetricRegistration::framework("drasi.component.calls", &scope, []).is_err());
        assert!(MetricRegistration::framework(
            "drasi.component.calls",
            &scope,
            [
                ("operation".into(), "transform".into()),
                ("operation".into(), "transform".into())
            ],
        )
        .is_err());
        for key in RESERVED_LABELS {
            assert!(MetricRegistration::plugin(
                &scope,
                "test",
                "count",
                MetricKind::Counter,
                MetricUnit::One,
                "Test",
                [(key.to_string(), "spoof".into())],
            )
            .is_err());
        }
        for leaf in ["drasi.errors", "", "UPPER", "a..b", "a.", "a b"] {
            assert!(MetricRegistration::plugin_name("test", leaf).is_err());
        }
        assert!(MetricRegistration::plugin_name("test.other", "count").is_err());
        let mut invalid = custom("instance", "count");
        invalid.labels.push(("bad key".into(), "value".into()));
        assert!(invalid.validate(&RegistrationLimits::default()).is_err());
        assert!(TelemetryScope::graph("", "graph").is_err());
    }

    #[test]
    fn registration_limits_accept_exact_boundary_and_reject_one_beyond() {
        let registration = custom("instance", "count");
        let limits = RegistrationLimits::default();
        let setters: [fn(&mut RegistrationLimits, usize); 5] = [
            |limits, size| limits.name_bytes = size,
            |limits, size| limits.description_bytes = size,
            |limits, size| limits.labels = size,
            |limits, size| limits.label_key_bytes = size,
            |limits, size| limits.label_value_bytes = size,
        ];
        let sizes = [
            registration.name.len(),
            registration.description.len(),
            registration.labels.len(),
            registration
                .labels
                .iter()
                .map(|(key, _)| key.len())
                .max()
                .unwrap(),
            registration
                .labels
                .iter()
                .map(|(_, value)| value.len())
                .max()
                .unwrap(),
        ];
        for (set, size) in setters.into_iter().zip(sizes) {
            let mut boundary = limits;
            set(&mut boundary, size);
            assert!(registration.validate(&boundary).is_ok());
            set(&mut boundary, size - 1);
            assert!(registration.validate(&boundary).is_err());
        }
        assert!(MetricRegistrationState::new(RegistrationLimits {
            definitions: 0,
            ..limits
        })
        .is_err());
    }

    #[test]
    fn shared_names_conflict_across_instances_but_identity_values_do_not() {
        let state = MetricRegistrationState::default();
        let first = custom("one", "count");
        let second = custom("two", "count");
        state.register(&first).unwrap();
        state.register(&second).unwrap();
        state.register(&second).unwrap();
        assert_eq!(state.counts().unwrap(), (1, 2));
        let mut changed = second.clone();
        changed.kind = MetricKind::Gauge;
        assert!(matches!(
            state.register(&changed),
            Err(TelemetryError::Conflict(_))
        ));
        changed = second.clone();
        changed.unit = MetricUnit::Seconds;
        assert!(matches!(
            state.register(&changed),
            Err(TelemetryError::Conflict(_))
        ));
        changed = second;
        changed.labels.retain(|(key, _)| key != "region");
        assert!(matches!(
            state.register(&changed),
            Err(TelemetryError::Conflict(_))
        ));
        assert_eq!(state.counts().unwrap(), (1, 2));
        MetricRegistrationState::default()
            .register(&changed)
            .unwrap();
    }

    #[test]
    fn reordering_labels_does_not_change_the_schema() {
        let state = MetricRegistrationState::default();
        for labels in [
            vec![("a".into(), "1".into()), ("b".into(), "2".into())],
            vec![("b".into(), "2".into()), ("a".into(), "1".into())],
        ] {
            state
                .register(
                    &MetricRegistration::plugin(
                        &scope("one"),
                        "test",
                        "count",
                        MetricKind::Counter,
                        MetricUnit::One,
                        "Test",
                        labels,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        assert_eq!(state.counts().unwrap(), (1, 1));
    }

    #[test]
    fn capacity_is_atomic_and_never_evicts_definitions() {
        let state = MetricRegistrationState::new(RegistrationLimits {
            definitions: 1,
            series: 2,
            ..RegistrationLimits::default()
        })
        .unwrap();
        state.register(&custom("one", "count")).unwrap();
        state.register(&custom("two", "count")).unwrap();
        assert!(matches!(
            state.register(&custom("three", "count")),
            Err(TelemetryError::Capacity(_))
        ));
        assert!(matches!(
            state.register(&custom("one", "other")),
            Err(TelemetryError::Capacity(_))
        ));
        state.register(&custom("one", "count")).unwrap();
        assert_eq!(state.counts().unwrap(), (1, 2));
    }

    #[test]
    fn concurrent_conflicts_have_one_winner() {
        let state = Arc::new(MetricRegistrationState::default());
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = [MetricKind::Counter, MetricKind::Gauge]
            .into_iter()
            .map(|kind| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut registration = custom("instance", "count");
                    registration.kind = kind;
                    barrier.wait();
                    state.register(&registration)
                })
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(TelemetryError::Conflict(_))))
                .count(),
            1
        );
        assert_eq!(state.counts().unwrap(), (1, 1));
    }
}
