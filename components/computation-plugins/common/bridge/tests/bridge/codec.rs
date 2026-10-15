// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_bridge_common::{codec::preflight_messagepack, PortableEnvelopeCodec};
use serde::Serialize;

#[tokio::test]
async fn portable_standard_schemas_preserve_exact_binary_context_and_transactions() -> Result<()> {
    for (schema, envelope) in [
        (
            PortableSchema::GraphChange,
            graph_input(u64::MAX, i64::MIN)?,
        ),
        (PortableSchema::QueryChange, query_input(7)?),
        (PortableSchema::SourceTransaction, transaction_input(13)?),
    ] {
        let codec = PortableEnvelopeCodec::new(schema, namespace(), Limits::default())?;
        let bytes = codec.encode(&envelope)?;
        let decoded = codec.decode(&bytes)?;
        assert_eq!(codec.encode(&decoded)?, bytes);
        assert_eq!(decoded.system(), envelope.system());
        assert_eq!(
            decoded.changes().operations(),
            envelope.changes().operations()
        );
        assert_eq!(
            decoded.annotations().entries().collect::<Vec<_>>(),
            envelope.annotations().entries().collect::<Vec<_>>()
        );
        if schema == PortableSchema::SourceTransaction {
            let transaction = SourceTransactionCodec::decode(
                &decoded,
                SourceTransactionLimits {
                    max_changes: NonZeroUsize::new(2).unwrap(),
                    max_bytes: NonZeroUsize::new(65_536).unwrap(),
                    max_duration_ms: std::num::NonZeroU64::new(1_000).unwrap(),
                },
            )?;
            assert_eq!(transaction.position(), b"commit/42");
            assert_eq!(transaction.into_changes().len(), 2);
        }
    }
    Ok(())
}

#[test]
fn namespace_and_wire_versions_do_not_load_or_substitute_a_validator() -> Result<()> {
    let codec =
        PortableEnvelopeCodec::new(PortableSchema::GraphChange, namespace(), Limits::default())?;
    let other_namespace = Namespace {
        version: 1,
        source_prefixes: vec!["elsewhere/".into()],
        query_prefixes: vec![],
    };
    let other = PortableEnvelopeCodec::new(
        PortableSchema::GraphChange,
        other_namespace,
        Limits::default(),
    )?;
    let bytes = codec.encode(&graph_input(1, 1)?)?;
    assert!(other.decode(&bytes).is_err());
    let query =
        PortableEnvelopeCodec::new(PortableSchema::QueryChange, namespace(), Limits::default())?;
    assert!(query.decode(&bytes).is_err());
    let mut broken = bytes.clone();
    broken.push(0xc0);
    assert!(codec.decode(&broken).is_err());
    Ok(())
}

#[test]
fn hostile_messagepack_is_rejected_iteratively_before_deserialization() {
    let mut nested = vec![0x91; 20_000];
    nested.push(0);
    assert!(preflight_messagepack(&nested, 100_000, 100_000).is_err());
    assert!(preflight_messagepack(&[0xdd, 0xff, 0xff, 0xff, 0xff], 100, 100).is_err());
    assert!(preflight_messagepack(&[0xdf, 0x80, 0, 0, 0], 100, 100).is_err());
    assert!(preflight_messagepack(&[0xc6, 0xff, 0xff, 0xff, 0xff], 100, 100).is_err());
    assert!(preflight_messagepack(&[0x91, 0], 2, 2).is_ok());
    assert!(preflight_messagepack(&[0x91, 0], 2, 1).is_err());
}

#[derive(Serialize)]
struct Id<'a> {
    namespace: &'a str,
    #[serde(with = "serde_bytes")]
    value: &'a [u8],
}

#[derive(Serialize)]
struct Image<'a> {
    identity: Id<'a>,
    kind: u8,
    #[serde(with = "serde_bytes")]
    bytes: &'a [u8],
}

#[derive(Serialize)]
enum Op<'a> {
    Add { ordinal: u64, after: Image<'a> },
}

#[derive(Serialize)]
struct Descriptor<'a> {
    id: &'a str,
    version: u32,
    encoding: &'a str,
    #[serde(with = "serde_bytes")]
    definition: &'a [u8],
}

#[derive(Clone, Serialize)]
struct Metadata {
    stream: &'static str,
    sequence: u64,
    timestamp: Option<String>,
    source_position: Option<Vec<u8>>,
}

