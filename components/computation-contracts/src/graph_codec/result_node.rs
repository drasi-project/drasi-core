// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use bytes::Bytes;
use drasi_core_types::{
    data_value::{decode_variables, encode_variables, DataValueError},
    evaluation::context::{QueryPartEvaluationContext, QueryVariables},
    models::{ElementMetadata, ElementReference},
};

use super::result_change::{result_changes, ResultChange};
use crate::{
    ChangeOperation, ElementImage, GraphChangeCodec, GraphCodecError, QueryRowKind, Record,
    UpdateSemantics,
};

#[derive(Debug, thiserror::Error)]
pub enum ResultNodeError {
    #[error("invalid result dataset: {0}")]
    Configuration(&'static str),
    #[error("a result row requires a nonempty opaque identity")]
    EmptyIdentity,
    #[error("element does not belong to this result dataset or has an invalid row identity")]
    Identity,
    #[error("expected a result node with the configured label")]
    Image,
    #[error(transparent)]
    Value(#[from] DataValueError),
    #[error(transparent)]
    Graph(Box<GraphCodecError>),
}

impl From<GraphCodecError> for ResultNodeError {
    fn from(error: GraphCodecError) -> Self {
        Self::Graph(Box::new(error))
    }
}

/// Query-facing facts about one projected operation. Generic graph consumers
/// never need this; the operation's records are complete without it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultOperationMetadata {
    /// The result's position in the evaluator slice, including skipped `Noop`s.
    pub ordinal: u64,
    /// The opaque row key the node identity was derived from.
    pub row_identity: Bytes,
    pub kind: QueryRowKind,
}

/// Canonical operations plus an index-aligned, separate metadata side channel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResultProjection {
    operations: Vec<ChangeOperation>,
    metadata: Vec<ResultOperationMetadata>,
}

impl ResultProjection {
    pub fn operations(&self) -> &[ChangeOperation] {
        &self.operations
    }

    /// `metadata()[i]` describes `operations()[i]`.
    pub fn metadata(&self) -> &[ResultOperationMetadata] {
        &self.metadata
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    pub fn into_parts(self) -> (Vec<ChangeOperation>, Vec<ResultOperationMetadata>) {
        (self.operations, self.metadata)
    }
}

/// Stateless projection of named result values onto the standard graph contract.
///
/// The caller supplies the stable, output-scoped dataset identity and the row's
/// opaque identity. Neither identity is inferred from a returned column. This
/// helper owns no rows, output cursor, epoch, index or publication policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultNodeProjector {
    dataset: Arc<str>,
    label: Arc<str>,
}

impl ResultNodeProjector {
    pub fn try_new(dataset: &str, label: Option<&str>) -> Result<Self, ResultNodeError> {
        if dataset.is_empty() {
            return Err(ResultNodeError::Configuration("empty output identity"));
        }
        let label = label.unwrap_or("Result");
        if label.is_empty() {
            return Err(ResultNodeError::Configuration("empty result label"));
        }
        Ok(Self {
            dataset: Arc::from(dataset),
            label: Arc::from(label),
        })
    }

    pub fn dataset(&self) -> &str {
        &self.dataset
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// The current opaque row key for an evaluator row signature.
    pub const fn signature_identity(row_signature: u64) -> [u8; 8] {
        row_signature.to_be_bytes()
    }

    pub fn reference(&self, identity: &[u8]) -> Result<ElementReference, ResultNodeError> {
        if identity.is_empty() {
            return Err(ResultNodeError::EmptyIdentity);
        }
        let key = identity
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(ElementReference {
            source_id: self.dataset.clone(),
            element_id: Arc::from(key),
        })
    }

    pub fn row_identity(&self, reference: &ElementReference) -> Result<Bytes, ResultNodeError> {
        let key = reference.element_id.as_bytes();
        if reference.source_id != self.dataset || key.is_empty() || key.len() % 2 != 0 {
            return Err(ResultNodeError::Identity);
        }
        let nibble = |byte| match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(ResultNodeError::Identity),
        };
        key.chunks_exact(2)
            .map(|pair| Ok((nibble(pair[0])? << 4) | nibble(pair[1])?))
            .collect::<Result<Vec<_>, _>>()
            .map(Bytes::from)
    }

