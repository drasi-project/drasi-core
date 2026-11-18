// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Output rows: solution, status and scenario elements, incremental diffing
//! against what was emitted, and the published snapshot.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ElementImage, GraphChangeCodec, GraphProducerIdentity, OutputCursor, OutputReadError,
    OutputSnapshot, SnapshotProvider,
};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock, Weak},
};

use crate::{
    config::{OptimizationConfig, VariableType},
    model::{clean_float, Key, MainOutcome, Properties, ScenarioOutcome, Status},
};

/// The id, label and properties of one output element.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OutRow {
    pub label: String,
    pub properties: Properties,
}

pub(crate) type Rows = BTreeMap<String, OutRow>;

/// Percent-escapes the id separator.
fn escape(part: &str) -> String {
    part.replace('%', "%25").replace('/', "%2F")
}

fn id(prefix: &str, key: &[Key]) -> String {
    let mut id = escape(prefix);
    for part in key {
        id.push('/');
        id.push_str(&escape(&part.text()));
    }
    id
}

fn number(value: f64) -> Value {
    serde_json::Number::from_f64(clean_float(value)).map_or(Value::Null, Value::Number)
}

fn objectives(values: &[(String, f64)]) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(name, value)| (name.clone(), number(*value)))
            .collect::<Map<_, _>>(),
    )
}

/// Solution rows of an optimal outcome; none otherwise.
pub(crate) fn solution_rows(config: &OptimizationConfig, outcome: &MainOutcome) -> Rows {
    let mut rows = Rows::new();
    let (Status::Optimal, Some(model)) = (outcome.solved.status, &outcome.model) else {
        return rows;
    };
    for (variable, value) in model.variables.iter().zip(&outcome.solved.values) {
        let definition = &config.variables[variable.family];
        let output = &definition.output;
        if !output.emit {
            continue;
        }
        let value = match variable.kind {
            VariableType::Binary | VariableType::Integer => {
                if *value == 0.0 && !output.include_zero {
                    continue;
                }
                Value::from(*value as i64)
            }
            VariableType::Continuous => {
                if *value == 0.0 && !output.include_zero {
                    continue;
                }
                number(*value)
            }
        };
        let mut properties = Properties::new();
        let id = match &variable.member {
            Some(member) => {
                match &output.properties {
                    Some(names) => {
                        for name in names {
                            if let Some(value) = member.properties.get(name) {
                                properties.insert(name.clone(), value.clone());
                            }
                        }
                    }
                    None => properties.extend(
                        member
                            .properties
                            .iter()
                            .filter(|(name, _)| !name.starts_with('$'))
                            .map(|(name, value)| (name.clone(), value.clone())),
                    ),
                }
                id(&definition.name, &member.key)
            }
            None => escape(&definition.name),
        };
        properties.insert("variable".into(), Value::from(definition.name.as_str()));
        properties.insert("value".into(), value);
        rows.insert(
            id,
            OutRow {
                label: output
                    .label
                    .clone()
                    .unwrap_or_else(|| definition.name.clone()),
                properties,
            },
        );
    }
    rows
}

pub(crate) fn status_row(config: &OptimizationConfig, outcome: &MainOutcome) -> (String, OutRow) {
    let solved = &outcome.solved;
    let mut properties = Properties::new();
    properties.insert("status".into(), Value::from(solved.status.as_str()));
    properties.insert("detail".into(), Value::from(solved.detail.as_str()));
    properties.insert(
        "objective".into(),
        solved
            .objectives
            .last()
            .map_or(Value::Null, |(_, value)| number(*value)),
    );
    properties.insert("objectives".into(), objectives(&solved.objectives));
    properties.insert(
        "signature".into(),
        outcome
            .signature
            .as_deref()
            .map_or(Value::Null, Value::from),
    );
    properties.insert("variables".into(), Value::from(outcome.variables));
    properties.insert("constraints".into(), Value::from(outcome.constraints));
    properties.insert("members".into(), Value::from(outcome.members));
    if config.provenance.is_some() {
        properties.insert("provenance".into(), outcome.provenance.clone());
    }
    (
        "status".into(),
        OutRow {
            label: config.status_label.clone(),
            properties,
        },
    )
}

pub(crate) fn scenario_rows(
    config: &OptimizationConfig,
    outcomes: &[ScenarioOutcome],
    signature: Option<&str>,
    provenance: &Value,
) -> Rows {
    outcomes
        .iter()
        .map(|outcome| {
            let mut properties = Properties::new();
            properties.insert("scenario".into(), Value::from(outcome.scenario.as_str()));
            properties.insert("property".into(), Value::from(outcome.property.as_str()));
            properties.insert("value".into(), outcome.value.clone());
            properties.insert("status".into(), Value::from(outcome.status.as_str()));
            properties.insert(
                "feasible".into(),
                match outcome.status {
                    Status::Optimal | Status::Unbounded => Value::Bool(true),
                    Status::Infeasible => Value::Bool(false),
                    Status::Limit | Status::Error => Value::Null,
                },
            );
            properties.insert("detail".into(), Value::from(outcome.detail.as_str()));
            properties.insert("objectives".into(), objectives(&outcome.objectives));
            properties.insert(
                "signature".into(),
                signature.map_or(Value::Null, Value::from),
            );
            if config.provenance.is_some() {
                properties.insert("provenance".into(), provenance.clone());
            }
            (
                id(
                    "scenario",
                    &[
                        Key::String(Arc::from(outcome.scenario.as_str())),
                        outcome.key.clone(),
                    ],
                ),
                OutRow {
                    label: config.scenario_label.clone(),
                    properties,
                },
            )
        })
        .collect()
}