#[derive(Serialize)]
struct RawEnvelope<'a> {
    format: u32,
    id: Id<'a>,
    change_set: Id<'a>,
    schema: Descriptor<'a>,
    operations: Vec<Op<'a>>,
    system: Metadata,
    lineage: Vec<(Id<'a>, Metadata)>,
    context_identity: u64,
    annotations: Vec<()>,
}

pub(super) fn hostile_envelope(depth: usize, lineage: usize) -> Result<Vec<u8>> {
    let reference = ElementReference::new("plant/db", "deep");
    let metadata = ElementMetadata {
        reference: reference.clone(),
        labels: Arc::from([]),
        effective_from: 1,
    };
    let mut payload = vec![];
    payload.extend_from_slice(&0u32.to_le_bytes()); // insert
    payload.extend_from_slice(&0u32.to_le_bytes()); // node
    payload.extend(bincode::serialize(&metadata)?);
    payload.extend_from_slice(&1u64.to_le_bytes()); // one property
    payload.extend(bincode::serialize("deep")?);
    for _ in 0..depth {
        payload.extend_from_slice(&5u32.to_le_bytes()); // list
        payload.extend_from_slice(&1u64.to_le_bytes());
    }
    payload.extend_from_slice(&0u32.to_le_bytes()); // null
    let reference = bincode::serialize(&reference)?;
    let schema = GraphChangeCodec::schema();
    let schema = schema.descriptor();
    let metadata = Metadata {
        stream: "sender/out",
        sequence: 1,
        timestamp: None,
        source_position: None,
    };
    let frame = RawEnvelope {
        format: 2,
        id: Id {
            namespace: "sender/out",
            value: b"id",
        },
        change_set: Id {
            namespace: "sender/out",
            value: b"changes",
        },
        schema: Descriptor {
            id: schema.id().as_str(),
            version: 1,
            encoding: schema.encoding(),
            definition: schema.definition(),
        },
        operations: vec![Op::Add {
            ordinal: 0,
            after: Image {
                identity: Id {
                    namespace: "drasi.graph-element",
                    value: &reference,
                },
                kind: 0,
                bytes: &payload,
            },
        }],
        system: metadata.clone(),
        lineage: (0..lineage)
            .map(|_| {
                (
                    Id {
                        namespace: "sender/out",
                        value: b"ancestor",
                    },
                    metadata.clone(),
                )
            })
            .collect(),
        context_identity: 1,
        annotations: vec![],
    };
    Ok(encode_bounded_messagepack(&frame, 1 << 20)?)
}

#[test]
fn opaque_bincode_depth_and_lineage_are_checked_before_recursive_construction_or_drop() -> Result<()>
{
    let codec =
        PortableEnvelopeCodec::new(PortableSchema::GraphChange, namespace(), Limits::default())?;
    assert!(codec.decode(&hostile_envelope(0, 0)?).is_ok());
    assert!(codec.decode(&hostile_envelope(8, 3)?).is_ok());
    assert!(codec.decode(&hostile_envelope(20_000, 0)?).is_err());
    assert!(codec.decode(&hostile_envelope(0, 2_000)?).is_err());
    Ok(())
}

#[test]
fn exact_envelope_byte_limit_and_annotation_limits_reject_next_unit() -> Result<()> {
    let codec =
        PortableEnvelopeCodec::new(PortableSchema::GraphChange, namespace(), Limits::default())?;
    let mut envelope = graph_input(1, 1)?;
    envelope.append_annotation(ContextEntry::try_new(
        ComponentId::try_new("test")?,
        "bytes",
        ContextValue::Bytes(Arc::from(vec![1u8; 2048])),
    )?)?;
    let length = codec.encode(&envelope)?.len();
    let exact = PortableEnvelopeCodec::new(
        PortableSchema::GraphChange,
        namespace(),
        Limits {
            max_envelope_bytes: length,
            ..Limits::default()
        },
    )?;
    let bytes = exact.encode(&envelope)?;
    assert_eq!(exact.encode(&exact.decode(&bytes)?)?.len(), length);
    let short = PortableEnvelopeCodec::new(
        PortableSchema::GraphChange,
        namespace(),
        Limits {
            max_envelope_bytes: length - 1,
            ..Limits::default()
        },
    )?;
    assert!(short.encode(&envelope).is_err());
    assert!(short.decode(&bytes).is_err());
    let restricted = PortableEnvelopeCodec::new(
        PortableSchema::GraphChange,
        namespace(),
        Limits {
            max_annotations: 2,
            ..Limits::default()
        },
    )?;
    assert!(restricted.decode(&bytes).is_err());
    Ok(())
}
