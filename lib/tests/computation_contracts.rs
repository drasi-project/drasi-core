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

#![cfg(feature = "computation")]

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_lib::computation::v1::*;
use tokio::sync::mpsc;

// A custom binary schema, unrelated to graph elements or query result rows.
// Full/patch: [record-id, unsigned value big-endian u16]; partial: [record-id].
struct MeterValidator {
    calls: Arc<AtomicUsize>,
}

impl RecordValidator for MeterValidator {
    fn validate_identity(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
    ) -> std::result::Result<(), RecordValidationError> {
        if schema.encoding() != "meter-bin-v1"
            || identity.namespace() != "meters"
            || identity.value().len() != 1
        {
            return Err(RecordValidationError::new(
                "identity",
                "meter identity must be one byte in the meters namespace",
            ));
        }
        Ok(())
    }

    fn validate(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
        image: RecordImage,
        payload: &[u8],
    ) -> std::result::Result<(), RecordValidationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let length = match image {
            RecordImage::Full | RecordImage::Patch => 3,
            RecordImage::Partial => 1,
        };
        if schema.encoding() != "meter-bin-v1" || payload.len() != length {
            return Err(RecordValidationError::new(
                "encoding",
                "invalid meter image",
            ));
        }
        if identity.namespace() != "meters" || identity.value().as_ref() != &payload[..1] {
            return Err(RecordValidationError::new(
                "identity",
                "embedded identity differs",
            ));
        }
        Ok(())
    }
}

fn descriptor() -> SchemaDescriptor {
    descriptor_with("example.meter", 1, "meter-bin-v1", b"id:u8,value:u16be")
}

fn descriptor_with(
    id: &str,
    version: u32,
    encoding: &str,
    definition: &'static [u8],
) -> SchemaDescriptor {
    SchemaDescriptor::try_new(
        SchemaId::try_new(id).expect("valid fixture schema ID"),
        SchemaVersion::try_new(version).expect("positive fixture schema version"),
        encoding,
        Bytes::from_static(definition),
    )
    .expect("valid fixture schema descriptor")
}