    pub fn image(
        &self,
        identity: &[u8],
        values: &QueryVariables,
        effective_from: u64,
    ) -> Result<ElementImage, ResultNodeError> {
        Ok(ElementImage::Node {
            metadata: ElementMetadata {
                reference: self.reference(identity)?,
                labels: Arc::from([self.label.clone()]),
                effective_from,
            },
            properties: encode_variables(values)?,
        })
    }

    pub fn record(
        &self,
        identity: &[u8],
        values: &QueryVariables,
        effective_from: u64,
    ) -> Result<Record, ResultNodeError> {
        Ok(GraphChangeCodec::element_record(&self.image(
            identity,
            values,
            effective_from,
        )?)?)
    }

    /// Project an ordered evaluator slice without a query-row envelope.
    ///
    /// Adding, Updating and Removing become add, full-replacement and delete
    /// operations with full before images. Aggregation keeps the existing
    /// query semantics: a full replacement whose before image is optional.
    /// `Noop` creates no operation and operations keep their original slice
    /// ordinals. Every image uses `effective_from`.
    pub fn project_evaluation(
        &self,
        results: &[QueryPartEvaluationContext],
        effective_from: u64,
    ) -> Result<ResultProjection, ResultNodeError> {
        let mut projection = ResultProjection::default();
        for change in result_changes(results) {
            let identity = Self::signature_identity(change.row_signature());
            let ordinal = change.ordinal() as u64;
            let operation = match change {
                ResultChange::Add { after, .. } => ChangeOperation::Added {
                    ordinal,
                    after: self.record(&identity, after, effective_from)?,
                },
                ResultChange::Update { before, after, .. } => ChangeOperation::Updated {
                    ordinal,
                    before: Some(self.record(&identity, before, effective_from)?),
                    after: self.record(&identity, after, effective_from)?,
                    semantics: UpdateSemantics::Replace,
                },
                ResultChange::Aggregate { before, after, .. } => ChangeOperation::Updated {
                    ordinal,
                    before: before
                        .map(|before| self.record(&identity, before, effective_from))
                        .transpose()?,
                    after: self.record(&identity, after, effective_from)?,
                    semantics: UpdateSemantics::Replace,
                },
                ResultChange::Remove { before, .. } => {
                    let before = self.record(&identity, before, effective_from)?;
                    ChangeOperation::Deleted {
                        ordinal,
                        identity: before.reference().clone(),
                        before: Some(before),
                    }
                }
            };
            projection.operations.push(operation);
            projection.metadata.push(ResultOperationMetadata {
                ordinal,
                row_identity: Bytes::copy_from_slice(&identity),
                kind: change.row_kind(),
            });
        }
        Ok(projection)
    }