/// An emitted element and the row it was built from.
#[derive(Clone, Debug)]
pub(crate) struct Emitted {
    namespace: String,
    row: OutRow,
    pub element: Element,
}

pub(crate) type EmittedRows = BTreeMap<String, Emitted>;

fn element(namespace: &str, id: &str, row: &OutRow, effective_from: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(namespace, id),
            labels: vec![Arc::from(row.label.as_str())].into(),
            effective_from,
        },
        properties: ElementPropertyMap::from(Value::Object(
            row.properties
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        )),
    }
}

/// The changes turning `emitted` into `desired`: retractions first, then
/// inserts and replacements, each in id order.
pub(crate) fn diff(
    emitted: &EmittedRows,
    desired: &Rows,
    namespace: &str,
    effective_from: u64,
) -> (EmittedRows, Vec<SourceChange>) {
    let mut deletes = Vec::new();
    let mut upserts = Vec::new();
    let mut next = EmittedRows::new();
    for (id, previous) in emitted {
        let kept = desired
            .get(id)
            .is_some_and(|_| previous.namespace == namespace);
        if !kept {
            let mut metadata = previous.element.get_metadata().clone();
            metadata.effective_from = effective_from;
            deletes.push(SourceChange::Delete { metadata });
        }
    }
    for (id, row) in desired {
        let previous = emitted
            .get(id)
            .filter(|previous| previous.namespace == namespace);
        match previous {
            Some(previous) if previous.row == *row => {
                next.insert(id.clone(), previous.clone());
            }
            _ => {
                let element = element(namespace, id, row, effective_from);
                upserts.push(match previous {
                    Some(previous) if previous.row.label == row.label => SourceChange::Replace {
                        element: element.clone(),
                    },
                    Some(previous) => {
                        let mut metadata = previous.element.get_metadata().clone();
                        metadata.effective_from = effective_from;
                        deletes.push(SourceChange::Delete { metadata });
                        SourceChange::Insert {
                            element: element.clone(),
                        }
                    }
                    None => SourceChange::Insert {
                        element: element.clone(),
                    },
                });
                next.insert(
                    id.clone(),
                    Emitted {
                        namespace: namespace.to_owned(),
                        row: row.clone(),
                        element,
                    },
                );
            }
        }
    }
    deletes.extend(upserts);
    (next, deletes)
}

struct Published {
    cursor: OutputCursor,
    rows: Vec<ElementImage>,
    running: bool,
    retired: bool,
}

/// The `out` snapshot: the elements emitted so far, at the last sequence.
pub(crate) struct Output {
    published: Arc<RwLock<Published>>,
    reader: Arc<Reader>,
}

fn poisoned() -> anyhow::Error {
    anyhow::anyhow!("optimization snapshot state poisoned")
}

impl Output {
    pub(crate) fn new(identity: GraphProducerIdentity) -> Result<Self> {
        let published = Arc::new(RwLock::new(Published {
            cursor: OutputCursor::try_new(identity, 0)?,
            rows: Vec::new(),
            running: false,
            retired: false,
        }));
        let reader = Arc::new(Reader(Arc::downgrade(&published)));
        Ok(Self { published, reader })
    }

    pub(crate) fn reader(&self) -> Arc<dyn SnapshotProvider> {
        self.reader.clone()
    }

    pub(crate) fn set_running(&self, running: bool) -> Result<()> {
        let mut published = self.published.write().map_err(|_| poisoned())?;
        anyhow::ensure!(
            !published.retired,
            "optimization state has been deprovisioned"
        );
        published.running = running;
        Ok(())
    }

    pub(crate) fn publish(&self, rows: &EmittedRows, sequence: u64) -> Result<()> {
        let images = rows
            .values()
            .map(|row| ElementImage::try_from(row.element.clone()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut published = self.published.write().map_err(|_| poisoned())?;
        anyhow::ensure!(
            published.running && !published.retired,
            "optimization is not accepting output"
        );
        published.rows = images;
        published.cursor = published.cursor.at_sequence(sequence);
        Ok(())
    }

    pub(crate) fn retire(&self) -> Result<()> {
        let mut published = self.published.write().map_err(|_| poisoned())?;
        anyhow::ensure!(
            !published.running,
            "stop the optimization before removing its state"
        );
        published.retired = true;
        published.rows.clear();
        Ok(())
    }
}

struct Reader(Weak<RwLock<Published>>);

#[async_trait]
impl SnapshotProvider for Reader {
    async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
        let published = self.0.upgrade().ok_or(OutputReadError::Retired)?;
        let (cursor, rows) = {
            let published = published
                .read()
                .map_err(|_| OutputReadError::Failed(poisoned()))?;
            if published.retired {
                return Err(OutputReadError::Retired);
            }
            if !published.running {
                return Err(OutputReadError::NotReady(
                    "optimization is not running".into(),
                ));
            }
            (published.cursor.clone(), published.rows.clone())
        };
        let records = rows
            .iter()
            .map(|image| {
                let record = GraphChangeCodec::element_record(image)?;
                Ok((record.identity().clone(), record))
            })
            .collect::<Result<_>>()
            .map_err(OutputReadError::Failed)?;
        let dataset = cursor.producer().component_id().to_string();
        OutputSnapshot::try_new(
            cursor,
            dataset,
            GraphChangeCodec::schema().descriptor().clone(),
            records,
        )
    }
}