fn schema() -> Schema {
    Schema::new(
        descriptor(),
        Arc::new(MeterValidator {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    )
}

fn record_id(id: u8) -> RecordId {
    RecordId::try_new("meters", Bytes::copy_from_slice(&[id])).expect("valid fixture record ID")
}

fn reference(id: u8) -> RecordReference {
    RecordReference::try_new(&schema(), record_id(id)).expect("valid fixture record reference")
}

fn record_for(schema: &Schema, id: u8, image: RecordImage) -> Record {
    let bytes = match image {
        RecordImage::Partial => vec![id],
        RecordImage::Full | RecordImage::Patch => vec![id, 0xff, 0xff],
    };
    Record::try_new(schema, record_id(id), image, Bytes::from(bytes))
        .expect("valid fixture record image")
}

fn record(id: u8, image: RecordImage) -> Record {
    record_for(&schema(), id, image)
}

fn change_set_id() -> ChangeSetId {
    ChangeSetId::try_new("test.batch", Bytes::from_static(b"batch-1"))
        .expect("valid fixture change-set ID")
}

fn changes(operations: Vec<ChangeOperation>) -> ChangeSetRef {
    ChangeSet::try_new(change_set_id(), descriptor(), operations).expect("valid fixture change-set")
}

fn added(ordinal: u64) -> ChangeOperation {
    ChangeOperation::Added {
        ordinal,
        after: record(1, RecordImage::Full),
    }
}

fn event_id(sequence: u64) -> EnvelopeId {
    EnvelopeId::try_new(
        "test.event",
        Bytes::copy_from_slice(&sequence.to_be_bytes()),
    )
    .expect("valid fixture envelope ID")
}

fn stream(name: &str) -> StreamId {
    StreamId::try_new(name).expect("valid fixture stream ID")
}

fn envelope(sequence: u64, operations: Vec<ChangeOperation>) -> Envelope {
    Envelope::new(
        event_id(sequence),
        changes(operations),
        SystemMetadata::new(stream("source/out"), sequence),
    )
}

fn entry(key: &str, value: ContextValue) -> ContextEntry {
    ContextEntry::try_new(
        ComponentId::try_new("transform").expect("valid fixture contributor"),
        key,
        value,
    )
    .expect("valid fixture context entry")
}

#[test]
fn public_identifiers_and_schema_versions_reject_invalid_inputs_without_repair() {
    for value in ["", " ", "leading ", "a\tb", "a\nb", "a\0b", "a\u{2003}b"] {
        assert!(matches!(
            SchemaId::try_new(value),
            Err(ContractError::InvalidIdentifier { .. })
        ));
        assert!(ComponentId::try_new(value).is_err());
        assert!(PortId::try_new(value).is_err());
        assert!(StreamId::try_new(value).is_err());
        assert!(RecordId::try_new(value, Bytes::from_static(b"x")).is_err());
        assert!(ChangeSetId::try_new(value, Bytes::from_static(b"x")).is_err());
        assert!(EnvelopeId::try_new(value, Bytes::from_static(b"x")).is_err());
        assert!(ContextEntry::try_new(
            ComponentId::try_new("test").unwrap(),
            value,
            ContextValue::Bool(true)
        )
        .is_err());
    }
    assert!(matches!(
        SchemaVersion::try_new(0),
        Err(ContractError::InvalidSchemaVersion { value: 0 })
    ));
    assert_eq!(SchemaVersion::try_new(u32::MAX).unwrap().value(), u32::MAX);
    assert_eq!(
        StreamId::try_new("producer/port:0").unwrap().as_str(),
        "producer/port:0"
    );
    assert!(matches!(
        RecordId::try_new("id", Bytes::new()),
        Err(ContractError::EmptyIdentity { .. })
    ));
    assert!(ChangeSetId::try_new("id", Bytes::new()).is_err());
    assert!(EnvelopeId::try_new("id", Bytes::new()).is_err());
    // Opaque zero u64 identities, including original A2 IDs, remain valid.
    assert_eq!(event_id(0).value().as_ref(), &0_u64.to_be_bytes());
    assert_ne!(
        RecordId::try_new("left", Bytes::from_static(b"1")).unwrap(),
        RecordId::try_new("right", Bytes::from_static(b"1")).unwrap()
    );
    assert!(matches!(
        SchemaDescriptor::try_new(
            SchemaId::try_new("meter").unwrap(),
            SchemaVersion::try_new(1).unwrap(),
            "binary",
            Bytes::new(),
        ),
        Err(ContractError::EmptySchemaDefinition)
    ));
    assert!(SchemaDescriptor::try_new(
        SchemaId::try_new("meter").unwrap(),
        SchemaVersion::try_new(1).unwrap(),
        " ",
        Bytes::from_static(b"definition"),
    )
    .is_err());
}

#[test]
fn full_descriptors_and_fingerprints_include_identity_version_encoding_and_definition() {
    let base = descriptor();
    for different in [
        descriptor_with("example.other", 1, "meter-bin-v1", b"id:u8,value:u16be"),
        descriptor_with("example.meter", 2, "meter-bin-v1", b"id:u8,value:u16be"),
        descriptor_with("example.meter", 1, "meter-bin-v2", b"id:u8,value:u16be"),
        descriptor_with("example.meter", 1, "meter-bin-v1", b"id:u8,value:u16le"),
    ] {
        assert_ne!(base, different);
        assert_ne!(base.fingerprint(), different.fingerprint());
    }
    assert_eq!(base, descriptor());
    assert_eq!(
        base.fingerprint().value(),
        descriptor().fingerprint().value()
    );
    // Tagged fields prevent ambiguous concatenations across identity/content.
    assert_ne!(
        descriptor_with("ab", 1, "c", b"d").fingerprint(),
        descriptor_with("a", 1, "bc", b"d").fingerprint()
    );
}

#[test]
fn validator_runs_on_every_image_and_checks_actual_payload_identity() {
    let calls = Arc::new(AtomicUsize::new(0));
    let schema = Schema::new(
        descriptor(),
        Arc::new(MeterValidator {
            calls: calls.clone(),
        }),
    );
    for image in [RecordImage::Full, RecordImage::Patch, RecordImage::Partial] {
        let valid = record_for(&schema, 1, image);
        assert_eq!(valid.image(), image);
        let invalid = Record::try_new(&schema, record_id(2), image, valid.payload().clone());
        assert!(matches!(
            invalid,
            Err(ContractError::RecordValidation { source, .. }) if source.code() == "identity"
        ));
        assert!(matches!(
            Record::try_new(&schema, record_id(1), image, Bytes::new()),
            Err(ContractError::RecordValidation { source, .. }) if source.code() == "encoding"
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 9);
}

#[test]
fn identity_only_deletes_require_schema_validated_keys() {
    let schema = schema();
    for identity in [
        RecordId::try_new("unrelated-namespace", Bytes::from_static(&[1])).unwrap(),
        RecordId::try_new("meters", Bytes::from_static(&[1, 2])).unwrap(),
    ] {
        assert!(matches!(
            RecordReference::try_new(&schema, identity.clone()),
            Err(ContractError::RecordValidation { source, .. })
                if source.code() == "identity"
        ));
        assert!(matches!(
            Record::try_new(&schema, identity, RecordImage::Full, Bytes::from_static(&[1, 0, 1])),
            Err(ContractError::RecordValidation { source, .. })
                if source.code() == "identity"
        ));
    }
    let identity = RecordReference::try_new(&schema, record_id(1)).unwrap();
    let delete = ChangeOperation::Deleted {
        ordinal: 0,
        identity: identity.clone(),
        before: None,
    };
    assert!(ChangeSet::try_new(change_set_id(), descriptor(), vec![delete.clone()]).is_ok());
    assert!(matches!(
        ChangeSet::try_new(
            change_set_id(),
            descriptor_with("example.meter", 2, "meter-bin-v1", b"id:u8,value:u16be"),
            vec![delete],
        ),
        Err(ContractError::SchemaMismatch { .. })
    ));
    assert_eq!(identity, record(1, RecordImage::Full).reference().clone());
}

#[test]
fn ordered_custom_changes_preserve_add_patch_replace_delete_and_before_images() {
    let operations = vec![
        added(0),
        ChangeOperation::Updated {
            ordinal: 2,
            before: Some(record(1, RecordImage::Partial)),
            after: record(1, RecordImage::Patch),
            semantics: UpdateSemantics::Patch,
        },
        ChangeOperation::Updated {
            ordinal: 3,
            before: Some(record(1, RecordImage::Full)),
            after: record(1, RecordImage::Full),
            semantics: UpdateSemantics::Replace,
        },
        ChangeOperation::Deleted {
            ordinal: u64::MAX,
            identity: reference(1),
            before: Some(record(1, RecordImage::Partial)),
        },
    ];
    let batch = changes(operations.clone());
    assert_eq!(batch.operations(), operations);
    assert_eq!(batch.id(), &change_set_id());
    assert!(!batch.is_append_only());
    assert!(batch
        .operations()
        .iter()
        .all(|op| op.identity() == &record_id(1)));
    assert_eq!(batch.operations()[3].ordinal(), u64::MAX);
    assert!(changes(vec![added(0), added(10)]).is_append_only());
    assert!(changes(vec![]).is_empty());
    assert!(changes(vec![]).is_append_only());
    assert!(ChangeSet::try_new(
        change_set_id(),
        descriptor(),
        vec![
            ChangeOperation::Updated {
                ordinal: 0,
                before: None,
                after: record(1, RecordImage::Patch),
                semantics: UpdateSemantics::Patch,
            },
            ChangeOperation::Updated {
                ordinal: 1,
                before: None,
                after: record(1, RecordImage::Full),
                semantics: UpdateSemantics::Replace,
            },
            ChangeOperation::Deleted {
                ordinal: 2,
                identity: reference(1),
                before: None,
            },
        ],
    )
    .is_ok());
}

#[test]
fn duplicate_and_out_of_order_ordinals_are_not_sorted_or_silently_accepted() {
    assert!(matches!(
        ChangeSet::try_new(change_set_id(), descriptor(), vec![added(5), added(5)]),
        Err(ContractError::DuplicateOrdinal { ordinal: 5 })
    ));
    assert!(matches!(
        ChangeSet::try_new(
            change_set_id(),
            descriptor(),
            vec![
                added(5),
                ChangeOperation::Deleted {
                    ordinal: 5,
                    identity: reference(1),
                    before: None
                }
            ]
        ),
        Err(ContractError::DuplicateOrdinal { ordinal: 5 })
    ));
    assert!(matches!(
        ChangeSet::try_new(change_set_id(), descriptor(), vec![added(5), added(1)]),
        Err(ContractError::OutOfOrderOrdinal {
            previous: 5,
            ordinal: 1
        })
    ));
}

#[test]
fn every_before_and_after_schema_must_match_the_entire_batch_descriptor() {
    for different in [
        descriptor_with("example.other", 1, "meter-bin-v1", b"id:u8,value:u16be"),
        descriptor_with("example.meter", 2, "meter-bin-v1", b"id:u8,value:u16be"),
        descriptor_with("example.meter", 1, "meter-bin-v1", b"different-definition"),
    ] {
        let other = Schema::new(
            different,
            Arc::new(MeterValidator {
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        );
        let foreign = record_for(&other, 1, RecordImage::Full);
        for operation in [
            ChangeOperation::Added {
                ordinal: 0,
                after: foreign.clone(),
            },
            ChangeOperation::Updated {
                ordinal: 0,
                before: Some(foreign.clone()),
                after: record(1, RecordImage::Full),
                semantics: UpdateSemantics::Replace,
            },
            ChangeOperation::Updated {
                ordinal: 0,
                before: Some(record(1, RecordImage::Full)),
                after: foreign.clone(),
                semantics: UpdateSemantics::Replace,
            },
            ChangeOperation::Deleted {
                ordinal: 0,
                identity: reference(1),
                before: Some(foreign),
            },
        ] {
            assert!(matches!(
                ChangeSet::try_new(change_set_id(), descriptor(), vec![operation]),
                Err(ContractError::SchemaMismatch { .. })
            ));
        }
    }
}

#[test]
fn before_identity_and_image_roles_are_checked_for_updates_and_deletes() {
    for operation in [
        ChangeOperation::Updated {
            ordinal: 0,
            before: Some(record(2, RecordImage::Full)),
            after: record(1, RecordImage::Full),
            semantics: UpdateSemantics::Replace,
        },
        ChangeOperation::Deleted {
            ordinal: 0,
            identity: reference(1),
            before: Some(record(2, RecordImage::Full)),
        },
    ] {
        assert!(matches!(
            ChangeSet::try_new(change_set_id(), descriptor(), vec![operation]),
            Err(ContractError::IdentityMismatch { ordinal: 0, .. })
        ));
    }
    for operation in [
        ChangeOperation::Added {
            ordinal: 0,
            after: record(1, RecordImage::Patch),
        },
        ChangeOperation::Added {
            ordinal: 0,
            after: record(1, RecordImage::Partial),
        },
        ChangeOperation::Updated {
            ordinal: 0,
            before: None,
            after: record(1, RecordImage::Patch),
            semantics: UpdateSemantics::Replace,
        },
        ChangeOperation::Updated {
            ordinal: 0,
            before: None,
            after: record(1, RecordImage::Full),
            semantics: UpdateSemantics::Patch,
        },
        ChangeOperation::Updated {
            ordinal: 0,
            before: Some(record(1, RecordImage::Patch)),
            after: record(1, RecordImage::Full),
            semantics: UpdateSemantics::Replace,
        },
        ChangeOperation::Deleted {
            ordinal: 0,
            identity: reference(1),
            before: Some(record(1, RecordImage::Patch)),
        },
    ] {
        assert!(matches!(
            ChangeSet::try_new(change_set_id(), descriptor(), vec![operation]),
            Err(ContractError::InvalidImage { ordinal: 0, .. })
        ));
    }
}

#[test]
fn change_event_is_shared_while_each_envelope_annotation_list_can_grow() {
    let event = Arc::new(ChangeEvent::new(
        event_id(3),
        changes(vec![added(0)]),
        SystemMetadata::new(stream("source/out"), 3),
    ));
    let mut first = ChangeEnvelope::from_event(event.clone());
    first
        .append_annotation(entry("source", ContextValue::Bool(true)))
        .unwrap();
    let mut left = first.clone();
    let mut right = first.clone();
    left.append_annotation(entry("branch", ContextValue::String(Arc::from("left"))))
        .unwrap();
    right
        .append_annotation(entry("branch", ContextValue::String(Arc::from("right"))))
        .unwrap();

    for envelope in [&first, &left, &right] {
        assert!(Arc::ptr_eq(envelope.event(), &event));
        assert!(Arc::ptr_eq(envelope.changes(), event.changes()));
        assert!(Arc::ptr_eq(envelope.system(), event.system()));
        assert_eq!(envelope.id(), event.id());
    }
    assert_eq!(first.annotations().len(), 1);
    assert_eq!(left.annotations().len(), 2);
    assert_eq!(right.annotations().len(), 2);
    assert_eq!(
        left.annotations().entries().next().unwrap().value(),
        ContextValue::String(Arc::from("left"))
    );
    assert_eq!(
        right.annotations().entries().next().unwrap().value(),
        ContextValue::String(Arc::from("right"))
    );
    assert_eq!(event.changes().operations().len(), 1);
    assert_eq!(event.system().sequence(), 3);
    assert!(event.lineage().is_none());
}

#[test]
fn deriving_a_new_change_event_retains_but_does_not_mutate_input_history() {
    let original = envelope(7, vec![added(0)]);
    let mut derived = original.derive(
        event_id(8),
        changes(vec![added(1)]),
        SystemMetadata::new(stream("transform/out"), 8),
    );
    derived
        .append_annotation(entry("processed", ContextValue::Bool(true)))
        .unwrap();
    assert!(!Arc::ptr_eq(original.event(), derived.event()));
    assert!(original.annotations().is_empty());
    assert_eq!(derived.annotations().len(), 1);
    assert_eq!(
        derived.event().lineage().unwrap().envelope_id(),
        original.id()
    );
    assert!(Arc::ptr_eq(
        derived.event().lineage().unwrap().system(),
        original.system()
    ));
    assert_eq!(original.event().changes().operations()[0].ordinal(), 0);
    assert_eq!(derived.event().changes().operations()[0].ordinal(), 1);
}

#[test]
fn context_branches_append_without_mutating_siblings_and_share_data() {
    let text: Arc<str> = Arc::from("value");
    let bytes: Arc<[u8]> = Arc::from([1, 2, 3]);
    let root = envelope(0, vec![added(0)]);
    let first = root
        .append_context(entry("key", ContextValue::String(text.clone())))
        .unwrap();
    let left = first
        .append_context(entry("key", ContextValue::Bytes(bytes.clone())))
        .unwrap();
    let right = first
        .append_context(entry("key", ContextValue::Bool(false)))
        .unwrap();
    assert!(root.context().is_empty());
    assert_eq!(first.context().len(), 1);
    assert_eq!(left.context().len(), 2);
    assert_eq!(right.context().len(), 2);
    let history: Vec<_> = left.context().entries().collect();
    assert_eq!(
        history.iter().map(ContextEntry::key).collect::<Vec<_>>(),
        ["key", "key"]
    );
    assert_eq!(history[0].contributor(), "transform");
    assert!(
        matches!(history[0].value(), ContextValue::Bytes(value) if Arc::ptr_eq(&value, &bytes))
    );
    assert!(
        matches!(history[1].value(), ContextValue::String(value) if Arc::ptr_eq(&value, &text))
    );
    assert_eq!(
        right.context().entries().next().unwrap().value(),
        ContextValue::Bool(false)
    );
    assert_eq!(
        first.context().entries().next().unwrap().value(),
        ContextValue::String(text)
    );
    assert_eq!(root.id(), left.id());
    assert!(Arc::ptr_eq(root.changes(), left.changes()));
    assert!(Arc::ptr_eq(left.changes(), right.changes()));
    assert!(Arc::ptr_eq(root.system(), left.system()));
    let signed = left
        .append_context(entry("signed", ContextValue::Signed(i64::MIN)))
        .unwrap();
    let unsigned = signed
        .append_context(entry("unsigned", ContextValue::Unsigned(u64::MAX)))
        .unwrap();
    let values: Vec<_> = unsigned
        .context()
        .entries()
        .map(|entry| entry.value())
        .collect();
    assert_eq!(values[0], ContextValue::Unsigned(u64::MAX));
    assert_eq!(values[1], ContextValue::Signed(i64::MIN));

    let payload = record(1, RecordImage::Full);
    let copy = payload.clone();
    assert_eq!(payload.payload().as_ptr(), copy.payload().as_ptr());
    assert_eq!(
        payload.schema().definition().as_ptr(),
        copy.schema().definition().as_ptr()
    );
}

#[test]
fn derivation_preserves_input_lineage_metadata_and_branch_context() {
    let timestamp = chrono::DateTime::from_timestamp(123, 0).unwrap();
    let root = Envelope::new(
        event_id(41),
        changes(vec![added(0)]),
        SystemMetadata::new(stream("input/out"), 41)
            .with_timestamp(timestamp)
            .with_source_position(Bytes::from_static(b"position")),
    )
    .append_context(entry("source", ContextValue::Bool(true)))
    .unwrap();
    let derived = root.derive(
        EnvelopeId::try_new("transform", Bytes::from_static(b"1")).unwrap(),
        root.changes().clone(),
        SystemMetadata::new(stream("transform/out"), 0),
    );
    let next = derived.derive(
        event_id(42),
        derived.changes().clone(),
        SystemMetadata::new(stream("next/out"), 7),
    );
    assert_eq!(derived.context().len(), 1);
    assert_eq!(derived.system().sequence(), 0);
    assert_eq!(derived.system().stream().as_str(), "transform/out");
    assert_eq!(root.system().sequence(), 41);
    assert_eq!(root.system().timestamp(), Some(timestamp));
    assert_eq!(
        root.system().source_position().unwrap().as_ref(),
        b"position"
    );
    assert!(root.lineage().is_none());
    assert_eq!(derived.lineage().unwrap().envelope_id(), root.id());
    assert!(Arc::ptr_eq(
        derived.lineage().unwrap().system(),
        root.system()
    ));
    assert!(Arc::ptr_eq(
        next.lineage().unwrap().parent().unwrap(),
        derived.lineage().unwrap()
    ));
    assert!(Arc::ptr_eq(root.changes(), derived.changes()));
}

fn port(name: &str, direction: PortDirection, requirements: PipeRequirements) -> PortDescriptor {
    PortDescriptor::new(
        PortId::try_new(name).expect("valid fixture port ID"),
        direction,
        descriptor(),
        requirements,
    )
}

#[test]
fn port_negotiation_checks_directions_full_schema_and_both_requirements() {
    let fifo = PipeCapabilities::volatile_bounded(NonZeroUsize::new(1).unwrap());
    let output = port("out", PortDirection::Output, PipeRequirements::default());
    let input = port("in", PortDirection::Input, PipeRequirements::default());
    assert!(validate_connection(&output, &input, &fifo).is_ok());
    assert!(matches!(
        validate_connection(&input, &output, &fifo),
        Err(ContractError::WrongPortDirection { .. })
    ));
    let non_fifo = PipeCapabilities::try_new([], None).unwrap();
    assert!(matches!(
        validate_connection(&output, &input, &non_fifo),
        Err(ContractError::UnsupportedCapability {
            capability: PipeCapability::FifoPerStream
        })
    ));
    for schema in [
        descriptor_with("example.meter", 2, "meter-bin-v1", b"id:u8,value:u16be"),
        descriptor_with("example.meter", 1, "other", b"id:u8,value:u16be"),
        descriptor_with("example.meter", 1, "meter-bin-v1", b"other-definition"),
    ] {
        let other = PortDescriptor::new(
            input.id().clone(),
            PortDirection::Input,
            schema,
            PipeRequirements::default(),
        );
        assert!(matches!(
            validate_connection(&output, &other, &fifo),
            Err(ContractError::SchemaMismatch { .. })
        ));
    }
    for capability in [
        PipeCapability::DurableAcceptance,
        PipeCapability::ExplicitAcknowledgement,
        PipeCapability::Replay,
        PipeCapability::Transactions,
        PipeCapability::ExactlyOnce,
    ] {
        let requirement = PipeRequirements::new([capability]);
        assert!(matches!(
            fifo.validate(&requirement),
            Err(ContractError::UnsupportedCapability { capability: actual }) if actual == capability
        ));
        let required_output = port("out", PortDirection::Output, requirement.clone());
        let required_input = port("in", PortDirection::Input, requirement);
        assert!(validate_connection(&required_output, &input, &fifo).is_err());
        assert!(validate_connection(&output, &required_input, &fifo).is_err());
    }
    assert_eq!(fifo.capacity().unwrap().get(), 1);
    assert_eq!(fifo.supported().len(), 2);
    assert!(matches!(
        ComponentDescriptor::try_new(
            ComponentId::try_new("duplicate").unwrap(),
            vec![
                input.clone(),
                PortDescriptor::new(
                    input.id().clone(),
                    PortDirection::Output,
                    descriptor(),
                    PipeRequirements::default()
                )
            ]
        ),
        Err(ContractError::DuplicatePort { .. })
    ));
}

#[test]
fn capability_claims_reject_missing_prerequisites() {
    use PipeCapability::*;
    for (supported, capability, requires) in [
        (vec![Replay], Replay, DurableAcceptance),
        (vec![Transactions], Transactions, ExplicitAcknowledgement),
        (vec![ExactlyOnce], ExactlyOnce, DurableAcceptance),
        (
            vec![ExactlyOnce, DurableAcceptance],
            ExactlyOnce,
            ExplicitAcknowledgement,
        ),
        (
            vec![ExactlyOnce, DurableAcceptance, ExplicitAcknowledgement],
            ExactlyOnce,
            Transactions,
        ),
    ] {
        assert!(matches!(
            PipeCapabilities::try_new(supported, None),
            Err(ContractError::InvalidCapabilities { capability: c, requires: r })
                if c == capability && r == requires
        ));
    }
}

// Consumer fixtures implement actual state transitions; no default lifecycle stubs.
struct Lifecycle {
    descriptor: ComponentDescriptor,
    running: bool,
}

impl Lifecycle {
    fn new(name: &str, ports: Vec<PortDescriptor>) -> Self {
        Self {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new(name).expect("valid fixture component ID"),
                ports,
            )
            .expect("valid fixture component descriptor"),
            running: false,
        }
    }

    fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.running, "already running");
        self.running = true;
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(self.running, "not running");
        self.running = false;
        Ok(())
    }

    fn require_running(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.running, "not running");
        Ok(())
    }
}

macro_rules! component_lifecycle {
    ($component:ty) => {
        #[async_trait]
        impl ComputationComponent for $component {
            fn descriptor(&self) -> &ComponentDescriptor {
                &self.lifecycle.descriptor
            }
            async fn start(&mut self) -> anyhow::Result<()> {
                self.lifecycle.start()
            }
            async fn stop(&mut self) -> anyhow::Result<()> {
                self.lifecycle.stop()
            }
        }
    };
}

struct CustomSource {
    lifecycle: Lifecycle,
    events: VecDeque<Envelope>,
}
component_lifecycle!(CustomSource);

#[async_trait]
impl EnvelopeSource for CustomSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        self.lifecycle.require_running()?;
        let port = PortId::try_new("out")?;
        Ok(self
            .events
            .pop_front()
            .map(|envelope| OutputEnvelope { port, envelope }))
    }
}

