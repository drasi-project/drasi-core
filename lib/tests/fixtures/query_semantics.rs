// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![allow(
    dead_code,
    reason = "Shared semantic cases are selected by independent test binaries"
)]

use anyhow::{Context, Result};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{
    channels::ResultDiff,
    computation::v1::*,
    config::{
        AssignedQueryConfig, QueryConfig, QueryJoinConfig, QueryJoinKeyConfig, QueryLanguage,
    },
    Query,
};
use serde_json::{json, Value};
use std::{num::NonZeroUsize, sync::Arc};

pub struct Case {
    id: ComponentId,
    pub config: QueryConfig,
    steps: Vec<Step>,
}

pub async fn reference(case: Case) -> Result<()> {
    let mut query = ContinuousQueryTransformer::new_configured(
        case.definition()?,
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
        QueryOptions::default(),
        case.execution(),
        None,
    )
    .await?;
    let reader = query
        .output_readers()
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|output| output.snapshot())
        .cloned()
        .context("reference snapshot provider")?;
    case.exercise(&mut query, reader).await
}

struct Step {
    input: InputEnvelope,
    rows: Rows,
    changes: Option<Vec<Value>>,
}

enum Rows {
    Exact(Vec<Value>),
    NumericCount(i64, bool),
}

impl Step {
    fn rows(input: InputEnvelope, rows: Vec<Value>) -> Self {
        Self {
            input,
            rows: Rows::Exact(rows),
            changes: None,
        }
    }
}

impl Case {
    fn assigned_config(&self) -> AssignedQueryConfig {
        self.config.clone().with_assigned_id(self.id.clone())
    }

    pub fn definition(&self) -> Result<ContinuousQueryDefinition> {
        let config = self.assigned_config();
        Ok(ContinuousQueryDefinition {
            graph_id: "configured".into(),
            id: config.id,
            query: config.query.clone(),
            language: match config.query_language {
                QueryLanguage::Cypher => ComputationQueryLanguage::Cypher,
                QueryLanguage::GQL => ComputationQueryLanguage::Gql,
            },
            output_stream: StreamId::try_new("joined/out")?,
            outbox_capacity: NonZeroUsize::new(8).expect("positive retention"),
        })
    }

    pub fn execution(&self) -> QueryExecutionSettings {
        QueryExecutionSettings::from_configuration(&self.assigned_config())
            .expect("fixture execution settings")
    }

    pub async fn exercise(
        &self,
        query: &mut dyn Transformer,
        reader: Arc<dyn SnapshotProvider>,
    ) -> Result<()> {
        query.start().await?;
        let outcome = async {
            for (index, step) in self.steps.iter().enumerate() {
                let output = query.transform(step.input.clone()).await?;
                query.delivery_completed(&output).await?;
                let snapshot = reader.snapshot().await?;
                let mut actual = snapshot
                    .records()
                    .values()
                    .map(|row| {
                        Ok(serde_json::to_value(
                            QueryChangeCodec::decode_row(row)?.values,
                        )?)
                    })
                    .collect::<Result<Vec<Value>>>()?;
                actual.sort_by_key(Value::to_string);
                match &step.rows {
                    Rows::Exact(expected) => {
                        let mut expected = expected.clone();
                        expected.sort_by_key(Value::to_string);
                        anyhow::ensure!(
                            actual == expected,
                            "{} step {index}: expected {expected:?}, got {actual:?}",
                            self.config.name
                        );
                    }
                    Rows::NumericCount(count, equal) => {
                        anyhow::ensure!(actual.len() == 1, "expected one aggregate row");
                        anyhow::ensure!(
                            actual[0]["count"].as_f64() == Some(*count as f64)
                                && actual[0]["expected"] == 2
                                && actual[0]["equal"] == *equal
                                && actual[0]["unequal"] == !equal,
                            "integer/list-size comparison changed: {actual:?}"
                        );
                    }
                }
                if let Some(expected) = &step.changes {
                    anyhow::ensure!(
                        output.len() == 1,
                        "one input batch must produce one envelope"
                    );
                    let result = QueryChangeCodec::to_legacy_result(&output[0].envelope)?;
                    let actual = result
                        .results
                        .into_iter()
                        .map(|change| match change {
                            ResultDiff::Add { data, .. } => Ok(json!({"add": data})),
                            ResultDiff::Update { before, after, .. } => {
                                Ok(json!({"before": before, "after": after}))
                            }
                            ResultDiff::Delete { data, .. } => Ok(json!({"delete": data})),
                            other => anyhow::bail!("unexpected row change: {other:?}"),
                        })
                        .collect::<Result<Vec<_>>>()?;
                    anyhow::ensure!(
                        &actual == expected,
                        "{} step {index}: result changes differ: {actual:?}",
                        self.config.name
                    );
                }
            }
            Ok(())
        }
        .await;
        let cleanup = query.stop().await;
        outcome.with_context(|| format!("query semantics case {}", self.config.name))?;
        cleanup
    }
}

