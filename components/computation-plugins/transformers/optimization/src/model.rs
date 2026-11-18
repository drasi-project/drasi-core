// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Pure model construction and solving: index sets from records, the linear
//! model, lexicographic solving, scenarios and the model signature.

use anyhow::{anyhow, bail, Context, Result};
use good_lp::{
    constraint, microlp, variable, Expression, ProblemVariables, ResolutionError, Solution,
    SolutionStatus, SolverModel, Variable, WithTimeLimit,
};
use serde_json::Value;
use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::config::{
    CompareOp, Condition, OptimizationConfig, Quantity, RelOp, ScenarioDef, Sense, SetDef, Source,
    TermDef, VariableType, ID_PROPERTY, NAMESPACE_PROPERTY,
};

/// Values differing by less than this are treated as equal when checking
/// constant constraints and cleaning solver output.
const EPSILON: f64 = 1e-9;
/// Continuous values with a smaller magnitude are reported as zero.
const ZERO: f64 = 1e-6;

/// One retained input record.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Record {
    pub namespace: Arc<str>,
    pub id: Arc<str>,
    pub labels: Vec<Arc<str>>,
    pub properties: BTreeMap<String, Value>,
}

pub(crate) type Properties = BTreeMap<String, Value>;

/// A canonical, totally ordered property value. Numbers compare by value, so
/// `1` and `1.0` are the same key.
#[derive(Clone, Debug)]
pub(crate) enum Key {
    Null,
    Bool(bool),
    Number(f64),
    String(Arc<str>),
    Other(String),
}

impl Key {
    pub(crate) fn of(value: Option<&Value>) -> Self {
        match value {
            None | Some(Value::Null) => Self::Null,
            Some(Value::Bool(value)) => Self::Bool(*value),
            Some(Value::Number(number)) => match number.as_f64() {
                Some(0.0) => Self::Number(0.0),
                Some(value) => Self::Number(value),
                None => Self::Other(number.to_string()),
            },
            Some(Value::String(value)) => Self::String(Arc::from(value.as_str())),
            Some(other) => Self::Other(other.to_string()),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::Null => 0,
            Self::Bool(_) => 1,
            Self::Number(_) => 2,
            Self::String(_) => 3,
            Self::Other(_) => 4,
        }
    }

    /// The text used in element ids.
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Null => "null".into(),
            Self::Bool(value) => value.to_string(),
            Self::Number(value) if value.fract() == 0.0 && value.abs() < 1e15 => {
                format!("{}", *value as i64)
            }
            Self::Number(value) => value.to_string(),
            Self::String(value) => value.to_string(),
            Self::Other(value) => value.clone(),
        }
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Key {}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => a.cmp(b),
            (Self::Number(a), Self::Number(b)) => a.total_cmp(b),
            (Self::String(a), Self::String(b)) => a.cmp(b),
            (Self::Other(a), Self::Other(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.rank().hash(state);
        match self {
            Self::Null => {}
            Self::Bool(value) => value.hash(state),
            Self::Number(value) => value.to_bits().hash(state),
            Self::String(value) => value.hash(state),
            Self::Other(value) => value.hash(state),
        }
    }
}

/// A set member: a canonical key and the properties visible to conditions,
/// coefficients and output.
#[derive(Clone, Debug)]
pub(crate) struct Member {
    pub key: Vec<Key>,
    pub properties: Arc<Properties>,
}

/// A model-size limit was exceeded; reported as status `limit`.
#[derive(Debug)]
pub(crate) struct LimitExceeded(pub String);

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LimitExceeded {}

/// Members of `removeFrom` sets whose property equals a value are removed.
pub(crate) struct Removal<'a> {
    pub sets: Vec<&'a str>,
    pub property: &'a str,
    pub value: &'a Key,
}

type Index = HashMap<Vec<Key>, Vec<usize>>;

/// Materialized index sets, in configuration order.
pub(crate) struct Sets {
    members: HashMap<String, Arc<Vec<Member>>>,
    order: Vec<String>,
    indexes: RefCell<HashMap<(String, Vec<String>), Arc<Index>>>,
}