    /// Decode for a row-oriented reader; ordinary graph consumers need only
    /// `GraphChangeCodec::decode_element`, with no query metadata or this helper.
    pub fn decode_row(&self, record: &Record) -> Result<(Bytes, QueryVariables), ResultNodeError> {
        let ElementImage::Node {
            metadata,
            properties,
        } = GraphChangeCodec::decode_element(record)?
        else {
            return Err(ResultNodeError::Image);
        };
        if metadata.labels.as_ref() != [self.label.clone()] {
            return Err(ResultNodeError::Image);
        }
        Ok((
            self.row_identity(&metadata.reference)?,
            decode_variables(properties)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use drasi_core_types::{
        data_value::DataValue,
        evaluation::variable_value::{float::Float, integer::Integer, VariableValue},
        models::{Element, ElementPropertyMap, SourceChange},
    };

    use super::*;
    use crate::{ChangeEnvelope, ChangeSet, ChangeSetId, StreamId, SystemMetadata};

    fn row(name: &str) -> QueryVariables {
        QueryVariables::from([
            ("id".into(), VariableValue::String("not-the-row-key".into())),
            ("name".into(), VariableValue::String(name.into())),
        ])
    }

    #[test]
    fn result_rows_are_ordinary_graph_nodes_with_stable_opaque_identities() {
        let projector = ResultNodeProjector::try_new("instance/graph/query/out", None).unwrap();
        let identity = [0, 255, 128, 1, 0, 2, 3, 4, 5];
        let first = projector.record(&identity, &row("Alice"), 200).unwrap();
        let correction = projector.record(&identity, &row("Bob"), 100).unwrap();
        assert_eq!(first.schema(), GraphChangeCodec::schema().descriptor());
        assert_eq!(first.identity(), correction.identity());
        let ElementImage::Node {
            metadata,
            properties,
        } = GraphChangeCodec::decode_element(&correction).unwrap()
        else {
            panic!("a result row is one node");
        };
        assert_eq!(metadata.effective_from, 100);
        assert_eq!(metadata.labels.as_ref(), [Arc::from("Result")]);
        assert_eq!(properties["name"], DataValue::String("Bob".into()));
        assert_eq!(
            projector.row_identity(&metadata.reference).unwrap(),
            identity[..]
        );
        let (decoded_identity, values) = projector.decode_row(&correction).unwrap();
        assert_eq!(decoded_identity, identity[..]);
        assert_eq!(values, row("Bob"));
    }

    #[test]
    fn duplicate_values_and_separate_output_datasets_keep_distinct_identities() {
        let first = ResultNodeProjector::try_new("first/out", Some("RoomComfort")).unwrap();
        let second = ResultNodeProjector::try_new("second/out", Some("RoomComfort")).unwrap();
        let a = first.record(&[1], &row("same"), 1).unwrap();
        let b = first.record(&[2], &row("same"), 1).unwrap();
        let c = second.record(&[1], &row("same"), 1).unwrap();
        assert_ne!(a.identity(), b.identity());
        assert_ne!(a.identity(), c.identity());
        assert_eq!(first.label(), "RoomComfort");
        assert_eq!(first.dataset(), "first/out");
        assert!(second.decode_row(&a).is_err());
    }

    #[test]
    fn rich_columns_remain_properties_instead_of_becoming_additional_nodes() {
        let projector = ResultNodeProjector::try_new("query/out", None).unwrap();
        let node = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("database", "original-node"),
                labels: Arc::from([Arc::from("Person")]),
                effective_from: 7,
            },
            properties: ElementPropertyMap::new(),
        };
        let values = QueryVariables::from([
            ("node".into(), VariableValue::Element(Arc::new(node))),
            (
                "unsigned".into(),
                VariableValue::Integer(Integer::from(u64::MAX)),
            ),
            (
                "float".into(),
                VariableValue::Float(Float::from(f64::from_bits(0x7ff8_0000_0000_0042))),
            ),
            ("pending".into(), VariableValue::Awaiting),
        ]);
        let record = projector.record(&[1], &values, 9).unwrap();
        let image = GraphChangeCodec::decode_element(&record).unwrap();
        assert!(matches!(image.properties()["node"], DataValue::Element(_)));
        let (_, decoded) = projector.decode_row(&record).unwrap();
        assert_eq!(
            encode_variables(&decoded).unwrap(),
            encode_variables(&values).unwrap()
        );
    }

    #[test]
    fn invalid_configuration_and_noncanonical_row_keys_are_rejected() {
        assert!(ResultNodeProjector::try_new("", None).is_err());
        assert!(ResultNodeProjector::try_new("query/out", Some("")).is_err());
        let projector = ResultNodeProjector::try_new("query/out", None).unwrap();
        assert!(projector.reference(&[]).is_err());
        for key in ["", "0", "FF", "gg", "é"] {
            assert!(projector
                .row_identity(&ElementReference::new("query/out", key))
                .is_err());
        }
    }

    fn envelope(operations: &[ChangeOperation]) -> ChangeEnvelope {
        let stream = StreamId::try_new("query/out").unwrap();
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new("query/out", Bytes::from_static(&[1])).unwrap(),
            GraphChangeCodec::schema().descriptor().clone(),
            operations.to_vec(),
        )
        .unwrap();
        ChangeEnvelope::new(
            crate::emission_id(&stream, 1).unwrap(),
            changes,
            SystemMetadata::new(stream, 1)
                .with_timestamp(DateTime::from_timestamp_millis(1_000).unwrap()),
        )
    }

    fn image_values(record: &Record) -> QueryVariables {
        decode_variables(
            GraphChangeCodec::decode_element(record)
                .unwrap()
                .properties()
                .clone(),
        )
        .unwrap()
    }

    fn aggregate(
        signature: u64,
        before: Option<QueryVariables>,
        after: QueryVariables,
    ) -> QueryPartEvaluationContext {
        QueryPartEvaluationContext::Aggregation {
            before,
            after,
            grouping_keys: vec!["room".into(), "floor".into()],
            default_before: true,
            default_after: false,
            row_signature: signature,
        }
    }