fn node(source: &str, label: &str, properties: Value, sequence: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, source),
            labels: Arc::from([Arc::from(label)]),
            effective_from: sequence * 1000,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

fn input(source: &str, change: SourceChange, sequence: u64) -> Result<InputEnvelope> {
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_change(
            change,
            StreamId::try_new(format!("{source}/out"))?,
            sequence,
            None,
        )?,
    })
}

fn insert(source: &str, label: &str, properties: Value) -> Result<InputEnvelope> {
    input(
        source,
        SourceChange::Insert {
            element: node(source, label, properties, 1),
        },
        1,
    )
}

fn delete(source: &str, sequence: u64) -> Result<InputEnvelope> {
    input(
        source,
        SourceChange::Delete {
            metadata: node(source, "unused", json!({}), sequence)
                .get_metadata()
                .clone(),
        },
        sequence,
    )
}

pub fn disconnected(language: ComputationQueryLanguage) -> Result<Case> {
    let mut config = Query::cypher(drasi_core_types::ComponentName::try_new("inventory-context").expect("fixture component name"))
        .query("MATCH (w:workload_requirements) OPTIONAL MATCH (a:AppliedPlan) RETURN w.workload_id AS workload_id, a.fleet_id AS fleet_id")
        .build()?;
    config.query_language = match language {
        ComputationQueryLanguage::Cypher => QueryLanguage::Cypher,
        ComputationQueryLanguage::Gql => QueryLanguage::GQL,
    };
    Ok(Case {
        id: ComponentId::generate(),
        config,
        steps: vec![
            Step::rows(
                insert(
                    "workload",
                    "workload_requirements",
                    json!({"workload_id":"workload"}),
                )?,
                vec![json!({"workload_id":"workload","fleet_id":null})],
            ),
            Step::rows(
                insert("plan", "AppliedPlan", json!({"fleet_id":"demo"}))?,
                vec![json!({"workload_id":"workload","fleet_id":"demo"})],
            ),
            Step::rows(
                input(
                    "plan",
                    SourceChange::update(node(
                        "plan",
                        "AppliedPlan",
                        json!({"fleet_id":"updated"}),
                        2,
                    )),
                    2,
                )?,
                vec![json!({"workload_id":"workload","fleet_id":"updated"})],
            ),
            Step::rows(
                delete("plan", 3)?,
                vec![json!({"workload_id":"workload","fleet_id":null})],
            ),
            Step::rows(delete("workload", 2)?, vec![]),
        ],
    })
}