impl Sets {
    pub(crate) fn materialize(
        config: &OptimizationConfig,
        records: &[Record],
        removal: Option<&Removal<'_>>,
    ) -> Result<Self> {
        let mut sets = Self {
            members: HashMap::new(),
            order: Vec::new(),
            indexes: RefCell::new(HashMap::new()),
        };
        let limit = usize::try_from(config.max_variables).unwrap_or(usize::MAX);
        for definition in &config.sets {
            let mut members = match (&definition.from, &definition.expand) {
                (Some(from), Some(expand)) => {
                    let parents = sets.get(from)?.clone();
                    let mut members = Vec::new();
                    for parent in parents.iter() {
                        let count = parent.properties.get(&expand.count);
                        let count = count
                            .and_then(Value::as_u64)
                            .or_else(|| {
                                count
                                    .and_then(Value::as_f64)
                                    .filter(|value| value.fract() == 0.0 && *value >= 0.0)
                                    .map(|value| value as u64)
                            })
                            .with_context(|| {
                                format!(
                                    "set {} expands {} with count property {} that is not a nonnegative integer",
                                    definition.name,
                                    describe(&parent.key),
                                    expand.count
                                )
                            })?;
                        if count > (limit.saturating_sub(members.len())) as u64 {
                            return Err(LimitExceeded(format!(
                                "set {} expands to more than maxVariables ({limit}) members",
                                definition.name
                            ))
                            .into());
                        }
                        for index in 0..count {
                            let mut properties = (*parent.properties).clone();
                            properties.insert(expand.index.clone(), Value::from(index));
                            if !sets.all(&definition.filter, &properties)? {
                                continue;
                            }
                            let mut key = parent.key.clone();
                            key.push(Key::Number(index as f64));
                            members.push(Member {
                                key,
                                properties: Arc::new(properties),
                            });
                        }
                    }
                    members
                }
                _ => sets.select(definition, records)?,
            };
            if let Some(removal) = removal {
                if removal.sets.contains(&definition.name.as_str()) {
                    members.retain(|member| {
                        Key::of(member.properties.get(removal.property)) != *removal.value
                    });
                }
            }
            sets.order.push(definition.name.clone());
            sets.members
                .insert(definition.name.clone(), Arc::new(members));
        }
        Ok(sets)
    }

    fn select(&self, definition: &SetDef, records: &[Record]) -> Result<Vec<Member>> {
        let mut members = Vec::new();
        for record in records {
            if definition
                .label
                .as_ref()
                .is_some_and(|label| !record.labels.iter().any(|l| **l == **label))
                || definition
                    .namespace
                    .as_ref()
                    .is_some_and(|namespace| **namespace != *record.namespace)
            {
                continue;
            }
            let mut properties = record.properties.clone();
            properties.insert(ID_PROPERTY.into(), Value::from(&*record.id));
            properties.insert(NAMESPACE_PROPERTY.into(), Value::from(&*record.namespace));
            if !self.all(&definition.filter, &properties)? {
                continue;
            }
            let key = match &definition.key {
                Some(names) => names
                    .iter()
                    .map(|name| Key::of(properties.get(name)))
                    .collect(),
                None => vec![
                    Key::String(record.namespace.clone()),
                    Key::String(record.id.clone()),
                ],
            };
            members.push(Member {
                key,
                properties: Arc::new(properties),
            });
        }
        members.sort_by(|a, b| a.key.cmp(&b.key));
        if let Some(pair) = members.windows(2).find(|pair| pair[0].key == pair[1].key) {
            bail!(
                "set {} has more than one member with key {}",
                definition.name,
                describe(&pair[0].key)
            );
        }
        Ok(members)
    }

    pub(crate) fn get(&self, name: &str) -> Result<&Arc<Vec<Member>>> {
        self.members
            .get(name)
            .ok_or_else(|| anyhow!("set {name} is not materialized"))
    }

    pub(crate) fn total(&self) -> usize {
        self.members.values().map(|members| members.len()).sum()
    }

    /// Member count of each set, by name.
    pub(crate) fn sizes(&self) -> BTreeMap<String, usize> {
        self.members
            .iter()
            .map(|(name, members)| (name.clone(), members.len()))
            .collect()
    }

    /// The first precondition that does not hold, as `(name, detail)`.
    fn unmet(&self, config: &OptimizationConfig) -> Result<Option<(String, String)>> {
        const SHOWN: usize = 5;
        for precondition in &config.preconditions {
            let mut matching = Vec::new();
            for member in self.get(&precondition.set)?.iter() {
                if self.all(&precondition.filter, &member.properties)? {
                    matching.push(member);
                }
            }
            let count = matching.len() as u64;
            let bound = match (precondition.min, precondition.max) {
                (Some(min), _) if count < min => format!("at least {min}"),
                (_, Some(max)) if count > max => format!("at most {max}"),
                _ => continue,
            };
            let mut detail = format!(
                "precondition {} does not hold: {count} member{} of {} match{}, expected {bound}",
                precondition.name,
                if count == 1 { "" } else { "s" },
                precondition.set,
                if count == 1 { "es" } else { "" },
            );
            if precondition.max.is_some_and(|max| count > max) {
                let shown: Vec<String> = matching
                    .iter()
                    .take(SHOWN)
                    .map(|member| describe(&member.key))
                    .collect();
                detail.push_str(&format!(
                    ": {}{}",
                    shown.join(", "),
                    if matching.len() > SHOWN { ", …" } else { "" }
                ));
            }
            return Ok(Some((precondition.name.clone(), detail)));
        }
        Ok(None)
    }