    #[test]
    fn evaluation_projection_keeps_noop_gaps_and_operation_order() {
        let projector = ResultNodeProjector::try_new("query/out", None).unwrap();
        let results = vec![
            QueryPartEvaluationContext::Noop,
            QueryPartEvaluationContext::Adding {
                after: row("added"),
                row_signature: 1,
            },
            QueryPartEvaluationContext::Noop,
            QueryPartEvaluationContext::Updating {
                before: row("old"),
                after: row("new"),
                row_signature: 2,
            },
            aggregate(3, None, row("group")),
            QueryPartEvaluationContext::Removing {
                before: row("gone"),
                row_signature: 4,
            },
            QueryPartEvaluationContext::Noop,
        ];
        let projection = projector.project_evaluation(&results, 50).unwrap();
        let operations = projection.operations();
        assert_eq!(
            operations
                .iter()
                .map(ChangeOperation::ordinal)
                .collect::<Vec<_>>(),
            [1, 3, 4, 5]
        );
        assert_eq!(
            projection
                .metadata()
                .iter()
                .map(|m| m.ordinal)
                .collect::<Vec<_>>(),
            [1, 3, 4, 5]
        );
        assert!(matches!(operations[0], ChangeOperation::Added { .. }));
        assert!(matches!(
            operations[1],
            ChangeOperation::Updated {
                semantics: UpdateSemantics::Replace,
                ..
            }
        ));
        assert!(matches!(
            operations[2],
            ChangeOperation::Updated {
                before: None,
                semantics: UpdateSemantics::Replace,
                ..
            }
        ));
        assert!(matches!(operations[3], ChangeOperation::Deleted { .. }));
        let envelope = envelope(operations);
        assert_eq!(
            envelope.changes().schema(),
            GraphChangeCodec::schema().descriptor()
        );
        let decoded = GraphChangeCodec::decode_changes(&envelope).unwrap();
        assert_eq!(decoded.len(), 4);
        assert!(matches!(decoded[0], SourceChange::Insert { .. }));
        assert!(matches!(decoded[1], SourceChange::Replace { .. }));
        assert!(matches!(decoded[2], SourceChange::Replace { .. }));
        let SourceChange::Delete { metadata } = &decoded[3] else {
            panic!("a removed row is one graph delete");
        };
        assert_eq!(metadata.effective_from, 1_000);
        assert_eq!(
            projector.row_identity(&metadata.reference).unwrap(),
            4u64.to_be_bytes()[..]
        );
        assert!(projector
            .project_evaluation(&[QueryPartEvaluationContext::Noop], 1)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn projected_identities_come_from_signatures_not_values() {
        let projector = ResultNodeProjector::try_new("query/out", None).unwrap();
        let results = [1, 2, u64::MAX]
            .map(|row_signature| QueryPartEvaluationContext::Adding {
                after: row("same"),
                row_signature,
            })
            .to_vec();
        let projection = projector.project_evaluation(&results, 1).unwrap();
        let identities = projection
            .operations()
            .iter()
            .map(|operation| operation.identity().clone())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(identities.len(), 3);
        for (metadata, signature) in projection.metadata().iter().zip([1u64, 2, u64::MAX]) {
            assert_eq!(metadata.row_identity, signature.to_be_bytes()[..]);
        }
        for operation in projection.operations() {
            let ChangeOperation::Added { after, .. } = operation else {
                panic!("expected an add");
            };
            assert_eq!(image_values(after), row("same"));
        }
    }

    #[test]
    fn aggregate_identity_is_stable_and_before_images_are_full() {
        let projector = ResultNodeProjector::try_new("query/out", None).unwrap();
        let first = projector
            .project_evaluation(&[aggregate(9, Some(row("one")), row("two"))], 10)
            .unwrap();
        let second = projector
            .project_evaluation(&[aggregate(9, Some(row("two")), row("three"))], 20)
            .unwrap();
        let (
            ChangeOperation::Updated {
                before: Some(before),
                after,
                ..
            },
            ChangeOperation::Updated { after: next, .. },
        ) = (&first.operations()[0], &second.operations()[0])
        else {
            panic!("aggregations replace one row");
        };
        assert_eq!(before.identity(), after.identity());
        assert_eq!(after.identity(), next.identity());
        assert_eq!(before.image(), crate::RecordImage::Full);
        assert_eq!(image_values(before), row("one"));
        assert_eq!(image_values(after), row("two"));
        assert_eq!(image_values(next), row("three"));

        let updating = projector
            .project_evaluation(
                &[QueryPartEvaluationContext::Updating {
                    before: row("old"),
                    after: row("new"),
                    row_signature: 5,
                }],
                1,
            )
            .unwrap();
        let ChangeOperation::Updated {
            before: Some(before),
            ..
        } = &updating.operations()[0]
        else {
            panic!("an update carries its before image");
        };
        assert_eq!(before.image(), crate::RecordImage::Full);
        assert_eq!(image_values(before), row("old"));
        let removing = projector
            .project_evaluation(
                &[QueryPartEvaluationContext::Removing {
                    before: row("gone"),
                    row_signature: 6,
                }],
                1,
            )
            .unwrap();
        let ChangeOperation::Deleted {
            identity,
            before: Some(before),
            ..
        } = &removing.operations()[0]
        else {
            panic!("a delete carries its before image");
        };
        assert_eq!(identity, before.reference());
        assert_eq!(before.image(), crate::RecordImage::Full);
        assert_eq!(image_values(before), row("gone"));
    }

    #[test]
    fn grouping_and_default_flags_live_only_in_separate_metadata() {
        let projector = ResultNodeProjector::try_new("query/out", None).unwrap();
        let results = [
            QueryPartEvaluationContext::Adding {
                after: row("a"),
                row_signature: 1,
            },
            aggregate(2, None, row("b")),
        ];
        let projection = projector.project_evaluation(&results, 1).unwrap();
        assert_eq!(projection.metadata()[0].kind, QueryRowKind::Row);
        assert_eq!(
            projection.metadata()[1].kind,
            QueryRowKind::Aggregation {
                grouping_keys: vec!["room".into(), "floor".into()],
                default_before: true,
                default_after: false,
            }
        );
        let ChangeOperation::Updated { after, .. } = &projection.operations()[1] else {
            panic!("an aggregation is a replacement");
        };
        let image = GraphChangeCodec::decode_element(after).unwrap();
        assert_eq!(
            image
                .properties()
                .keys()
                .map(|key| key.as_str())
                .collect::<Vec<_>>(),
            ["id", "name"]
        );
        let (operations, _) = projection.into_parts();
        assert!(GraphChangeCodec::decode_changes(&envelope(&operations)).is_ok());
    }

    #[test]
    fn projected_rows_use_caller_dataset_label_and_rich_values_as_properties() {
        let projector = ResultNodeProjector::try_new("query/out", Some("Comfort")).unwrap();
        let node = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("database", "original-node"),
                labels: Arc::from([Arc::from("Person")]),
                effective_from: 7,
            },
            properties: ElementPropertyMap::new(),
        };
        let values =
            QueryVariables::from([("node".into(), VariableValue::Element(Arc::new(node)))]);
        let projection = projector
            .project_evaluation(
                &[QueryPartEvaluationContext::Adding {
                    after: values.clone(),
                    row_signature: 7,
                }],
                9,
            )
            .unwrap();
        let ChangeOperation::Added { after, .. } = &projection.operations()[0] else {
            panic!("expected an add");
        };
        assert_eq!(after.schema(), GraphChangeCodec::schema().descriptor());
        let ElementImage::Node {
            metadata,
            properties,
        } = GraphChangeCodec::decode_element(after).unwrap()
        else {
            panic!("a result row is one node");
        };
        assert_eq!(metadata.reference.source_id.as_ref(), "query/out");
        assert_eq!(metadata.labels.as_ref(), [Arc::from("Comfort")]);
        assert_eq!(metadata.effective_from, 9);
        assert_eq!(properties.len(), 1);
        assert!(matches!(properties["node"], DataValue::Element(_)));
        assert_eq!(
            encode_variables(&image_values(after)).unwrap(),
            encode_variables(&values).unwrap()
        );
        let decoded = GraphChangeCodec::decode_changes(&envelope(projection.operations())).unwrap();
        assert_eq!(decoded.len(), 1);
    }
}