pub fn optional_list() -> Result<Case> {
    Ok(Case {
        id: ComponentId::generate(),
        config: Query::cypher(drasi_core_types::ComponentName::try_new("optional-list").expect("fixture component name"))
            .query("MATCH (w:Workload) OPTIONAL MATCH (p:Plan) RETURN w.id AS id, [d IN p.assignments WHERE d = 'gpu'] AS matches, size([d IN p.assignments WHERE d = 'gpu']) AS matched").build()?,
        steps: vec![
            Step::rows(insert("workload", "Workload", json!({"id":"workload"}))?,
                vec![json!({"id":"workload","matches":null,"matched":null})]),
            Step::rows(insert("plan", "Plan", json!({"assignments":["gpu","other"]}))?,
                vec![json!({"id":"workload","matches":["gpu"],"matched":1})]),
            Step::rows(input("plan", SourceChange::update(node("plan", "Plan", json!({"assignments":[]}), 2)), 2)?,
                vec![json!({"id":"workload","matches":[],"matched":0})]),
            Step::rows(delete("plan", 3)?, vec![json!({"id":"workload","matches":null,"matched":null})]),
        ],
    })
}

pub fn assignment_maps() -> Result<Case> {
    let assignment = json!({"gpu_id":"gpu-1","workload_revision":"1","memory_mib":4096});
    Ok(Case {
        id: ComponentId::generate(),
        config: Query::cypher(drasi_core_types::ComponentName::try_new("assignment-maps").expect("fixture component name"))
            .query("MATCH (w:workload_requirements) OPTIONAL MATCH (p:gpu_placements) RETURN w.workload_id AS workload_id, [d IN p.assignments WHERE d = w.assignment] AS matching").build()?,
        steps: vec![
            Step::rows(insert("workload", "workload_requirements",
                json!({"workload_id":"workload","assignment":assignment.clone()}))?,
                vec![json!({"workload_id":"workload","matching":null})]),
            Step::rows(insert("plan", "gpu_placements", json!({"assignments":[assignment.clone()]}))?,
                vec![json!({"workload_id":"workload","matching":[assignment]})]),
        ],
    })
}

pub fn numeric_count() -> Result<Case> {
    let mut steps = Vec::new();
    for (replicas, equal, sequence) in [(2, true, 1), (3, false, 2)] {
        let element = node(
            "workload",
            "workload_requirements",
            json!({"replicas":replicas}),
            sequence,
        );
        let change = if sequence == 1 {
            SourceChange::Insert { element }
        } else {
            SourceChange::update(element)
        };
        steps.push(Step {
            input: input("workload", change, sequence)?,
            rows: Rows::NumericCount(replicas, equal),
            changes: None,
        });
    }
    Ok(Case {
        id: ComponentId::generate(),
        config: Query::cypher(drasi_core_types::ComponentName::try_new("numeric-count").expect("fixture component name"))
            .query("MATCH (w:workload_requirements) WITH sum(w.replicas) AS count RETURN count, size([1,2]) AS expected, count = size([1,2]) AS equal, count <> size([1,2]) AS unequal").build()?,
        steps,
    })
}

pub fn optional_join() -> Result<Case> {
    Ok(Case {
        id: ComponentId::generate(),
        config: Query::cypher(drasi_core_types::ComponentName::try_new("decision").expect("fixture component name"))
            .query("MATCH (d:DecisionExplanation) OPTIONAL MATCH (d)-[:DECISION_WRITE]->(w:PlanWriteOutcome) RETURN d.decision_id AS decision_id, w.outcome AS outcome")
            .from_input_by_id(ComponentId::generate()).from_input_by_id(ComponentId::generate())
            .with_joins(vec![QueryJoinConfig {
                id: "DECISION_WRITE".into(),
                keys: ["DecisionExplanation", "PlanWriteOutcome"].into_iter()
                    .map(|label| QueryJoinKeyConfig { label: label.into(), property: "decision_id".into() }).collect(),
            }]).build()?,
        steps: vec![
            Step::rows(insert("decisions", "DecisionExplanation", json!({"decision_id":"decision"}))?,
                vec![json!({"decision_id":"decision","outcome":null})]),
            Step::rows(insert("writes", "PlanWriteOutcome", json!({"decision_id":"decision","outcome":"rejected"}))?,
                vec![json!({"decision_id":"decision","outcome":"rejected"})]),
        ],
    })
}