    fn index(&self, set: &str, properties: Vec<String>) -> Result<Arc<Index>> {
        let cache_key = (set.to_owned(), properties);
        if let Some(index) = self.indexes.borrow().get(&cache_key) {
            return Ok(index.clone());
        }
        let mut index = Index::new();
        for (position, member) in self.get(set)?.iter().enumerate() {
            index
                .entry(
                    cache_key
                        .1
                        .iter()
                        .map(|name| Key::of(member.properties.get(name)))
                        .collect(),
                )
                .or_default()
                .push(position);
        }
        let index = Arc::new(index);
        self.indexes.borrow_mut().insert(cache_key, index.clone());
        Ok(index)
    }

    pub(crate) fn all(&self, conditions: &[Condition], properties: &Properties) -> Result<bool> {
        for condition in conditions {
            if !self.holds(condition, properties)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn holds(&self, condition: &Condition, properties: &Properties) -> Result<bool> {
        let (set, matches, wanted) = match condition {
            Condition::Compare(compare) => {
                let left = properties.get(&compare.property);
                let right = match &compare.value_property {
                    Some(name) => properties.get(name),
                    None => compare.value.as_ref(),
                };
                return Ok(compare_values(compare.op, left, right));
            }
            Condition::InSet(condition) => (&condition.in_set, &condition.matches, true),
            Condition::NotInSet(condition) => (&condition.not_in_set, &condition.matches, false),
        };
        let index = self.index(set, matches.values().cloned().collect())?;
        let key: Vec<Key> = matches
            .keys()
            .map(|name| Key::of(properties.get(name)))
            .collect();
        Ok(index.contains_key(&key) == wanted)
    }

    /// Feeds every member of every set, in order, into the signature.
    fn digest(&self, hasher: &mut Fnv) {
        for name in &self.order {
            hasher.write_str(name);
            let members = &self.members[name];
            hasher.write_u64(members.len() as u64);
            for member in members.iter() {
                for key in &member.key {
                    hasher.write_str(&key.text());
                    hasher.write_u64(u64::from(key.rank()));
                }
                hasher.write_str(
                    &Value::from(
                        member
                            .properties
                            .iter()
                            .map(|(name, value)| (name.clone(), value.clone()))
                            .collect::<serde_json::Map<_, _>>(),
                    )
                    .to_string(),
                );
            }
        }
    }
}

fn compare_values(op: CompareOp, left: Option<&Value>, right: Option<&Value>) -> bool {
    let present = left.is_some_and(|value| !value.is_null());
    match op {
        CompareOp::Exists => present,
        CompareOp::NotExists => !present,
        CompareOp::Eq => Key::of(left) == Key::of(right),
        CompareOp::Ne => Key::of(left) != Key::of(right),
        CompareOp::In | CompareOp::NotIn => {
            let left = Key::of(left);
            let found = right
                .and_then(Value::as_array)
                .is_some_and(|values| values.iter().any(|value| Key::of(Some(value)) == left));
            found == (op == CompareOp::In)
        }
        CompareOp::Lt | CompareOp::Le | CompareOp::Gt | CompareOp::Ge => {
            let ordering = match (Key::of(left), Key::of(right)) {
                (Key::Number(a), Key::Number(b)) => a.partial_cmp(&b),
                (Key::String(a), Key::String(b)) => Some(a.cmp(&b)),
                _ => None,
            };
            ordering.is_some_and(|ordering| match op {
                CompareOp::Lt => ordering == Ordering::Less,
                CompareOp::Le => ordering != Ordering::Greater,
                CompareOp::Gt => ordering == Ordering::Greater,
                _ => ordering != Ordering::Less,
            })
        }
    }
}

/// A member key for messages.
pub(crate) fn describe(key: &[Key]) -> String {
    format!(
        "[{}]",
        key.iter().map(Key::text).collect::<Vec<_>>().join(", ")
    )
}

/// 64-bit FNV-1a, stable across platforms and releases.
pub(crate) struct Fnv(u64);

impl Fnv {
    pub(crate) fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    pub(crate) fn write_str(&mut self, value: &str) {
        self.write_u64(value.len() as u64);
        self.write(value.as_bytes());
    }
    pub(crate) fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }
    pub(crate) fn finish(&self) -> String {
        format!("{:016x}", self.0)
    }
}

/// One variable of the built model.
#[derive(Clone, Debug)]
pub(crate) struct ModelVariable {
    pub family: usize,
    /// The member of the family's set; `None` for a scalar variable.
    pub member: Option<Member>,
    pub kind: VariableType,
    pub lower: f64,
    pub upper: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub coefficients: Vec<(usize, f64)>,
    pub op: RelOp,
    pub rhs: f64,
}

#[derive(Clone, Debug)]
pub(crate) struct Objective {
    pub name: String,
    pub sense: Sense,
    pub coefficients: Vec<(usize, f64)>,
}

/// A linear model ready to solve.
#[derive(Clone, Debug, Default)]
pub(crate) struct LinearModel {
    pub variables: Vec<ModelVariable>,
    pub rows: Vec<Row>,
    pub objectives: Vec<Objective>,
}

/// Building stopped early: the data already proves infeasibility.
pub(crate) enum Built {
    Model(LinearModel),
    Infeasible(String),
}

type Coefficients = BTreeMap<usize, f64>;

/// The variables a term may select: filtered members of the family.
struct TermVariables<'a> {
    term: &'a TermDef,
    /// Variable index and member properties (empty for scalars).
    candidates: Vec<(usize, Arc<Properties>)>,
    /// For terms with `match`: matched-property values to candidate positions.
    index: Option<HashMap<Vec<Key>, Vec<usize>>>,
}

