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

#[allow(dead_code)]
mod computation_support;

use std::{num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use computation_support::*;
use drasi_lib::computation::v1::*;

fn codec(limit: usize) -> EnvelopeCodec {
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(limit).expect("positive limit"));
    let descriptor = schema_descriptor();
    codec
        .register_schema(Arc::new(Schema::new(
            descriptor.clone(),
            Arc::new(ReadingValidator(descriptor)),
        )))
        .expect("register schema");
    codec
}

#[test]
fn envelope_codec_preserves_records_opaque_ids_metadata_lineage_and_annotations() {
    let codec = codec(16_384);
    let timestamp = chrono::DateTime::from_timestamp(123, 123456789).expect("timestamp");
    let mut original = ChangeEnvelope::new(
        EnvelopeId::try_new("opaque", Bytes::from_static(&[0xff, 0, 0x80])).expect("identity"),
        changes("source", 7, &[1, 65535]),
        SystemMetadata::new(stream("source"), 7)
            .with_timestamp(timestamp)
            .with_source_position(Bytes::from_static(&[0xff, 0, 1])),
    );
    original
        .append_annotation(annotation("source"))
        .expect("source annotation");
    let mut envelope = original.derive(
        emission_id(&stream("transform"), 1).expect("id"),
        changes("transform", 1, &[2, 4]),
        SystemMetadata::new(stream("transform"), 1),
    );
    for value in [
        ContextValue::Bool(false),
        ContextValue::Signed(i64::MIN),
        ContextValue::Unsigned(u64::MAX),
        ContextValue::String(Arc::from("unicode \u{2603}")),
        ContextValue::Bytes(Arc::from([0xff, 0, 1])),
    ] {
        envelope
            .append_annotation(
                ContextEntry::try_new(component("transform"), "same-key", value).expect("entry"),
            )
            .expect("append");
    }
    let encoded = codec.encode(&envelope).expect("encode");
    let decoded = codec
        .decode(&encoded)
        .expect("decode through registered validators");
    assert_eq!(
        codec.encode(&decoded).expect("canonical re-encode"),
        encoded
    );
    assert_eq!(decoded.id(), envelope.id());
    assert_eq!(
        decoded.changes().operations(),
        envelope.changes().operations()
    );
    assert_eq!(
        decoded.annotations().entries().collect::<Vec<_>>(),
        envelope.annotations().entries().collect::<Vec<_>>()
    );
    let ancestor = decoded.lineage().expect("input lineage retained");
    assert_eq!(ancestor.envelope_id(), original.id());
    assert_eq!(ancestor.system().as_ref(), original.system().as_ref());
    let mut branch = decoded.clone();
    branch
        .append_annotation(annotation("sink"))
        .expect("branch annotation");
    assert!(Arc::ptr_eq(branch.event(), decoded.event()));
    assert_eq!(branch.annotations().len(), decoded.annotations().len() + 1);
    assert_eq!(codec.encode(&decoded).expect("sibling unchanged"), encoded);
}

#[test]
fn decoding_cannot_bypass_schema_identity_image_or_ordinal_validation() {
    let codec = codec(16_384);
    let encoded = codec.encode(&root("source", 1, &[2, 4])).expect("encode");
    for fault in [
        "version",
        "schema",
        "definition",
        "identity",
        "image",
        "ordinal",
        "unknown-field",
        "annotation",
    ] {
        let mut value: serde_json::Value = serde_json::from_slice(&encoded).expect("stored frame");
        match fault {
            "version" => value["format"] = serde_json::json!(2),
            "schema" => value["schema"]["id"] = serde_json::json!("unknown"),
            "definition" => value["schema"]["definition"] = serde_json::json!([1, 2, 3]),
            "identity" => {
                value["operations"][0]["Add"]["after"]["identity"]["value"] =
                    serde_json::json!([99])
            }
            "image" => value["operations"][0]["Add"]["after"]["kind"] = serde_json::json!("Patch"),
            "ordinal" => {
                value["operations"][1]["Add"]["ordinal"] =
                    value["operations"][0]["Add"]["ordinal"].clone()
            }
            "unknown-field" => value["unrecognized"] = serde_json::json!(true),
            "annotation" => {
                value["annotations"][0]["contributor"] = serde_json::json!("invalid id")
            }
            _ => unreachable!(),
        }
        let bytes = serde_json::to_vec(&value).expect("tampered frame");
        assert!(codec.decode(&bytes).is_err(), "{fault}");
    }
}

#[test]
fn encode_and_decode_limits_are_enforced_without_silent_truncation() {
    let envelope = root("source", 1, &[1, 2]);
    assert!(matches!(
        codec(8).encode(&envelope),
        Err(EnvelopeCodecError::SizeLimit { .. })
    ));
    let encoded = codec(16_384).encode(&envelope).expect("encode");
    assert!(matches!(
        codec(encoded.len() - 1).decode(&encoded),
        Err(EnvelopeCodecError::SizeLimit { .. })
    ));
    assert!(codec(16_384).decode(&encoded[..encoded.len() - 1]).is_err());
}

#[test]
fn codec_preserves_identity_only_deletes_and_replace_before_images() {
    let schema = Schema::new(
        schema_descriptor(),
        Arc::new(ReadingValidator(schema_descriptor())),
    );
    let identity = RecordId::try_new("readings", Bytes::from_static(&[1])).expect("id");
    let before = Record::try_new(
        &schema,
        identity.clone(),
        RecordImage::Full,
        Bytes::from_static(&[1, 0, 3]),
    )
    .expect("before");
    let after = Record::try_new(
        &schema,
        identity.clone(),
        RecordImage::Full,
        Bytes::from_static(&[1, 0, 4]),
    )
    .expect("after");
    let changes = ChangeSet::try_new(
        ChangeSetId::try_new("changes", Bytes::from_static(b"updates")).expect("id"),
        schema_descriptor(),
        vec![
            ChangeOperation::Updated {
                ordinal: 1,
                before: Some(before),
                after,
                semantics: UpdateSemantics::Replace,
            },
            ChangeOperation::Deleted {
                ordinal: 2,
                identity: RecordReference::try_new(&schema, identity).expect("validated key"),
                before: None,
            },
        ],
    )
    .expect("changes");
    let envelope = ChangeEnvelope::new(
        emission_id(&stream("source"), 1).expect("id"),
        changes,
        SystemMetadata::new(stream("source"), 1),
    );
    let codec = codec(16_384);
    let decoded = codec
        .decode(&codec.encode(&envelope).expect("encode"))
        .expect("decode");
    assert_eq!(
        decoded.changes().operations(),
        envelope.changes().operations()
    );
}