pub fn canonical_replacement() -> Result<Case> {
    let before = json!({"name":"Alice","extra":"old"});
    let after = json!({"name":"Bob","extra":null});
    let stream = StreamId::try_new("people/out")?;
    let replacement = InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: ChangeEnvelope::new(
            emission_id(&stream, 2)?,
            ChangeSet::try_new(
                ChangeSetId::try_new("people/out", bytes::Bytes::from_static(b"replace"))?,
                GraphChangeCodec::schema().descriptor().clone(),
                vec![ChangeOperation::Updated {
                    ordinal: 0,
                    before: None,
                    semantics: UpdateSemantics::Replace,
                    after: GraphChangeCodec::element_record(&ElementImage::try_from(node(
                        "people",
                        "Person",
                        json!({"name":"Bob"}),
                        2,
                    ))?)?,
                }],
            )?,
            SystemMetadata::new(stream, 2),
        ),
    };
    Ok(Case {
        id: ComponentId::generate(),
        config: Query::cypher(
            drasi_core_types::ComponentName::try_new("replacement")
                .expect("fixture component name"),
        )
        .query("MATCH (n:Person) RETURN n.name AS name, n.extra AS extra")
        .build()?,
        steps: vec![
            Step::rows(
                insert("people", "Person", before.clone())?,
                vec![before.clone()],
            ),
            Step {
                input: replacement,
                rows: Rows::Exact(vec![after.clone()]),
                changes: Some(vec![json!({"before": before, "after": after})]),
            },
        ],
    })
}

pub fn ordered_batch() -> Result<Case> {
    let image = |sequence, properties| {
        ElementImage::try_from(node("people", "Person", properties, sequence))
    };
    let operations = vec![
        ChangeOperation::Added {
            ordinal: 0,
            after: GraphChangeCodec::element_record(&image(
                1,
                json!({"name":"Alice","extra":"old"}),
            )?)?,
        },
        ChangeOperation::Updated {
            ordinal: 1,
            before: None,
            semantics: UpdateSemantics::Patch,
            after: GraphChangeCodec::patch_record(&ElementPatch::try_new(
                image(2, json!({"name":"Bob"}))?,
                ["extra".to_owned()].into(),
            )?)?,
        },
        ChangeOperation::Updated {
            ordinal: 2,
            before: None,
            semantics: UpdateSemantics::Patch,
            after: GraphChangeCodec::patch_record(&ElementPatch::try_new(
                image(3, json!({"extra":null}))?,
                Default::default(),
            )?)?,
        },
        ChangeOperation::Updated {
            ordinal: 3,
            before: None,
            semantics: UpdateSemantics::Replace,
            after: GraphChangeCodec::element_record(&image(
                4,
                json!({"name":"Carol","extra":"new"}),
            )?)?,
        },
    ];
    let stream = StreamId::try_new("people/out")?;
    Ok(Case {
        id: ComponentId::generate(),
        config: Query::cypher(
            drasi_core_types::ComponentName::try_new("ordered-batch")
                .expect("fixture component name"),
        )
        .query("MATCH (n:Person) RETURN n.name AS name, n.extra AS extra")
        .build()?,
        steps: vec![Step {
            input: InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: ChangeEnvelope::new(
                    emission_id(&stream, 1)?,
                    ChangeSet::try_new(
                        ChangeSetId::try_new("people/out", bytes::Bytes::from_static(b"batch"))?,
                        GraphChangeCodec::schema().descriptor().clone(),
                        operations,
                    )?,
                    SystemMetadata::new(stream, 1),
                ),
            },
            rows: Rows::Exact(vec![json!({"name":"Carol","extra":"new"})]),
            changes: Some(vec![
                json!({"add":{"name":"Alice","extra":"old"}}),
                json!({"before":{"name":"Alice","extra":"old"},"after":{"name":"Bob","extra":null}}),
                json!({"before":{"name":"Bob","extra":null},"after":{"name":"Carol","extra":"new"}}),
            ]),
        }],
    })
}