impl LinearModel {
    pub(crate) fn build(config: &OptimizationConfig, sets: &Sets) -> Result<Built> {
        let max_variables = usize::try_from(config.max_variables).unwrap_or(usize::MAX);
        let max_rows = usize::try_from(config.max_constraints).unwrap_or(usize::MAX);
        let mut model = Self::default();
        // Variables of each family, in member order.
        let mut families: Vec<Vec<usize>> = Vec::with_capacity(config.variables.len());
        let mut by_name = HashMap::new();
        for (family, definition) in config.variables.iter().enumerate() {
            by_name.insert(definition.name.as_str(), family);
            let members: Vec<Option<Member>> = match &definition.over {
                Some(set) => {
                    let mut selected = Vec::new();
                    for member in sets.get(set)?.iter() {
                        if sets.all(&definition.filter, &member.properties)? {
                            selected.push(Some(member.clone()));
                        }
                    }
                    selected
                }
                None => vec![None],
            };
            if model.variables.len() + members.len() > max_variables {
                return Err(LimitExceeded(format!(
                    "the model needs more than maxVariables ({max_variables}) variables"
                ))
                .into());
            }
            let mut indices = Vec::with_capacity(members.len());
            for member in members {
                let empty = Properties::new();
                let properties = member.as_ref().map_or(&empty, |m| &*m.properties);
                let name = || match &member {
                    Some(member) => format!("{}{}", definition.name, describe(&member.key)),
                    None => definition.name.clone(),
                };
                let (lower, upper) = match definition.kind {
                    VariableType::Binary => (0.0, 1.0),
                    _ => (
                        match &definition.lower {
                            Some(quantity) => quantity_value(quantity, properties)
                                .with_context(|| format!("lower bound of {}", name()))?,
                            None => 0.0,
                        },
                        match &definition.upper {
                            Some(quantity) => quantity_value(quantity, properties)
                                .with_context(|| format!("upper bound of {}", name()))?,
                            None => f64::INFINITY,
                        },
                    ),
                };
                if lower > upper + EPSILON {
                    return Ok(Built::Infeasible(format!(
                        "variable {} has lower bound {lower} above upper bound {upper}",
                        name()
                    )));
                }
                if definition.kind == VariableType::Integer
                    && (lower < f64::from(i32::MIN)
                        || (upper.is_finite() && upper > f64::from(i32::MAX)))
                {
                    bail!(
                        "integer variable {} bounds must lie within the 32-bit integer range",
                        name()
                    );
                }
                indices.push(model.variables.len());
                model.variables.push(ModelVariable {
                    family,
                    member,
                    kind: definition.kind,
                    lower,
                    upper,
                });
            }
            families.push(indices);
        }

        for definition in &config.constraints {
            let terms = definition
                .terms
                .iter()
                .map(|term| term_variables(term, &by_name, &families, &model, sets))
                .collect::<Result<Vec<_>>>()?;
            let context = |key: &str| format!("constraint {}{key}", definition.name);
            let mut groups: Vec<(String, Arc<Properties>, Coefficients)> = Vec::new();
            if let Some(set) = &definition.for_each {
                for member in sets.get(set)?.iter() {
                    if !sets.all(&definition.filter, &member.properties)? {
                        continue;
                    }
                    let mut coefficients = Coefficients::new();
                    for term in &terms {
                        add_term(term, Some(&member.properties), &mut coefficients)
                            .with_context(|| context(&describe(&member.key)))?;
                    }
                    groups.push((
                        describe(&member.key),
                        member.properties.clone(),
                        coefficients,
                    ));
                }
            } else if let Some(group_by) = &definition.group_by {
                let mut grouped: BTreeMap<Vec<Key>, (Arc<Properties>, Coefficients)> =
                    BTreeMap::new();
                for term in terms.iter().filter(|term| !term.candidates.is_empty()) {
                    let scalar = model.variables[term.candidates[0].0].member.is_none();
                    if scalar {
                        continue;
                    }
                    for (variable, properties) in &term.candidates {
                        let key: Vec<Key> = group_by
                            .iter()
                            .map(|name| Key::of(properties.get(name)))
                            .collect();
                        let entry = grouped.entry(key).or_insert_with(|| {
                            (
                                Arc::new(
                                    group_by
                                        .iter()
                                        .filter_map(|name| {
                                            properties
                                                .get(name)
                                                .map(|value| (name.clone(), value.clone()))
                                        })
                                        .collect(),
                                ),
                                Coefficients::new(),
                            )
                        });
                        let coefficient = coefficient(term.term, properties, &entry.0)
                            .with_context(|| context(""))?;
                        *entry.1.entry(*variable).or_default() += coefficient;
                    }
                }
                for (key, (properties, mut coefficients)) in grouped {
                    for term in &terms {
                        if term.candidates.first().is_some_and(|(variable, _)| {
                            model.variables[*variable].member.is_none()
                        }) {
                            add_term(term, Some(&properties), &mut coefficients)
                                .with_context(|| context(&describe(&key)))?;
                        }
                    }
                    groups.push((describe(&key), properties, coefficients));
                }
            } else {
                let mut coefficients = Coefficients::new();
                for term in &terms {
                    add_term(term, None, &mut coefficients).with_context(|| context(""))?;
                }
                groups.push((String::new(), Arc::new(Properties::new()), coefficients));
            }
            for (key, properties, coefficients) in groups {
                let rhs = quantity_value(&definition.rhs, &properties)
                    .with_context(|| format!("rhs of {}", context(&key)))?;
                let coefficients: Vec<(usize, f64)> = coefficients
                    .into_iter()
                    .filter(|(_, coefficient)| *coefficient != 0.0)
                    .collect();
                if coefficients.is_empty() {
                    let holds = match definition.op {
                        RelOp::Le => 0.0 <= rhs + EPSILON,
                        RelOp::Ge => 0.0 >= rhs - EPSILON,
                        RelOp::Eq => rhs.abs() <= EPSILON,
                    };
                    if !holds {
                        return Ok(Built::Infeasible(format!(
                            "{} has no eligible variables and cannot hold",
                            context(&key)
                        )));
                    }
                    continue;
                }
                if model.rows.len() >= max_rows {
                    return Err(LimitExceeded(format!(
                        "the model needs more than maxConstraints ({max_rows}) constraints"
                    ))
                    .into());
                }
                model.rows.push(Row {
                    coefficients,
                    op: definition.op,
                    rhs,
                });
            }
        }

        for definition in &config.objectives {
            let mut coefficients = Coefficients::new();
            for term in &definition.terms {
                let term = term_variables(term, &by_name, &families, &model, sets)?;
                add_term(&term, None, &mut coefficients)
                    .with_context(|| format!("objective {}", definition.name))?;
            }
            model.objectives.push(Objective {
                name: definition.name.clone(),
                sense: definition.sense,
                coefficients: coefficients
                    .into_iter()
                    .filter(|(_, coefficient)| *coefficient != 0.0)
                    .collect(),
            });
        }
        Ok(Built::Model(model))
    }
}

