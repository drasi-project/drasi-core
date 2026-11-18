// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Retained members and incremental relation evaluation.
//!
//! A batch is prepared without mutation (so any failure leaves the state
//! unchanged) and then committed. Only pairs involving a touched member are
//! re-evaluated; untouched pairs keep their records.

use anyhow::{bail, ensure, Context, Result};
use drasi_computation_contracts::{ElementImage, ElementPatch};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementTimestamp, ElementValue,
    SourceChange,
};
use geo::{CoordsIter, Distance, Euclidean, Intersects, Rect, Relate};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::config::{GeometryConfig, InvalidGeometry, Limits, Predicate};
use crate::shape::{self, Rules, Shape};

pub(crate) type RefKey = (Arc<str>, Arc<str>);

fn ref_key(reference: &ElementReference) -> RefKey {
    (reference.namespace.clone(), reference.element_id.clone())
}

/// Output identity: the relation and its member keys, or an invalid record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum OutputKey {
    Relation {
        relation: usize,
        left: String,
        right: String,
    },
    Error {
        set: usize,
        element: RefKey,
    },
}

/// One emitted record. Records are compared by label and properties only.
#[derive(Debug, Clone)]
pub(crate) struct Record {
    pub element_id: Arc<str>,
    pub label: Arc<str>,
    pub properties: ElementPropertyMap,
    pub effective_from: ElementTimestamp,
}

impl Record {
    fn same(&self, other: &Self) -> bool {
        self.label == other.label && self.properties == other.properties
    }

    pub(crate) fn element(&self, namespace: &str) -> Element {
        Element::Node {
            metadata: self.metadata(namespace, self.effective_from),
            properties: self.properties.clone(),
        }
    }

    fn metadata(&self, namespace: &str, effective_from: ElementTimestamp) -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new(namespace, &self.element_id),
            labels: Arc::from([self.label.clone()]),
            effective_from,
        }
    }
}

pub(crate) type Records = BTreeMap<OutputKey, Record>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Membership {
    Member(String),
    Invalid,
}

#[derive(Debug, Clone)]
struct Member {
    properties: ElementPropertyMap,
    shape: Shape,
    vertices: usize,
    /// Threshold contribution per relation when this member is on the left.
    left_reach: Vec<f64>,
    right_reach: Vec<f64>,
}

/// The validated configuration in the form evaluation reads.
pub(crate) struct Engine {
    pub(crate) config: GeometryConfig,
    namespace: Arc<str>,
    rules: Rules,
    relations: Vec<(usize, usize, Arc<str>)>,
    labels: BTreeSet<String>,
}

