// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Retained members and incremental relation evaluation.
//!
//! The relevant inputs, each input's set memberships and the emitted records
//! are durable, in the transformer's host stores. Members with their shapes,
//! the ordered records and the right-member index are rebuilt from them when
//! the state opens; memberships are stored rather than re-derived because
//! arrival order decides which input owns a duplicated key.
//!
//! A batch is prepared without mutation (so any failure leaves the state
//! unchanged), stored and then committed. Only pairs involving a touched
//! member are re-evaluated; untouched pairs keep their records.

use anyhow::{bail, ensure, Context, Result};
use drasi_computation_plugin_sdk::transformer::{
    apply_inputs_where, get_values, set_value, InputChange, StateStore,
};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementTimestamp, ElementValue,
    SourceChange,
};
use geo::{CoordsIter, Distance, Euclidean, Intersects, Rect, Relate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

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
#[cfg_attr(test, derive(PartialEq))]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Membership {
    Member(String),
    Invalid,
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
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

    pub(crate) fn relevant(&self, element: &Element) -> bool {
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

/// The private table of each retained input's set memberships.
const MEMBERSHIPS: &str = "geometry/memberships";
/// The private table of the records the transformer emitted.
const RECORDS: &str = "geometry/records";

fn input_key(key: &RefKey) -> Result<String> {
    Ok(serde_json::to_string(&(&*key.0, &*key.1))?)
}

fn restore_input_key(key: &str) -> Result<RefKey> {
    let (namespace, element): (String, String) =
        serde_json::from_str(key).context("malformed geometry membership key")?;
    Ok((namespace.into(), element.into()))
}

/// The stored form of an [`OutputKey`].
#[derive(Serialize, Deserialize)]
enum StoredKey {
    Relation(usize, String, String),
    Error(usize, String, String),
}

impl OutputKey {
    fn stored(&self) -> Result<String> {
        Ok(serde_json::to_string(&match self {
            Self::Relation {
                relation,
                left,
                right,
            } => StoredKey::Relation(*relation, left.clone(), right.clone()),
            Self::Error { set, element } => {
                StoredKey::Error(*set, element.0.to_string(), element.1.to_string())
            }
        })?)
    }

    fn restore(key: &str) -> Result<Self> {
        Ok(
            match serde_json::from_str(key).context("malformed geometry record key")? {
                StoredKey::Relation(relation, left, right) => Self::Relation {
                    relation,
                    left,
                    right,
                },
                StoredKey::Error(set, namespace, element) => Self::Error {
                    set,
                    element: (namespace.into(), element.into()),
                },
            },
        )
    }
}

impl Record {
    fn restore(element: Element) -> Result<Self> {
        let metadata = element.get_metadata();
        let [label] = &*metadata.labels else {
            bail!("a stored geometry record must have exactly one label");
        };
        Ok(Self {
            element_id: metadata.reference.element_id.clone(),
            label: label.clone(),
            properties: element.get_properties().clone(),
            effective_from: metadata.effective_from,
        })
    }
}

/// A prepared batch: everything the commit writes, and the changes to emit.
pub(crate) struct Delta {
    memberships: BTreeMap<RefKey, BTreeMap<usize, Membership>>,
    /// The membership entries to store or forget.
    stored_memberships: Vec<(RefKey, Option<BTreeMap<usize, Membership>>)>,
    removed: Vec<BTreeSet<String>>,
    added: Vec<BTreeMap<String, Member>>,
    retract: BTreeSet<OutputKey>,
    emit: Records,
    /// The records to store or forget.
    stored_records: Vec<(OutputKey, Option<Record>)>,
    retained: usize,
    pub(crate) changes: Vec<SourceChange>,
}

/// The in-memory structures derived from the stored state.
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct State {
    /// How many relevant inputs are retained.
    retained: usize,
    memberships: BTreeMap<RefKey, BTreeMap<usize, Membership>>,
    members: Vec<BTreeMap<String, Member>>,
    outputs: Records,
    /// (relation, right key, left key) for retracting by the right member.
    by_right: BTreeSet<(usize, String, String)>,
}

impl State {
    pub(crate) fn new(sets: usize) -> Self {
        Self {
            retained: 0,
            memberships: BTreeMap::new(),
            members: vec![BTreeMap::new(); sets],
            outputs: Records::new(),
            by_right: BTreeSet::new(),
        }
    }

    /// The current records.
    #[cfg(test)]
    pub(crate) fn outputs(&self) -> &Records {
        &self.outputs
    }

    /// Whether the retained state already satisfies `limits`.
    pub(crate) fn fits(&self, limits: &Limits) -> bool {
        self.retained <= limits.max_elements
            && self.outputs.len() <= limits.max_outputs
            && self
                .members
                .iter()
                .flat_map(BTreeMap::values)
                .all(|member| member.vertices <= limits.max_vertices)
    }

    /// Evaluate the inputs a batch changed, at the batch's latest effective
    /// time.
    pub(crate) fn prepare(
        &self,
        changed: Vec<InputChange>,
        time: ElementTimestamp,
        engine: &Engine,
    ) -> Result<Delta> {
        let limits = &engine.config.limits;
        // Each changed input: whether it was retained, and its image now.
        let inputs: BTreeMap<RefKey, (bool, Option<Element>)> = changed
            .into_iter()
            .filter(|input| input.previous.is_some() || input.current.is_some())
            .map(|input| {
                (
                    ref_key(&input.reference),
                    (input.previous.is_some(), input.current),
                )
            })
            .collect();
        let mut retained = self.retained;
        for (was_retained, next) in inputs.values() {
            match (*was_retained, next.is_some()) {
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
        for (key, (_, element)) in &inputs {
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
        let stored_memberships = memberships
            .iter()
            .filter(|(key, current)| self.memberships.get(*key) != Some(*current))
            .filter(|(key, current)| !current.is_empty() || self.memberships.contains_key(*key))
            .map(|(key, current)| (key.clone(), (!current.is_empty()).then(|| current.clone())))
            .collect();

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
        let mut stored_records = Vec::new();
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
                (Some(previous), None) => {
                    deletes.push(SourceChange::Delete {
                        metadata: previous.metadata(namespace, time.max(previous.effective_from)),
                    });
                    stored_records.push((output.clone(), None));
                }
                (None, Some(current)) => {
                    upserts.push(SourceChange::Insert {
                        element: current.element(namespace),
                    });
                    stored_records.push((output.clone(), Some(current.clone())));
                }
                (Some(previous), Some(current)) if previous.same(current) => {
                    *current = previous.clone();
                }
                (Some(_), Some(current)) => {
                    upserts.push(SourceChange::Replace {
                        element: current.element(namespace),
                    });
                    stored_records.push((output.clone(), Some(current.clone())));
                }
                (None, None) => {}
            }
        }
        deletes.extend(upserts);
        Ok(Delta {
            memberships,
            stored_memberships,
            removed,
            added,
            retract,
            emit,
            stored_records,
            retained,
            changes: deletes,
        })
    }

    /// Writes a prepared batch to the in-memory structures. Cannot fail.
    pub(crate) fn commit(&mut self, delta: Delta) {
        self.retained = delta.retained;
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
            self.outputs.remove(&key);
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
            self.outputs.insert(key, record);
        }
    }

    /// Rebuild the in-memory structures from the stored state. Retained state
    /// must satisfy the configuration, including its limits.
    pub(crate) async fn open<S: StateStore + ?Sized>(store: &S, engine: &Engine) -> Result<Self> {
        let mut state = Self::new(engine.config.sets.len());
        let inputs: BTreeMap<RefKey, Element> = store
            .inputs()
            .await?
            .into_iter()
            .map(|element| (ref_key(element.get_reference()), element))
            .collect();
        state.retained = inputs.len();
        for (key, memberships) in
            get_values::<BTreeMap<usize, Membership>, _>(store, MEMBERSHIPS).await?
        {
            let key = restore_input_key(&key)?;
            let element = inputs.get(&key).with_context(|| {
                format!(
                    "geometry membership of {}/{} has no retained input",
                    key.0, key.1
                )
            })?;
            for (set, membership) in &memberships {
                let name = &engine
                    .config
                    .sets
                    .get(*set)
                    .context("geometry membership names an unknown set")?
                    .name;
                let Membership::Member(member) = membership else {
                    continue;
                };
                let (computed, value) = engine.member(*set, element).with_context(|| {
                    format!("retained member '{member}' of set '{name}' no longer fits the configuration")
                })?;
                ensure!(
                    computed == *member,
                    "retained member '{member}' of set '{name}' disagrees with its input"
                );
                ensure!(
                    state.members[*set].insert(computed, value).is_none(),
                    "retained member '{member}' of set '{name}' is duplicated"
                );
            }
            state.memberships.insert(key, memberships);
        }
        for (key, element) in get_values::<Element, _>(store, RECORDS).await? {
            let key = OutputKey::restore(&key)?;
            if let OutputKey::Relation {
                relation,
                left,
                right,
            } = &key
            {
                state
                    .by_right
                    .insert((*relation, right.clone(), left.clone()));
            }
            state.outputs.insert(key, Record::restore(element)?);
        }
        let limits = &engine.config.limits;
        ensure!(
            state.retained <= limits.max_elements,
            "retained geometry state holds {} elements, more than limits.maxElements ({})",
            state.retained,
            limits.max_elements
        );
        ensure!(
            state.outputs.len() <= limits.max_outputs,
            "retained geometry state holds {} records, more than limits.maxOutputs ({})",
            state.outputs.len(),
            limits.max_outputs
        );
        Ok(state)
    }
}

/// Store a prepared batch's memberships and records.
async fn persist<S: StateStore + ?Sized>(store: &S, delta: &Delta, namespace: &str) -> Result<()> {
    for (key, memberships) in &delta.stored_memberships {
        let key = input_key(key)?;
        match memberships {
            Some(memberships) => set_value(store, MEMBERSHIPS, &key, memberships).await?,
            None => store.delete_value(MEMBERSHIPS, &key).await?,
        }
    }
    for (key, record) in &delta.stored_records {
        let key = key.stored()?;
        match record {
            Some(record) => set_value(store, RECORDS, &key, &record.element(namespace)).await?,
            None => store.delete_value(RECORDS, &key).await?,
        }
    }
    Ok(())
}

fn lock(state: &Mutex<State>) -> Result<std::sync::MutexGuard<'_, State>> {
    state
        .lock()
        .map_err(|_| anyhow::anyhow!("geometry state poisoned"))
}

/// Apply one batch: retain its relevant inputs, store the resulting
/// memberships and records, and return the record changes to emit.
///
/// The in-memory structures change last. If the surrounding operation still
/// fails, the runtime fences the transformer and reopens the stored state.
pub(crate) async fn transform<S: StateStore + ?Sized>(
    store: &S,
    state: &Mutex<State>,
    engine: &Engine,
    changes: &[SourceChange],
) -> Result<Vec<SourceChange>> {
    let mut time: ElementTimestamp = 0;
    for change in changes {
        let metadata = match change {
            SourceChange::Insert { element }
            | SourceChange::Replace { element }
            | SourceChange::Update { element, .. } => element.get_metadata(),
            SourceChange::Delete { metadata } => metadata,
            SourceChange::Future { .. } => {
                bail!("geometry transforms do not accept scheduled notifications")
            }
        };
        time = time.max(metadata.effective_from);
    }
    let changed = apply_inputs_where(store, changes, |element| engine.relevant(element)).await?;
    let mut delta = lock(state)?.prepare(changed, time, engine)?;
    persist(store, &delta, engine.namespace()).await?;
    let changes = std::mem::take(&mut delta.changes);
    lock(state)?.commit(delta);
    Ok(changes)
}