fn term_variables<'a>(
    term: &'a TermDef,
    by_name: &HashMap<&str, usize>,
    families: &[Vec<usize>],
    model: &LinearModel,
    sets: &Sets,
) -> Result<TermVariables<'a>> {
    let family = *by_name
        .get(term.variable.as_str())
        .with_context(|| format!("unknown variable {}", term.variable))?;
    let mut candidates = Vec::new();
    for variable in &families[family] {
        let properties = model.variables[*variable]
            .member
            .as_ref()
            .map_or_else(|| Arc::new(Properties::new()), |m| m.properties.clone());
        if sets.all(&term.filter, &properties)? {
            candidates.push((*variable, properties));
        }
    }
    let index = (!term.matches.is_empty()).then(|| {
        let mut index: HashMap<Vec<Key>, Vec<usize>> = HashMap::new();
        for (position, (_, properties)) in candidates.iter().enumerate() {
            index
                .entry(
                    term.matches
                        .keys()
                        .map(|name| Key::of(properties.get(name)))
                        .collect(),
                )
                .or_default()
                .push(position);
        }
        index
    });
    Ok(TermVariables {
        term,
        candidates,
        index,
    })
}

/// Adds the term's variables selected for `group` (all when ungrouped).
fn add_term(
    term: &TermVariables<'_>,
    group: Option<&Arc<Properties>>,
    coefficients: &mut Coefficients,
) -> Result<()> {
    let empty = Arc::new(Properties::new());
    let group = group.unwrap_or(&empty);
    let mut add = |position: usize| -> Result<()> {
        let (variable, properties) = &term.candidates[position];
        *coefficients.entry(*variable).or_default() += coefficient(term.term, properties, group)?;
        Ok(())
    };
    match &term.index {
        Some(index) => {
            let key: Vec<Key> = term
                .term
                .matches
                .values()
                .map(|name| Key::of(group.get(name)))
                .collect();
            for position in index.get(&key).into_iter().flatten() {
                add(*position)?;
            }
        }
        None => {
            for position in 0..term.candidates.len() {
                add(position)?;
            }
        }
    }
    Ok(())
}