struct CustomTransformer {
    lifecycle: Lifecycle,
    next_sequence: u64,
}
component_lifecycle!(CustomTransformer);

#[async_trait]
impl Transformer for CustomTransformer {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.lifecycle.require_running()?;
        anyhow::ensure!(input.port.as_str() == "in", "wrong port");
        let annotated = input
            .envelope
            .append_context(entry("processed", ContextValue::Bool(true)))?;
        let mut outputs = Vec::new();
        for _ in annotated.changes().operations() {
            let sequence = self.next_sequence;
            self.next_sequence = sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("sequence exhausted"))?;
            outputs.push(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: annotated.derive(
                    EnvelopeId::try_new(
                        "custom.transform",
                        Bytes::copy_from_slice(&sequence.to_be_bytes()),
                    )?,
                    annotated.changes().clone(),
                    SystemMetadata::new(stream("transform/out"), sequence),
                ),
            });
        }
        Ok(outputs)
    }
}

struct CustomSink {
    lifecycle: Lifecycle,
    completion: SinkCompletion,
    observed: Arc<AtomicUsize>,
}
component_lifecycle!(CustomSink);

#[async_trait]
impl EnvelopeSink for CustomSink {
    fn completion(&self) -> SinkCompletion {
        self.completion
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.lifecycle.require_running()?;
        anyhow::ensure!(input.port.as_str() == "in", "wrong port");
        self.observed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn custom_sink(completion: SinkCompletion, observed: Arc<AtomicUsize>) -> Box<dyn EnvelopeSink> {
    Box::new(CustomSink {
        lifecycle: Lifecycle::new(
            "sink",
            vec![port("in", PortDirection::Input, PipeRequirements::default())],
        ),
        completion,
        observed,
    })
}

#[tokio::test]
async fn external_stateful_components_are_object_safe_and_support_zero_one_many_outputs() {
    let mut source: Box<dyn EnvelopeSource> = Box::new(CustomSource {
        lifecycle: Lifecycle::new(
            "source",
            vec![port("out", PortDirection::Output, PipeRequirements::default())],
        ),
        events: VecDeque::from([
            envelope(0, vec![]),
            envelope(1, vec![added(0)]),
            envelope(2, vec![added(0), added(1)]),
        ]),
    });
    let mut transform: Box<dyn Transformer> = Box::new(CustomTransformer {
        lifecycle: Lifecycle::new(
            "transform",
            vec![
                port("in", PortDirection::Input, PipeRequirements::default()),
                port("out", PortDirection::Output, PipeRequirements::default()),
            ],
        ),
        next_sequence: 0,
    });
    let observed = Arc::new(AtomicUsize::new(0));
    let mut sink = custom_sink(SinkCompletion::Handled, observed.clone());
    assert!(source.next().await.is_err());
    source.start().await.unwrap();
    transform.start().await.unwrap();
    sink.start().await.unwrap();
    assert_eq!(transform.descriptor().id().as_str(), "transform");
    let mut next_sequence = 0;
    for count in 0..3 {
        let input = source.next().await.unwrap().unwrap();
        let input_id = input.envelope.id().clone();
        let outputs = transform
            .transform(InputEnvelope {
                port: PortId::try_new("in").unwrap(),
                envelope: input.envelope,
            })
            .await
            .unwrap();
        assert_eq!(outputs.len(), count);
        for output in outputs {
            assert_eq!(output.envelope.system().stream().as_str(), "transform/out");
            assert_eq!(output.envelope.system().sequence(), next_sequence);
            next_sequence += 1;
            assert_eq!(output.envelope.lineage().unwrap().envelope_id(), &input_id);
            assert_eq!(output.envelope.context().len(), 1);
            sink.handle(InputEnvelope {
                port: PortId::try_new("in").unwrap(),
                envelope: output.envelope,
            })
            .await
            .unwrap();
        }
    }
    assert!(source.next().await.unwrap().is_none());
    assert_eq!(observed.load(Ordering::SeqCst), 3);
    source.stop().await.unwrap();
    transform.stop().await.unwrap();
    sink.stop().await.unwrap();
    assert!(sink
        .handle(InputEnvelope {
            port: PortId::try_new("in").unwrap(),
            envelope: envelope(9, vec![])
        })
        .await
        .is_err());
}

#[tokio::test]
async fn accepted_sink_never_upgrades_to_handled_even_with_ack_capable_pipe() {
    use PipeCapability::*;
    let pipe = PipeCapabilities::try_new(
        [
            FifoPerStream,
            DurableAcceptance,
            ExplicitAcknowledgement,
            Replay,
            Transactions,
            ExactlyOnce,
        ],
        NonZeroUsize::new(1),
    )
    .unwrap();
    let accepted_count = Arc::new(AtomicUsize::new(0));
    let handled_count = Arc::new(AtomicUsize::new(0));
    let mut accepted = custom_sink(SinkCompletion::Accepted, accepted_count.clone());
    let mut handled = custom_sink(SinkCompletion::Handled, handled_count.clone());
    for required in [ExplicitAcknowledgement, Transactions, ExactlyOnce] {
        let requirements = PipeRequirements::new([required]);
        assert!(pipe.validate(&requirements).is_ok());
        assert!(matches!(
            validate_sink_completion(accepted.completion(), &requirements),
            Err(ContractError::InsufficientSinkCompletion { actual: SinkCompletion::Accepted, required: r }) if r == required
        ));
        assert!(validate_sink_completion(handled.completion(), &requirements).is_ok());
    }
    assert!(validate_sink_completion(accepted.completion(), &PipeRequirements::default()).is_ok());
    accepted.start().await.unwrap();
    handled.start().await.unwrap();
    for sink in [&mut accepted, &mut handled] {
        sink.handle(InputEnvelope {
            port: PortId::try_new("in").unwrap(),
            envelope: envelope(0, vec![added(0)]),
        })
        .await
        .unwrap();
        sink.stop().await.unwrap();
    }
    assert_eq!(accepted.completion(), SinkCompletion::Accepted);
    assert_eq!(handled.completion(), SinkCompletion::Handled);
    assert_eq!(accepted_count.load(Ordering::SeqCst), 1);
    assert_eq!(handled_count.load(Ordering::SeqCst), 1);
}

struct TestAcknowledgement(Arc<AtomicUsize>);

#[async_trait]
impl Acknowledgement for TestAcknowledgement {
    async fn complete(
        self: Box<Self>,
        outcome: HandlingOutcome,
    ) -> std::result::Result<(), PipeError> {
        if outcome == HandlingOutcome::Handled {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

struct TestSender(mpsc::Sender<Envelope>);

#[async_trait]
impl EnvelopeSender for TestSender {
    async fn send(&self, envelope: Envelope) -> std::result::Result<EnqueueReceipt, SendFailure> {
        let id = envelope.id().clone();
        self.0.send(envelope).await.map_err(|error| SendFailure {
            envelope: error.0,
            error: PipeError::Closed,
        })?;
        Ok(EnqueueReceipt::new(id))
    }
}

struct TestReceiver(mpsc::Receiver<Envelope>);

#[async_trait]
impl EnvelopeReceiver for TestReceiver {
    async fn receive(&mut self) -> std::result::Result<Option<Delivery>, PipeError> {
        Ok(self
            .0
            .recv()
            .await
            .map(|envelope| Delivery::new(envelope, None)))
    }
}

struct TestPipe {
    capabilities: PipeCapabilities,
    sender: Arc<dyn EnvelopeSender>,
    receiver: Option<Box<dyn EnvelopeReceiver>>,
}

impl Pipe for TestPipe {
    fn capabilities(&self) -> &PipeCapabilities {
        &self.capabilities
    }
    fn sender(&self) -> Arc<dyn EnvelopeSender> {
        self.sender.clone()
    }
    fn take_receiver(&mut self) -> std::result::Result<Box<dyn EnvelopeReceiver>, PipeError> {
        self.receiver.take().ok_or(PipeError::ReceiverTaken)
    }
}

#[tokio::test]
async fn transport_acceptance_delivery_and_handling_ack_are_separate() {
    let (tx, rx) = mpsc::channel(2);
    let mut pipe: Box<dyn Pipe> = Box::new(TestPipe {
        capabilities: PipeCapabilities::volatile_bounded(NonZeroUsize::new(2).unwrap()),
        sender: Arc::new(TestSender(tx)),
        receiver: Some(Box::new(TestReceiver(rx))),
    });
    assert_eq!(pipe.capabilities().capacity().unwrap().get(), 2);
    let mut receiver = pipe.take_receiver().unwrap();
    assert!(matches!(
        pipe.take_receiver(),
        Err(PipeError::ReceiverTaken)
    ));
    let sender = pipe.sender();
    let first = Envelope::new(
        event_id(1),
        changes(vec![]),
        SystemMetadata::new(stream("source/out"), 1)
            .with_timestamp(chrono::DateTime::from_timestamp(200, 0).unwrap()),
    );
    let second = Envelope::new(
        event_id(2),
        changes(vec![]),
        SystemMetadata::new(stream("source/out"), 2)
            .with_timestamp(chrono::DateTime::from_timestamp(100, 0).unwrap()),
    );
    let handled = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        sender.send(first.clone()).await.unwrap().envelope_id(),
        first.id()
    );
    sender.send(second).await.unwrap();
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    let delivery = receiver.receive().await.unwrap().unwrap();
    assert_eq!(delivery.envelope().system().sequence(), 1);
    let (received, acknowledgement) = delivery.into_parts();
    assert!(acknowledgement.is_none());
    assert!(Arc::ptr_eq(received.changes(), first.changes()));
    assert_eq!(
        receiver
            .receive()
            .await
            .unwrap()
            .unwrap()
            .envelope()
            .system()
            .sequence(),
        2
    );
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    // An explicit local delivery handle is not an envelope/context property.
    let explicit = Delivery::new(
        received.clone(),
        Some(Box::new(TestAcknowledgement(handled.clone()))),
    );
    drop(Delivery::new(
        received,
        Some(Box::new(TestAcknowledgement(handled.clone()))),
    ));
    assert_eq!(handled.load(Ordering::SeqCst), 0);
    explicit
        .into_parts()
        .1
        .unwrap()
        .complete(HandlingOutcome::Handled)
        .await
        .unwrap();
    assert_eq!(handled.load(Ordering::SeqCst), 1);
    drop(pipe);
    drop(sender);
    assert!(receiver.receive().await.unwrap().is_none());

    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let failure = TestSender(tx).send(first.clone()).await.unwrap_err();
    assert!(matches!(failure.error, PipeError::Closed));
    assert!(Arc::ptr_eq(failure.envelope.changes(), first.changes()));
}

#[tokio::test]
async fn blocked_sender_returns_closed_with_unaccepted_event_when_receiver_drops() {
    let (tx, rx) = mpsc::channel(1);
    let sender: Arc<dyn EnvelopeSender> = Arc::new(TestSender(tx));
    let receiver: Box<dyn EnvelopeReceiver> = Box::new(TestReceiver(rx));
    sender.send(envelope(0, vec![added(0)])).await.unwrap();
    let waiting = envelope(1, vec![added(0)]);
    let mut send = Box::pin(sender.send(waiting.clone()));
    assert!(futures::poll!(send.as_mut()).is_pending());
    drop(receiver);
    let std::task::Poll::Ready(Err(failure)) = futures::poll!(send.as_mut()) else {
        panic!("receiver closure must release a capacity-blocked sender");
    };
    assert!(matches!(failure.error, PipeError::Closed));
    assert_eq!(failure.envelope.id(), waiting.id());
    assert!(Arc::ptr_eq(failure.envelope.changes(), waiting.changes()));
}

#[tokio::test]
async fn cancelling_waiting_receive_keeps_the_next_volatile_event_available() {
    let (tx, rx) = mpsc::channel(1);
    let sender: Arc<dyn EnvelopeSender> = Arc::new(TestSender(tx));
    let mut receiver: Box<dyn EnvelopeReceiver> = Box::new(TestReceiver(rx));
    let mut receive = Box::pin(receiver.receive());
    assert!(futures::poll!(receive.as_mut()).is_pending());
    drop(receive);
    let event = envelope(1, vec![added(0)]);
    sender.send(event.clone()).await.unwrap();
    let delivered = receiver.receive().await.unwrap().unwrap();
    assert_eq!(delivered.envelope().id(), event.id());
    assert!(Arc::ptr_eq(delivered.envelope().changes(), event.changes()));
    let mut receive = Box::pin(receiver.receive());
    assert!(futures::poll!(receive.as_mut()).is_pending());
    drop(sender);
    assert!(matches!(
        futures::poll!(receive.as_mut()),
        std::task::Poll::Ready(Ok(None))
    ));
}

#[test]
fn public_data_and_trait_objects_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    assert_send_sync::<Envelope>();
    assert_send_sync::<ChangeSet>();
    assert_send_sync::<ProcessingContext>();
    assert_send_sync::<dyn RecordValidator>();
    assert_send_sync::<dyn Transformer>();
    assert_send_sync::<dyn EnvelopeSource>();
    assert_send_sync::<dyn EnvelopeSink>();
    assert_send_sync::<dyn Pipe>();
    assert_send_sync::<dyn EnvelopeSender>();
    assert_send_sync::<dyn EnvelopeReceiver>();
    assert_send_sync::<dyn Acknowledgement>();
}