impl Engine {
    pub(crate) fn new(config: GeometryConfig, namespace: &str) -> Result<Self> {
        let relations = config
            .relations
            .iter()
            .map(|relation| {
                Ok((
                    config
                        .set_index(&relation.left)
                        .context("unknown left set")?,
                    config
                        .set_index(&relation.right)
                        .context("unknown right set")?,
                    Arc::from(relation.label.as_str()),
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            rules: Rules {
                epsilon: config.epsilon,
                ring_closure: config.ring_closure,
                max_vertices: config.limits.max_vertices,
            },
            labels: config.sets.iter().map(|set| set.label.clone()).collect(),
            relations,
            namespace: Arc::from(namespace),
            config,
        })
    }

    pub(crate) fn namespace(&self) -> &Arc<str> {
        &self.namespace
    }

    fn relevant(&self, element: &Element) -> bool {
        element
            .get_metadata()
            .labels
            .iter()
            .any(|label| self.labels.contains(label.as_ref()))
    }

    fn sets_for(&self, element: &Element) -> Vec<usize> {
        let labels = &element.get_metadata().labels;
        let properties = element.get_properties();
        self.config
            .sets
            .iter()
            .enumerate()
            .filter(|(_, set)| {
                labels.iter().any(|label| label.as_ref() == set.label)
                    && set.filter.iter().all(|(name, expected)| {
                        let actual = properties.get(name).map_or(Value::Null, Value::from);
                        actual == *expected
                            || matches!((actual.as_f64(), expected.as_f64()), (Some(a), Some(b)) if a == b)
                    })
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn member(&self, set: usize, element: &Element) -> Result<(String, Member)> {
        let config = &self.config.sets[set];
        let properties = element.get_properties();
        let key = match &config.key {
            None => element.get_reference().element_id.to_string(),
            Some(name) => match properties.get(name) {
                Some(ElementValue::String(text)) if !text.is_empty() => text.to_string(),
                Some(ElementValue::Integer(number)) => number.to_string(),
                _ => bail!("key property '{name}' must be a nonempty string or an integer"),
            },
        };
        let shape = shape::extract(&config.geometry, properties, &self.rules)?;
        let mut left_reach = vec![0.0; self.relations.len()];
        let mut right_reach = vec![0.0; self.relations.len()];
        for (index, relation) in self.config.relations.iter().enumerate() {
            let Some(threshold) = &relation.threshold else {
                continue;
            };
            if self.relations[index].0 == set {
                left_reach[index] = reach(properties, &threshold.left)?;
            }
            if self.relations[index].1 == set {
                right_reach[index] = reach(properties, &threshold.right)?;
            }
        }
        Ok((
            key,
            Member {
                properties: properties.clone(),
                vertices: shape.geometry.coords_count(),
                shape,
                left_reach,
                right_reach,
            },
        ))
    }

    fn element_id(&self, key: &OutputKey) -> Result<Arc<str>> {
        let parts = match key {
            OutputKey::Relation {
                relation,
                left,
                right,
            } => [
                "relation",
                self.config.relations[*relation].name.as_str(),
                left,
                right,
            ],
            OutputKey::Error { set, element } => [
                "error",
                self.config.sets[*set].name.as_str(),
                &element.0,
                &element.1,
            ],
        };
        Ok(Arc::from(serde_json::to_string(&parts)?))
    }

    fn error_record(
        &self,
        set: usize,
        element: &RefKey,
        error: &anyhow::Error,
        effective_from: ElementTimestamp,
    ) -> Result<(OutputKey, Record)> {
        let key = OutputKey::Error {
            set,
            element: element.clone(),
        };
        let mut properties = ElementPropertyMap::new();
        properties.insert(
            "set",
            ElementValue::String(self.config.sets[set].name.as_str().into()),
        );
        properties.insert("element", ElementValue::String(element.1.clone()));
        properties.insert("error", ElementValue::String(format!("{error:#}").into()));
        Ok((
            key.clone(),
            Record {
                element_id: self.element_id(&key)?,
                label: Arc::from(self.config.error_label.as_str()),
                properties,
                effective_from,
            },
        ))
    }

    /// The relation record for one ordered pair, when the predicate holds.
    fn evaluate(
        &self,
        index: usize,
        (left_key, left): (&str, &Member),
        (right_key, right): (&str, &Member),
        effective_from: ElementTimestamp,
    ) -> Result<Option<(OutputKey, Record)>> {
        let relation = &self.config.relations[index];
        let epsilon = self.config.epsilon;
        let gap = gap(&left.shape.bounds, &right.shape.bounds);
        let (lg, rg) = (&left.shape.geometry, &right.shape.geometry);
        let (distance, threshold) = match relation.predicate {
            Predicate::WithinDistance => {
                let constant = relation.threshold.as_ref().map_or(0.0, |t| t.constant);
                let threshold = constant + left.left_reach[index] + right.right_reach[index];
                if gap > threshold + epsilon {
                    return Ok(None);
                }
                let distance = Euclidean.distance(lg, rg);
                if distance > threshold + epsilon {
                    return Ok(None);
                }
                (Some(distance), Some(threshold))
            }
            predicate => {
                if gap > 0.0 {
                    return Ok(None);
                }
                let holds = match predicate {
                    Predicate::Intersects => lg.intersects(rg),
                    Predicate::Contains => lg.relate(rg).is_contains(),
                    Predicate::Within => lg.relate(rg).is_within(),
                    Predicate::WithinDistance => unreachable!("handled above"),
                };
                if !holds {
                    return Ok(None);
                }
                let distance = relation
                    .properties
                    .distance
                    .is_some()
                    .then(|| Euclidean.distance(lg, rg));
                (distance, None)
            }
        };
        let mut properties = ElementPropertyMap::new();
        for (target, source, member) in relation
            .copy_left
            .iter()
            .map(|(target, source)| (target, source, left))
            .chain(
                relation
                    .copy_right
                    .iter()
                    .map(|(target, source)| (target, source, right)),
            )
        {
            if let Some(value) = member.properties.get(source) {
                properties.insert(target, value.clone());
            }
        }
        let names = &relation.properties;
        let text = |value: &str| ElementValue::String(value.into());
        let float = |value: f64| ElementValue::Float(value.into());
        for (name, value) in [
            (&names.id, Some(text(&format!("{left_key}/{right_key}")))),
            (&names.relation, Some(text(&relation.name))),
            (&names.left_key, Some(text(left_key))),
            (&names.right_key, Some(text(right_key))),
            (&names.distance, distance.map(float)),
            (&names.threshold, threshold.map(float)),
        ] {
            if let (Some(name), Some(value)) = (name, value) {
                properties.insert(name, value);
            }
        }
        let key = OutputKey::Relation {
            relation: index,
            left: left_key.into(),
            right: right_key.into(),
        };
        Ok(Some((
            key.clone(),
            Record {
                element_id: self.element_id(&key)?,
                label: self.relations[index].2.clone(),
                properties,
                effective_from,
            },
        )))
    }
}

fn reach(properties: &ElementPropertyMap, names: &[String]) -> Result<f64> {
    names.iter().try_fold(0.0, |total, name| {
        let value = shape::number(properties, name)?;
        ensure!(
            value >= 0.0,
            "threshold property '{name}' must not be negative"
        );
        Ok(total + value)
    })
}

/// The distance between two bounding rectangles (zero when they overlap).
fn gap(a: &Rect<f64>, b: &Rect<f64>) -> f64 {
    let dx = (a.min().x - b.max().x).max(b.min().x - a.max().x).max(0.0);
    let dy = (a.min().y - b.max().y).max(b.min().y - a.max().y).max(0.0);
    dx.hypot(dy)
}

/// A prepared batch: everything the commit writes, and the changes to emit.
pub(crate) struct Delta {
    inputs: BTreeMap<RefKey, Option<Element>>,
    memberships: BTreeMap<RefKey, BTreeMap<usize, Membership>>,
    removed: Vec<BTreeSet<String>>,
    added: Vec<BTreeMap<String, Member>>,
    retract: BTreeSet<OutputKey>,
    emit: Records,
    pub(crate) changes: Vec<SourceChange>,
}

#[derive(Clone)]
pub(crate) struct State {
    inputs: BTreeMap<RefKey, Element>,
    memberships: BTreeMap<RefKey, BTreeMap<usize, Membership>>,
    members: Vec<BTreeMap<String, Member>>,
    outputs: Arc<Records>,
    /// (relation, right key, left key) for retracting by the right member.
    by_right: BTreeSet<(usize, String, String)>,
}

impl State {
    pub(crate) fn new(sets: usize) -> Self {
        Self {
            inputs: BTreeMap::new(),
            memberships: BTreeMap::new(),
            members: vec![BTreeMap::new(); sets],
            outputs: Arc::new(Records::new()),
            by_right: BTreeSet::new(),
        }
    }

    pub(crate) fn outputs(&self) -> Arc<Records> {
        self.outputs.clone()
    }

    /// Whether the retained state already satisfies `limits`.
    pub(crate) fn fits(&self, limits: &Limits) -> bool {
        self.inputs.len() <= limits.max_elements
            && self.outputs.len() <= limits.max_outputs
            && self
                .members
                .iter()
                .flat_map(BTreeMap::values)
                .all(|member| member.vertices <= limits.max_vertices)
    }

    pub(crate) fn prepare(&self, changes: Vec<SourceChange>, engine: &Engine) -> Result<Delta> {
        let limits = &engine.config.limits;
        let mut inputs: BTreeMap<RefKey, Option<Element>> = BTreeMap::new();
        let mut time: ElementTimestamp = 0;
        for change in changes {
            change.validate()?;
            let metadata = match &change {
                SourceChange::Insert { element }
                | SourceChange::Replace { element }
                | SourceChange::Update { element, .. } => element.get_metadata(),
                SourceChange::Delete { metadata } => metadata,
                SourceChange::Future { .. } => {
                    bail!("geometry transforms do not accept scheduled notifications")
                }
            };
            time = time.max(metadata.effective_from);
            let reference = metadata.reference.clone();
            let key = ref_key(&reference);
            let next = match change {
                SourceChange::Insert { element } | SourceChange::Replace { element } => {
                    engine.relevant(&element).then_some(element)
                }
                SourceChange::Update {
                    element,
                    removed_properties,
                } => {
                    let base = match inputs.get(&key) {
                        Some(pending) => pending.as_ref(),
                        None => self.inputs.get(&key),
                    };
                    match base {
                        Some(base) => {
                            let patch = ElementPatch::try_new(
                                ElementImage::try_from(element)?,
                                removed_properties,
                            )?;
                            Some(patch.apply_to(Some(base))?).filter(|e| engine.relevant(e))
                        }
                        None if engine.relevant(&element) => {
                            bail!("geometry patch for {reference} has no retained full input image")
                        }
                        None => None,
                    }
                }
                SourceChange::Delete { .. } => None,
                SourceChange::Future { .. } => unreachable!("rejected above"),
            };
            inputs.insert(key, next);
        }
        inputs.retain(|key, next| next.is_some() || self.inputs.contains_key(key));
        let mut retained = self.inputs.len();
        for (key, next) in &inputs {
            match (self.inputs.contains_key(key), next.is_some()) {
                (false, true) => retained += 1,
                (true, false) => retained -= 1,
                _ => {}
            }
        }
        ensure!(
            retained <= limits.max_elements,
            "the input would retain {retained} elements, more than limits.maxElements ({})",
            limits.max_elements
        );

        let sets = engine.config.sets.len();
        let mut removed = vec![BTreeSet::new(); sets];
        let mut added: Vec<BTreeMap<String, Member>> = vec![BTreeMap::new(); sets];
        let mut retract = BTreeSet::new();
        let mut emit = Records::new();
        for key in inputs.keys() {
            for (set, membership) in self.memberships.get(key).into_iter().flatten() {
                match membership {
                    Membership::Member(member) => {
                        removed[*set].insert(member.clone());
                    }
                    Membership::Invalid => {
                        retract.insert(OutputKey::Error {
                            set: *set,
                            element: key.clone(),
                        });
                    }
                }
            }
        }
        let mut memberships = BTreeMap::new();
        for (key, element) in &inputs {
            let mut current = BTreeMap::new();
            let Some(element) = element else {
                memberships.insert(key.clone(), current);
                continue;
            };
            for set in engine.sets_for(element) {
                let outcome = engine.member(set, element).and_then(|(member, value)| {
                    ensure!(
                        !added[set].contains_key(&member)
                            && (!self.members[set].contains_key(&member)
                                || removed[set].contains(&member)),
                        "duplicate key '{member}'"
                    );
                    Ok((member, value))
                });
                match outcome {
                    Ok((member, value)) => {
                        added[set].insert(member.clone(), value);
                        current.insert(set, Membership::Member(member));
                    }
                    Err(error) => {
                        let error = error.context(format!(
                            "invalid member of set '{}': element {}",
                            engine.config.sets[set].name,
                            element.get_reference()
                        ));
                        match engine.config.invalid_geometry {
                            InvalidGeometry::Fail => return Err(error),
                            InvalidGeometry::Skip => {}
                            InvalidGeometry::Report => {
                                let (output, record) =
                                    engine.error_record(set, key, &error, time)?;
                                emit.insert(output, record);
                                current.insert(set, Membership::Invalid);
                            }
                        }
                    }
                }
            }
            memberships.insert(key.clone(), current);
        }

        for (index, &(left, right, _)) in engine.relations.iter().enumerate() {
            for key in removed[left].iter().chain(added[left].keys()) {
                let start = OutputKey::Relation {
                    relation: index,
                    left: key.clone(),
                    right: String::new(),
                };
                retract.extend(
                    self.outputs
                        .range(start..)
                        .map(|(output, _)| output)
                        .take_while(|output| {
                            matches!(output, OutputKey::Relation { relation, left, .. } if *relation == index && left == key)
                        })
                        .cloned(),
                );
            }
            for key in removed[right].iter().chain(added[right].keys()) {
                retract.extend(
                    self.by_right
                        .range((index, key.clone(), String::new())..)
                        .take_while(|(relation, right, _)| *relation == index && right == key)
                        .map(|(_, right, left)| OutputKey::Relation {
                            relation: index,
                            left: left.clone(),
                            right: right.clone(),
                        }),
                );
            }
            let base = |set: usize| {
                let (removed, added) = (&removed[set], &added[set]);
                self.members[set]
                    .iter()
                    .filter(move |(key, _)| !removed.contains(*key) && !added.contains_key(*key))
            };
            let mut evaluate = |left: (&String, &Member), right: (&String, &Member)| {
                if engine.relations[index].0 == engine.relations[index].1 && left.0 == right.0 {
                    return Ok(());
                }
                if let Some((output, record)) =
                    engine.evaluate(index, (left.0, left.1), (right.0, right.1), time)?
                {
                    emit.insert(output, record);
                }
                anyhow::Ok(())
            };
            for left_member in &added[left] {
                for right_member in base(right).chain(&added[right]) {
                    evaluate(left_member, right_member)?;
                }
            }
            for right_member in &added[right] {
                for left_member in base(left) {
                    evaluate(left_member, right_member)?;
                }
            }
        }

        let retracted = retract
            .iter()
            .filter(|key| self.outputs.contains_key(*key))
            .count();
        let total = self.outputs.len() - retracted + emit.len();
        ensure!(
            total <= limits.max_outputs,
            "the input would produce {total} records, more than limits.maxOutputs ({})",
            limits.max_outputs
        );

        let mut deletes = Vec::new();
        let mut upserts = Vec::new();
        let namespace = engine.namespace();
        let touched: Vec<OutputKey> = retract
            .iter()
            .chain(emit.keys().filter(|key| !retract.contains(*key)))
            .cloned()
            .collect();
        for output in &touched {
            let previous = self
                .outputs
                .get(output)
                .filter(|_| retract.contains(output));
            match (previous, emit.get_mut(output)) {
                (Some(previous), None) => deletes.push(SourceChange::Delete {
                    metadata: previous.metadata(namespace, time.max(previous.effective_from)),
                }),
                (None, Some(current)) => upserts.push(SourceChange::Insert {
                    element: current.element(namespace),
                }),
                (Some(previous), Some(current)) if previous.same(current) => {
                    *current = previous.clone();
                }
                (Some(_), Some(current)) => upserts.push(SourceChange::Replace {
                    element: current.element(namespace),
                }),
                (None, None) => {}
            }
        }
        deletes.extend(upserts);
        Ok(Delta {
            inputs,
            memberships,
            removed,
            added,
            retract,
            emit,
            changes: deletes,
        })
    }

    /// Writes a prepared batch. Cannot fail.
    pub(crate) fn commit(&mut self, delta: Delta) {
        for (key, next) in delta.inputs {
            match next {
                Some(element) => self.inputs.insert(key, element),
                None => self.inputs.remove(&key),
            };
        }
        for (key, memberships) in delta.memberships {
            if memberships.is_empty() {
                self.memberships.remove(&key);
            } else {
                self.memberships.insert(key, memberships);
            }
        }
        for (set, keys) in delta.removed.into_iter().enumerate() {
            for key in keys {
                self.members[set].remove(&key);
            }
        }
        for (set, members) in delta.added.into_iter().enumerate() {
            self.members[set].extend(members);
        }
        let outputs = Arc::make_mut(&mut self.outputs);
        for key in delta.retract {
            if let OutputKey::Relation {
                relation,
                left,
                right,
            } = &key
            {
                self.by_right
                    .remove(&(*relation, right.clone(), left.clone()));
            }
            outputs.remove(&key);
        }
        for (key, record) in delta.emit {
            if let OutputKey::Relation {
                relation,
                left,
                right,
            } = &key
            {
                self.by_right
                    .insert((*relation, right.clone(), left.clone()));
            }
            outputs.insert(key, record);
        }
    }
}