fn coefficient(term: &TermDef, member: &Properties, group: &Properties) -> Result<f64> {
    match &term.coefficient {
        None => Ok(1.0),
        Some(Quantity::Constant(value)) => Ok(*value),
        Some(Quantity::Property(property)) => {
            let source = match property.of {
                Some(Source::Group) => group,
                Some(Source::Member) => member,
                // Scalar variables have no member properties.
                None if member.is_empty() => group,
                None => member,
            };
            quantity_value(term.coefficient.as_ref().expect("checked"), source)
                .with_context(|| format!("coefficient of {}", term.variable))
        }
    }
}

fn quantity_value(quantity: &Quantity, properties: &Properties) -> Result<f64> {
    match quantity {
        Quantity::Constant(value) => Ok(*value),
        Quantity::Property(property) => {
            let value = match properties.get(&property.property) {
                None | Some(Value::Null) => property.default.with_context(|| {
                    format!(
                        "property {} is missing and has no default",
                        property.property
                    )
                })?,
                Some(value) => value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .with_context(|| {
                        format!("property {} is not a finite number", property.property)
                    })?,
            };
            let value = value * property.scale;
            anyhow::ensure!(
                value.is_finite(),
                "property {} scales to a non-finite value",
                property.property
            );
            Ok(value)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Optimal,
    Infeasible,
    Unbounded,
    Limit,
    Error,
    /// A precondition does not hold; the model was not solved.
    Unknown,
}

impl Status {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Optimal => "optimal",
            Self::Infeasible => "infeasible",
            Self::Unbounded => "unbounded",
            Self::Limit => "limit",
            Self::Error => "error",
            Self::Unknown => "unknown",
        }
    }

    /// Whether repeating the solve on the same model gives the same answer.
    pub(crate) fn is_final(self) -> bool {
        matches!(
            self,
            Self::Optimal | Self::Infeasible | Self::Unbounded | Self::Unknown
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Solved {
    pub status: Status,
    pub detail: String,
    /// Objective values in order; complete only when optimal.
    pub objectives: Vec<(String, f64)>,
    /// One value per model variable when optimal; otherwise empty.
    pub values: Vec<f64>,
}

impl Solved {
    fn failed(status: Status, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
            objectives: Vec::new(),
            values: Vec::new(),
        }
    }
}

impl LinearModel {
    /// Lexicographic solve: each objective is optimized with every earlier one
    /// held at its optimum (within `tolerance`, relative). Without objectives,
    /// or when `optimize` is false, the model is solved for feasibility.
    pub(crate) fn solve(&self, optimize: bool, budget: Duration, tolerance: f64) -> Solved {
        let deadline = Instant::now() + budget;
        let phases: Vec<Option<&Objective>> = if optimize && !self.objectives.is_empty() {
            self.objectives.iter().map(Some).collect()
        } else {
            vec![None]
        };
        if self.variables.is_empty() {
            return Solved {
                status: Status::Optimal,
                detail: "the model has no variables".into(),
                objectives: phases
                    .iter()
                    .flatten()
                    .map(|objective| (objective.name.clone(), 0.0))
                    .collect(),
                values: Vec::new(),
            };
        }
        let mut fixed: Vec<(&Objective, f64)> = Vec::new();
        let mut values = Vec::new();
        for phase in phases {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Solved::failed(Status::Limit, "the solve time limit was reached");
            }
            let mut problem = ProblemVariables::new();
            let handles: Vec<Variable> = self
                .variables
                .iter()
                .map(|v| {
                    let definition = match v.kind {
                        VariableType::Binary => variable().binary(),
                        VariableType::Integer => variable().integer().min(v.lower).max(v.upper),
                        VariableType::Continuous => variable().min(v.lower).max(v.upper),
                    };
                    problem.add(definition)
                })
                .collect();
            let expression = |coefficients: &[(usize, f64)]| {
                let mut expression = Expression::with_capacity(coefficients.len());
                for (variable, coefficient) in coefficients {
                    expression.add_mul(*coefficient, handles[*variable]);
                }
                expression
            };
            let objective = phase.map_or_else(Expression::default, |o| expression(&o.coefficients));
            let unsolved = match phase.map(|o| o.sense) {
                Some(Sense::Maximize) => problem.maximise(objective),
                _ => problem.minimise(objective),
            };
            let mut solver = unsolved
                .using(microlp)
                .with_time_limit(remaining.as_secs_f64());
            for row in &self.rows {
                let lhs = expression(&row.coefficients);
                solver.add_constraint(match row.op {
                    RelOp::Le => constraint::leq(lhs, row.rhs),
                    RelOp::Ge => constraint::geq(lhs, row.rhs),
                    RelOp::Eq => constraint::eq(lhs, row.rhs),
                });
            }
            for (objective, optimum) in &fixed {
                let lhs = expression(&objective.coefficients);
                let slack = tolerance * optimum.abs().max(1.0);
                solver.add_constraint(match objective.sense {
                    Sense::Minimize => constraint::leq(lhs, optimum + slack),
                    Sense::Maximize => constraint::geq(lhs, optimum - slack),
                });
            }
            let phase_name = phase.map_or("feasibility", |o| o.name.as_str());
            let solution = match solver.solve() {
                Ok(solution) if matches!(solution.status(), SolutionStatus::Optimal) => solution,
                Ok(_) => {
                    return Solved::failed(
                        Status::Limit,
                        format!("the time limit was reached before proving the {phase_name} optimum"),
                    )
                }
                Err(ResolutionError::Infeasible) if fixed.is_empty() => {
                    return Solved::failed(
                        Status::Infeasible,
                        "no assignment satisfies every constraint",
                    )
                }
                Err(ResolutionError::Infeasible) => {
                    return Solved::failed(
                        Status::Error,
                        format!("objective {phase_name} became infeasible after fixing earlier objectives; increase tolerance"),
                    )
                }
                Err(ResolutionError::Unbounded) => {
                    return Solved::failed(
                        Status::Unbounded,
                        format!("objective {phase_name} is unbounded"),
                    )
                }
                Err(error) => return Solved::failed(Status::Error, error.to_string()),
            };
            values = self
                .variables
                .iter()
                .zip(&handles)
                .map(|(variable, handle)| clean(variable.kind, solution.value(*handle)))
                .collect();
            if let Some(objective) = phase {
                let value: f64 = objective
                    .coefficients
                    .iter()
                    .map(|(variable, coefficient)| coefficient * values[*variable])
                    .sum();
                fixed.push((objective, value));
            }
        }
        Solved {
            status: Status::Optimal,
            detail: if fixed.is_empty() {
                "a feasible assignment was found".into()
            } else {
                "all objectives are optimal".into()
            },
            objectives: fixed
                .iter()
                .map(|(objective, value)| (objective.name.clone(), clean_float(*value)))
                .collect(),
            values,
        }
    }
}

fn clean(kind: VariableType, value: f64) -> f64 {
    match kind {
        VariableType::Binary | VariableType::Integer => value.round(),
        VariableType::Continuous => clean_float(value),
    }
}

/// Removes solver noise so equal models give byte-identical output.
pub(crate) fn clean_float(value: f64) -> f64 {
    let rounded = value.round();
    if (value - rounded).abs() < EPSILON {
        rounded + 0.0
    } else if value.abs() < ZERO {
        0.0
    } else {
        (value * 1e9).round() / 1e9
    }
}

/// The main solve of one dataset.
#[derive(Clone, Debug)]
pub(crate) struct MainOutcome {
    pub signature: Option<String>,
    pub provenance: Value,
    pub solved: Solved,
    pub model: Option<Arc<LinearModel>>,
    pub variables: usize,
    pub constraints: usize,
    pub members: usize,
    /// Member count of each main set; empty when the sets failed to materialize.
    pub set_sizes: BTreeMap<String, usize>,
    /// The precondition that did not hold, if any.
    pub precondition: Option<String>,
    /// `{summary: [entry…]}` while optimal with summaries configured; null otherwise.
    pub summaries: Value,
}

impl MainOutcome {
    /// An outcome for a dataset whose sets could not be materialized.
    pub(crate) fn failed(status: Status, detail: impl Into<String>) -> Self {
        Self {
            signature: None,
            provenance: Value::Null,
            solved: Solved::failed(status, detail),
            model: None,
            variables: 0,
            constraints: 0,
            members: 0,
            set_sizes: BTreeMap::new(),
            precondition: None,
            summaries: Value::Null,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ScenarioOutcome {
    pub scenario: String,
    pub property: String,
    pub value: Value,
    pub key: Key,
    pub status: Status,
    pub detail: String,
    pub objectives: Vec<(String, f64)>,
}

/// Prepared main sets, kept for the scenario phase.
pub(crate) struct Prepared {
    sets: Sets,
    pub signature: String,
    pub provenance: Value,
    /// The first precondition that does not hold, as `(name, detail)`.
    pub unmet: Option<(String, String)>,
}

/// Materializes the main sets and their signature.
pub(crate) fn prepare(config: &OptimizationConfig, records: &[Record]) -> Result<Prepared> {
    let sets = Sets::materialize(config, records, None)?;
    let unmet = sets.unmet(config)?;
    let mut hasher = Fnv::new();
    hasher.write_str(&serde_json::to_string(config)?);
    sets.digest(&mut hasher);
    let provenance = match &config.provenance {
        Some(definition) => sets
            .get(&definition.set)?
            .first()
            .map_or(Value::Null, |member| {
                Value::Object(
                    member
                        .properties
                        .iter()
                        .filter(|(name, _)| match &definition.properties {
                            Some(selected) => selected.contains(name),
                            None => !name.starts_with('$'),
                        })
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect(),
                )
            }),
        None => Value::Null,
    };
    Ok(Prepared {
        signature: hasher.finish(),
        provenance,
        sets,
        unmet,
    })
}

fn failure(error: &anyhow::Error) -> Solved {
    let status = if error.downcast_ref::<LimitExceeded>().is_some() {
        Status::Limit
    } else {
        Status::Error
    };
    Solved::failed(status, format!("{error:#}"))
}

/// Builds and solves the main model, unless a precondition does not hold.
pub(crate) fn solve_main(config: &OptimizationConfig, prepared: &Prepared) -> MainOutcome {
    let members = prepared.sets.total();
    let (solved, model) = match &prepared.unmet {
        Some((_, detail)) => (Solved::failed(Status::Unknown, detail.clone()), None),
        None => match LinearModel::build(config, &prepared.sets) {
            Ok(Built::Model(model)) => (
                model.solve(
                    true,
                    Duration::from_millis(config.time_limit_ms),
                    config.tolerance,
                ),
                Some(Arc::new(model)),
            ),
            Ok(Built::Infeasible(detail)) => (Solved::failed(Status::Infeasible, detail), None),
            Err(error) => (failure(&error), None),
        },
    };
    let (solved, summaries) = match (&model, solved.status) {
        (Some(built), Status::Optimal) if !config.summaries.is_empty() => {
            match summarize(config, &prepared.sets, built, &solved.values) {
                Ok(summaries) => (solved, summaries),
                Err(error) => (failure(&error.context("summaries")), Value::Null),
            }
        }
        _ => (solved, Value::Null),
    };
    MainOutcome {
        signature: Some(prepared.signature.clone()),
        provenance: prepared.provenance.clone(),
        variables: model.as_ref().map_or(0, |m| m.variables.len()),
        constraints: model.as_ref().map_or(0, |m| m.rows.len()),
        solved,
        model,
        members,
        set_sizes: prepared.sets.sizes(),
        precondition: prepared.unmet.as_ref().map(|(name, _)| name.clone()),
        summaries,
    }
}

/// `{summary: [entry…]}` over the nonzero variables of each summary, in member order.
fn summarize(
    config: &OptimizationConfig,
    sets: &Sets,
    model: &LinearModel,
    values: &[f64],
) -> Result<Value> {
    let mut summaries = serde_json::Map::new();
    for summary in &config.summaries {
        let family = config
            .variables
            .iter()
            .position(|variable| variable.name == summary.variable)
            .with_context(|| format!("unknown variable {}", summary.variable))?;
        let definition = &config.variables[family];
        let mut entries = Vec::new();
        for (variable, value) in model.variables.iter().zip(values) {
            if variable.family != family || (*value == 0.0 && !definition.output.include_zero) {
                continue;
            }
            let mut entry = serde_json::Map::new();
            if let Some(member) = &variable.member {
                if !sets.all(&summary.filter, &member.properties)? {
                    continue;
                }
                match summary
                    .properties
                    .as_ref()
                    .or(definition.output.properties.as_ref())
                {
                    Some(names) => {
                        for name in names {
                            if let Some(value) = member.properties.get(name) {
                                entry.insert(name.clone(), value.clone());
                            }
                        }
                    }
                    None => entry.extend(
                        member
                            .properties
                            .iter()
                            .filter(|(name, _)| !name.starts_with('$'))
                            .map(|(name, value)| (name.clone(), value.clone())),
                    ),
                }
            }
            entry.insert(
                "value".into(),
                match variable.kind {
                    VariableType::Continuous => {
                        serde_json::Number::from_f64(*value).map_or(Value::Null, Value::Number)
                    }
                    _ => Value::from(*value as i64),
                },
            );
            entries.push(Value::Object(entry));
        }
        summaries.insert(summary.name.clone(), Value::Array(entries));
    }
    Ok(Value::Object(summaries))
}

/// The distinct scenario values, in key order, with the first raw value of each.
pub(crate) fn scenario_values(
    definition: &ScenarioDef,
    prepared: &Prepared,
) -> Result<Vec<(Key, Value)>> {
    let mut values = BTreeMap::new();
    for member in prepared.sets.get(&definition.set)?.iter() {
        if let Some(value) = member.properties.get(&definition.property) {
            if !value.is_null() {
                values
                    .entry(Key::of(Some(value)))
                    .or_insert_with(|| value.clone());
            }
        }
    }
    Ok(values.into_iter().collect())
}

/// Re-solves with every member matching `value` removed from the scenario's sets.
pub(crate) fn solve_scenario(
    config: &OptimizationConfig,
    records: &[Record],
    definition: &ScenarioDef,
    key: &Key,
) -> Solved {
    let removal = Removal {
        sets: definition.remove_from.as_ref().map_or_else(
            || vec![definition.set.as_str()],
            |sets| sets.iter().map(String::as_str).collect(),
        ),
        property: definition
            .remove_property
            .as_deref()
            .unwrap_or(&definition.property),
        value: key,
    };
    let budget = Duration::from_millis(config.scenario_time_limit_ms());
    match Sets::materialize(config, records, Some(&removal))
        .and_then(|sets| LinearModel::build(config, &sets))
    {
        Ok(Built::Model(model)) => model.solve(definition.optimize, budget, config.tolerance),
        Ok(Built::Infeasible(detail)) => Solved::failed(Status::Infeasible, detail),
        Err(error) => failure(&error),
    }
}
