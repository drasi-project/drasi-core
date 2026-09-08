// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Utc};
use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        variable_value::VariableValue,
    },
    models::SourceChange,
};

use crate::{
    channels::{QueryResult, ResultDiff, SourceEvent, SourceEventWrapper},
    profiling::ProfilingMetadata,
};

use super::{
    canonical::{encode_query_variables, encode_source_change, CanonicalEncodingError},
    graph_change_schema, query_result_schema, AddedRecord, ChangeContractError, ChangeEnvelope,
    ChangeSet, ChangeSetId, DeletedRecord, GraphRecord, RecordData, RecordIdentity,
    StableIdBuilder, SystemMetadata, SystemMetadataExtensions, UpdateMetadata, UpdateSemantics,
    UpdatedRecord,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum ChangeAdapterError {
    #[error(transparent)]
    Contract(#[from] ChangeContractError),
    #[error("source control events cannot be represented as graph changes")]
    UnsupportedSourceControl,
    #[error("expected a {expected} change envelope")]
    WrongSchema { expected: &'static str },
    #[error("graph envelope must contain exactly one supported source change")]
    InvalidGraphEnvelope,
    #[error("query envelope is missing its query identity")]
    MissingQueryId,
    #[error("query envelope is missing its result sequence")]
    MissingQuerySequence,
    #[error("query update at ordinal {ordinal} is missing its before image")]
    MissingQueryBeforeImage { ordinal: usize },
    #[error("record at ordinal {ordinal} is incompatible with the boundary codec")]
    IncompatibleRecord { ordinal: usize },
    #[error(transparent)]
    CanonicalEncoding(#[from] CanonicalEncodingError),
}

#[derive(Debug, Clone)]
pub(crate) struct QueryEnvelopeMetadata {
    query_id: Arc<str>,
    source_id: Option<Arc<str>>,
    sequence: u64,
    timestamp: DateTime<Utc>,
    metadata: HashMap<String, serde_json::Value>,
    profiling: Option<ProfilingMetadata>,
    extensions: SystemMetadataExtensions,
}

impl QueryEnvelopeMetadata {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        query_id: impl Into<Arc<str>>,
        source_id: Option<Arc<str>>,
        sequence: u64,
        timestamp: DateTime<Utc>,
        metadata: HashMap<String, serde_json::Value>,
        profiling: Option<ProfilingMetadata>,
    ) -> Self {
        Self {
            query_id: query_id.into(),
            source_id,
            sequence,
            timestamp,
            metadata,
            profiling,
            extensions: SystemMetadataExtensions::default(),
        }
    }

    pub(crate) fn with_extensions(mut self, extensions: SystemMetadataExtensions) -> Self {
        self.extensions = extensions;
        self
    }
}

pub(crate) fn source_event_to_envelope(
    wrapper: &SourceEventWrapper,
    extensions: SystemMetadataExtensions,
) -> Result<ChangeEnvelope, ChangeAdapterError> {
    let SourceEvent::Change(change) = &wrapper.event else {
        return Err(ChangeAdapterError::UnsupportedSourceControl);
    };

    let (added, updated, deleted) = match change {
        SourceChange::Insert { element } => (
            vec![AddedRecord::new(
                0,
                RecordIdentity::GraphElement(element.get_reference().clone()),
                RecordData::Graph(GraphRecord::Element(Arc::new(element.clone()))),
            )],
            vec![],
            vec![],
        ),
        SourceChange::Update { element } => (
            vec![],
            vec![UpdatedRecord::new(
                0,
                RecordIdentity::GraphElement(element.get_reference().clone()),
                None,
                RecordData::Graph(GraphRecord::Element(Arc::new(element.clone()))),
                UpdateSemantics::Patch,
                UpdateMetadata::None,
            )],
            vec![],
        ),
        SourceChange::Delete { metadata } => (
            vec![],
            vec![],
            vec![DeletedRecord::new(
                0,
                RecordIdentity::GraphElement(metadata.reference.clone()),
                Some(RecordData::Graph(GraphRecord::Metadata(Arc::new(
                    metadata.clone(),
                )))),
            )],
        ),
        SourceChange::Future { future_ref } => (
            vec![],
            vec![UpdatedRecord::new(
                0,
                RecordIdentity::GraphElement(future_ref.element_ref.clone()),
                None,
                RecordData::Graph(GraphRecord::Future(Arc::new(future_ref.clone()))),
                UpdateSemantics::Patch,
                UpdateMetadata::None,
            )],
            vec![],
        ),
    };

    let encoded_change = encode_source_change(change)?;
    let mut identity = StableIdBuilder::new("drasi.internal.graph-change-set/v1");
    identity.string("source-id", &wrapper.source_id);
    identity.optional_u64("sequence", wrapper.sequence);
    identity.optional_bytes(
        "source-position",
        wrapper.source_position.as_ref().map(bytes::Bytes::as_ref),
    );
    identity.bytes("source-change", &encoded_change);
    let change_set = ChangeSet::try_new(
        ChangeSetId::new(identity.finish()),
        graph_change_schema(),
        added,
        updated,
        deleted,
    )?;
    let system = SystemMetadata::new(
        Some(Arc::from(wrapper.source_id.as_str())),
        None,
        wrapper.sequence,
        wrapper.source_position.clone(),
        wrapper.timestamp,
        wrapper.profiling.clone(),
        extensions,
    );

    Ok(ChangeEnvelope::new(change_set, system, HashMap::new()))
}

pub(crate) fn source_event_from_envelope(
    envelope: &ChangeEnvelope,
) -> Result<SourceEventWrapper, ChangeAdapterError> {
    if envelope.change_set().schema().kind() != super::ChangeSchemaKind::GraphChange {
        return Err(ChangeAdapterError::WrongSchema {
            expected: "graph-change",
        });
    }

    let change_set = envelope.change_set();
    let change = match (
        change_set.added().first(),
        change_set.updated().first(),
        change_set.deleted().first(),
        change_set.added().len() + change_set.updated().len() + change_set.deleted().len(),
    ) {
        (Some(added), None, None, 1) => match added.after() {
            RecordData::Graph(GraphRecord::Element(element)) => SourceChange::Insert {
                element: element.as_ref().clone(),
            },
            _ => return Err(ChangeAdapterError::InvalidGraphEnvelope),
        },
        (None, Some(updated), None, 1) => match updated.after() {
            RecordData::Graph(GraphRecord::Element(element))
                if updated.semantics() == UpdateSemantics::Patch =>
            {
                SourceChange::Update {
                    element: element.as_ref().clone(),
                }
            }
            RecordData::Graph(GraphRecord::Future(future))
                if updated.semantics() == UpdateSemantics::Patch =>
            {
                SourceChange::Future {
                    future_ref: future.as_ref().clone(),
                }
            }
            _ => return Err(ChangeAdapterError::InvalidGraphEnvelope),
        },
        (None, None, Some(deleted), 1) => match deleted.before() {
            Some(RecordData::Graph(GraphRecord::Metadata(metadata))) => SourceChange::Delete {
                metadata: metadata.as_ref().clone(),
            },
            Some(RecordData::Graph(GraphRecord::Element(element))) => SourceChange::Delete {
                metadata: element.get_metadata().clone(),
            },
            _ => return Err(ChangeAdapterError::InvalidGraphEnvelope),
        },
        _ => return Err(ChangeAdapterError::InvalidGraphEnvelope),
    };

    let system = envelope.system();
    let source_id = system
        .source_id()
        .ok_or(ChangeAdapterError::InvalidGraphEnvelope)?;

    Ok(SourceEventWrapper {
        source_id: source_id.to_string(),
        event: SourceEvent::Change(change),
        timestamp: system.timestamp(),
        profiling: system.profiling().cloned(),
        sequence: system.sequence(),
        source_position: system.source_position().cloned(),
    })
}

pub(crate) fn query_evaluation_to_envelope(
    results: &[QueryPartEvaluationContext],
    metadata: QueryEnvelopeMetadata,
) -> Result<Option<ChangeEnvelope>, ChangeAdapterError> {
    let mut added = Vec::new();
    let mut updated = Vec::new();
    let mut deleted = Vec::new();
    let mut identity = StableIdBuilder::new("drasi.internal.query-change-set/v2");
    identity.string("query-id", &metadata.query_id);
    identity.u64("sequence", metadata.sequence);
    identity.u64(
        "schema-fingerprint",
        query_result_schema().fingerprint().value(),
    );
    identity.optional_u64("graph-epoch", metadata.extensions.graph_epoch());
    identity.optional_u64("config-epoch", metadata.extensions.config_epoch());
    identity.u64(
        "result-count",
        results
            .iter()
            .filter(|result| !matches!(result, QueryPartEvaluationContext::Noop))
            .count() as u64,
    );

    for (ordinal, result) in results.iter().enumerate() {
        match result {
            QueryPartEvaluationContext::Adding {
                after,
                row_signature,
            } => {
                identity.u64("result-ordinal", ordinal as u64);
                identity.string("result-kind", "add");
                identity.u64("row-signature", *row_signature);
                hash_query_variables(&mut identity, "after", after)?;
                added.push(AddedRecord::new(
                    ordinal,
                    RecordIdentity::QueryRow(*row_signature),
                    RecordData::QueryRow(Arc::new(after.clone())),
                ));
            }
            QueryPartEvaluationContext::Updating {
                before,
                after,
                row_signature,
            } => {
                identity.u64("result-ordinal", ordinal as u64);
                identity.string("result-kind", "update");
                identity.u64("row-signature", *row_signature);
                hash_query_variables(&mut identity, "before", before)?;
                hash_query_variables(&mut identity, "after", after)?;
                updated.push(UpdatedRecord::new(
                    ordinal,
                    RecordIdentity::QueryRow(*row_signature),
                    Some(RecordData::QueryRow(Arc::new(before.clone()))),
                    RecordData::QueryRow(Arc::new(after.clone())),
                    UpdateSemantics::Replace,
                    UpdateMetadata::QueryUpdate {
                        grouping_keys: None,
                    },
                ));
            }
            QueryPartEvaluationContext::Removing {
                before,
                row_signature,
            } => {
                identity.u64("result-ordinal", ordinal as u64);
                identity.string("result-kind", "delete");
                identity.u64("row-signature", *row_signature);
                hash_query_variables(&mut identity, "before", before)?;
                deleted.push(DeletedRecord::new(
                    ordinal,
                    RecordIdentity::QueryRow(*row_signature),
                    Some(RecordData::QueryRow(Arc::new(before.clone()))),
                ));
            }
            QueryPartEvaluationContext::Aggregation {
                before,
                after,
                grouping_keys,
                default_before,
                default_after,
                row_signature,
            } => {
                identity.u64("result-ordinal", ordinal as u64);
                identity.string("result-kind", "aggregation");
                identity.u64("row-signature", *row_signature);
                identity.bool("before-present", before.is_some());
                if let Some(before) = before {
                    hash_query_variables(&mut identity, "before", before)?;
                }
                hash_query_variables(&mut identity, "after", after)?;
                identity.u64("grouping-key-count", grouping_keys.len() as u64);
                for grouping_key in grouping_keys {
                    identity.string("grouping-key", grouping_key);
                }
                identity.bool("default-before", *default_before);
                identity.bool("default-after", *default_after);
                updated.push(UpdatedRecord::new(
                    ordinal,
                    RecordIdentity::QueryRow(*row_signature),
                    before
                        .as_ref()
                        .map(|before| RecordData::QueryRow(Arc::new(before.clone()))),
                    RecordData::QueryRow(Arc::new(after.clone())),
                    UpdateSemantics::Replace,
                    UpdateMetadata::QueryAggregation {
                        grouping_keys: grouping_keys
                            .iter()
                            .map(|key| Arc::from(key.as_str()))
                            .collect::<Vec<_>>()
                            .into(),
                        default_before: *default_before,
                        default_after: *default_after,
                    },
                ));
            }
            QueryPartEvaluationContext::Noop => {}
        }
    }

    if added.is_empty() && updated.is_empty() && deleted.is_empty() {
        return Ok(None);
    }

    let change_set = ChangeSet::try_new(
        ChangeSetId::new(identity.finish()),
        query_result_schema(),
        added,
        updated,
        deleted,
    )?;
    let system = SystemMetadata::new(
        metadata.source_id,
        Some(metadata.query_id),
        Some(metadata.sequence),
        None,
        metadata.timestamp,
        metadata.profiling,
        metadata.extensions,
    );

    Ok(Some(ChangeEnvelope::new(
        change_set,
        system,
        metadata.metadata,
    )))
}

fn hash_query_variables(
    identity: &mut StableIdBuilder,
    section: &str,
    variables: &QueryVariables,
) -> Result<(), CanonicalEncodingError> {
    identity.bytes(section, &encode_query_variables(variables)?);
    Ok(())
}

pub(crate) fn query_result_from_envelope(
    envelope: &ChangeEnvelope,
) -> Result<QueryResult, ChangeAdapterError> {
    if envelope.change_set().schema().kind() != super::ChangeSchemaKind::QueryResult {
        return Err(ChangeAdapterError::WrongSchema {
            expected: "query-result",
        });
    }

    let mut ordered = Vec::new();
    for added in envelope.change_set().added() {
        let RecordIdentity::QueryRow(row_signature) = added.identity() else {
            return Err(ChangeAdapterError::IncompatibleRecord {
                ordinal: added.ordinal(),
            });
        };
        let RecordData::QueryRow(after) = added.after() else {
            return Err(ChangeAdapterError::IncompatibleRecord {
                ordinal: added.ordinal(),
            });
        };
        ordered.push((
            added.ordinal(),
            ResultDiff::Add {
                data: query_variables_to_json(after),
                row_signature: *row_signature,
            },
        ));
    }

    for updated in envelope.change_set().updated() {
        let RecordIdentity::QueryRow(row_signature) = updated.identity() else {
            return Err(ChangeAdapterError::IncompatibleRecord {
                ordinal: updated.ordinal(),
            });
        };
        let RecordData::QueryRow(after) = updated.after() else {
            return Err(ChangeAdapterError::IncompatibleRecord {
                ordinal: updated.ordinal(),
            });
        };
        let after = query_variables_to_json(after);
        let result = match updated.metadata() {
            UpdateMetadata::QueryUpdate { grouping_keys } => {
                let Some(RecordData::QueryRow(before)) = updated.before() else {
                    return Err(ChangeAdapterError::MissingQueryBeforeImage {
                        ordinal: updated.ordinal(),
                    });
                };
                ResultDiff::Update {
                    data: after.clone(),
                    before: query_variables_to_json(before),
                    after,
                    grouping_keys: grouping_keys
                        .as_ref()
                        .map(|keys| keys.iter().map(|key| key.to_string()).collect::<Vec<_>>()),
                    row_signature: *row_signature,
                }
            }
            UpdateMetadata::QueryAggregation { .. } => ResultDiff::Aggregation {
                before: match updated.before() {
                    Some(RecordData::QueryRow(before)) => Some(query_variables_to_json(before)),
                    None => None,
                    _ => {
                        return Err(ChangeAdapterError::IncompatibleRecord {
                            ordinal: updated.ordinal(),
                        });
                    }
                },
                after,
                row_signature: *row_signature,
            },
            UpdateMetadata::None => {
                return Err(ChangeAdapterError::IncompatibleRecord {
                    ordinal: updated.ordinal(),
                });
            }
        };
        ordered.push((updated.ordinal(), result));
    }

    for deleted in envelope.change_set().deleted() {
        let RecordIdentity::QueryRow(row_signature) = deleted.identity() else {
            return Err(ChangeAdapterError::IncompatibleRecord {
                ordinal: deleted.ordinal(),
            });
        };
        let Some(RecordData::QueryRow(before)) = deleted.before() else {
            return Err(ChangeAdapterError::IncompatibleRecord {
                ordinal: deleted.ordinal(),
            });
        };
        ordered.push((
            deleted.ordinal(),
            ResultDiff::Delete {
                data: query_variables_to_json(before),
                row_signature: *row_signature,
            },
        ));
    }
    ordered.sort_by_key(|(ordinal, _)| *ordinal);

    let system = envelope.system();
    let query_id = system
        .query_id()
        .ok_or(ChangeAdapterError::MissingQueryId)?;
    let sequence = system
        .sequence()
        .ok_or(ChangeAdapterError::MissingQuerySequence)?;

    Ok(QueryResult {
        query_id: query_id.to_string(),
        sequence,
        timestamp: system.timestamp(),
        results: ordered.into_iter().map(|(_, result)| result).collect(),
        metadata: envelope.metadata().clone(),
        profiling: system.profiling().cloned(),
    })
}

fn query_variables_to_json(variables: &QueryVariables) -> serde_json::Value {
    serde_json::Value::Object(
        variables
            .iter()
            .map(|(key, value)| (key.to_string(), query_value_to_json(value)))
            .collect(),
    )
}

fn query_value_to_json(value: &VariableValue) -> serde_json::Value {
    match value {
        VariableValue::Null => serde_json::Value::Null,
        VariableValue::Bool(value) => serde_json::Value::Bool(*value),
        VariableValue::Float(value) => {
            if value.is_f64() {
                let value = value.to_string();
                value
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or_else(|| serde_json::Value::String(value))
            } else {
                serde_json::Value::String(value.to_string())
            }
        }
        VariableValue::Integer(value) => {
            if let Some(value) = value.as_i64() {
                serde_json::Value::Number(value.into())
            } else if let Some(value) = value.as_u64() {
                serde_json::Value::Number(value.into())
            } else {
                serde_json::Value::String(value.to_string())
            }
        }
        VariableValue::String(value) => serde_json::Value::String(value.clone()),
        VariableValue::List(values) => {
            serde_json::Value::Array(values.iter().map(query_value_to_json).collect())
        }
        VariableValue::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), query_value_to_json(value)))
                .collect(),
        ),
        VariableValue::Date(value) => serde_json::Value::String(value.to_string()),
        VariableValue::LocalTime(value) => serde_json::Value::String(value.to_string()),
        VariableValue::ZonedTime(value) => serde_json::Value::String(value.to_string()),
        VariableValue::LocalDateTime(value) => serde_json::Value::String(value.to_string()),
        VariableValue::ZonedDateTime(value) => {
            serde_json::Value::String(value.datetime().to_rfc3339())
        }
        VariableValue::Duration(value) => serde_json::Value::String(value.to_string()),
        _ => serde_json::Value::String(format!("{value:?}")),
    }
}
