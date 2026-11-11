// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Version-2 payloads. Discovery/configuration use JSON. Operation payloads use
//! bounded named MessagePack with binary buffers and binary computation envelopes.

use crate::{abi, metadata::Capabilities};
use drasi_computation_contracts::{
    decode_bounded_messagepack, encode_bounded_messagepack, BinaryEnvelopeCodec, ChangeEnvelope,
    ComponentDescriptor, ComponentId, ComponentUpdatePlan, ImplementationIdentity, InputBinding,
    InputEnvelope, OutputEnvelope, PortId, RecordId, Schema, SchemaDescriptor,
};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, num::NonZeroUsize, sync::Arc};

pub fn encode<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    Ok(encode_bounded_messagepack(value, abi::MAX_MESSAGE_BYTES)?)
}

pub fn encode_buffer(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    encode(&serde_bytes::Bytes::new(bytes))
}
pub fn decode<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> anyhow::Result<T> {
    Ok(decode_bounded_messagepack(bytes, abi::MAX_MESSAGE_BYTES)?)
}
pub fn codec(schemas: &[Arc<Schema>]) -> anyhow::Result<BinaryEnvelopeCodec> {
    let mut codec =
        BinaryEnvelopeCodec::new(NonZeroUsize::new(abi::MAX_MESSAGE_BYTES).expect("constant"));
    for schema in schemas {
        codec.register_schema(schema.clone())?;
    }
    Ok(codec)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub instance_id: String,
    pub graph_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    pub id: ComponentId,
    pub implementation: ImplementationIdentity,
    pub configuration_version: u32,
    pub configuration: serde_json::Value,
    pub scope: Option<Scope>,
    /// The graph's declared incoming edges, supplied only to factories whose
    /// capabilities set `binds_inputs`. Each names the receiving input port, the
    /// producer's actual output port and that output's stream.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InputBinding>,
    /// Local SDK annotation set only by the negotiated activation-credentials
    /// creation envelope. Not a new field in the frozen base wire request.
    #[serde(skip)]
    pub activation_credentials_bound: bool,
    /// Local execution annotation populated only by the separately tagged
    /// reconstruction envelope. Never serialized as a planning description or
    /// configuration field. Initial construction has no update directive.
    #[serde(skip)]
    pub update_directive: Option<UpdateDirective>,
}

/// Persisted, owner-approved reconstruction intent, not a pure planning input or
/// permission to discard state. The factory must honor the requested state
/// treatment or reject construction; Core validates actual data safety.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateDirective {
    pub operation_id: String,
    pub plan: ComponentUpdatePlan,
}

impl UpdateDirective {
    pub fn validate(&self) -> anyhow::Result<()> {
        drasi_computation_contracts::validate_identifier(
            "native update operation",
            &self.operation_id,
        )?;
        anyhow::ensure!(
            self.plan.mode == drasi_computation_contracts::ComponentUpdateMode::Recreate,
            "native reconstruction directive requires a Recreate plan"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateCreation {
    update_creation_version: u32,
    request: CreateRequest,
    directive: UpdateDirective,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    activation_credentials_version: Option<u32>,
}

/// Encode actual construction, retaining the negotiated credentials marker and
/// optional owner-approved reconstruction directive in execution-only envelopes.
/// Use ordinary serde JSON for pure UpdateRequest planning descriptions.
pub fn encode_create_request(request: &CreateRequest) -> anyhow::Result<Vec<u8>> {
    if let Some(directive) = &request.update_directive {
        directive.validate()?;
        return encode(&UpdateCreation {
            update_creation_version: 1,
            request: request.clone(),
            directive: directive.clone(),
            activation_credentials_version: request
                .activation_credentials_bound
                .then_some(abi::activation_credentials::VERSION),
        });
    }
    if request.activation_credentials_bound {
        crate::activation_credentials::encode_bound_create_request(request)
    } else {
        encode(request)
    }
}

pub(crate) fn decode_update_creation(bytes: &[u8]) -> anyhow::Result<CreateRequest> {
    let mut creation: UpdateCreation = decode(bytes)?;
    anyhow::ensure!(
        creation.update_creation_version == 1
            && creation
                .activation_credentials_version
                .is_none_or(|version| version == abi::activation_credentials::VERSION),
        "unsupported native reconstruction envelope version"
    );
    creation.directive.validate()?;
    creation.request.activation_credentials_bound =
        creation.activation_credentials_version.is_some();
    creation.request.update_directive = Some(creation.directive);
    Ok(creation.request)
}

/// Pure factory inspection of two configurations for the same logical component.
/// UUID, scope, implementation and configuration version must match. Factory
/// planning uses bounded JSON, not a component operation or construction callback.
/// Current-request matching excludes both execution-only creation annotations;
/// a retained reconstruction directive never authorizes another update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequest {
    pub current: CreateRequest,
    pub proposed: CreateRequest,
}

/// Approved plugin-authored intent for a retained object. The caller must first
/// validate actual data boundaries and storage/downstream requirements; this
/// payload and the factory's metadata cannot establish them. Encoded as bounded
/// named MessagePack when passed to the mutable APPLY_UPDATE operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyUpdateRequest {
    /// Stable host operation identity, unchanged for retries of this intent.
    pub operation_id: String,
    pub request: UpdateRequest,
    pub plan: ComponentUpdatePlan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceMetadata {
    pub descriptor: ComponentDescriptor,
    pub capabilities: Capabilities,
    /// Descriptions captured at construction or the last successfully validated
    /// APPLY_UPDATE. Re-inspect after update success; an earlier decoded metadata
    /// value is a snapshot and never updates itself. Reader identities stay fixed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub queries: std::collections::BTreeMap<PortId, drasi_computation_contracts::QueryDescription>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope<'a> {
    pub port: PortId,
    #[serde(borrow, with = "serde_bytes")]
    pub envelope: Cow<'a, [u8]>,
}
impl Envelope<'_> {
    pub fn input(input: &InputEnvelope, codec: &BinaryEnvelopeCodec) -> anyhow::Result<Self> {
        Ok(Self {
            port: input.port.clone(),
            envelope: Cow::Owned(codec.encode(&input.envelope)?),
        })
    }
    pub fn output(output: &OutputEnvelope, codec: &BinaryEnvelopeCodec) -> anyhow::Result<Self> {
        Ok(Self {
            port: output.port.clone(),
            envelope: Cow::Owned(codec.encode(&output.envelope)?),
        })
    }
    pub fn into_input(self, codec: &BinaryEnvelopeCodec) -> anyhow::Result<InputEnvelope> {
        Ok(InputEnvelope {
            execution_context: Default::default(),
            port: self.port,
            envelope: codec.decode(&self.envelope)?,
        })
    }
    pub fn into_output(self, codec: &BinaryEnvelopeCodec) -> anyhow::Result<OutputEnvelope> {
        Ok(OutputEnvelope {
            execution_context: Default::default(),
            port: self.port,
            envelope: codec.decode(&self.envelope)?,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputBatch<'a> {
    #[serde(borrow)]
    pub outputs: Vec<Envelope<'a>>,
    pub pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_observation: Option<crate::output_reads::WireQueryObservation>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordValidation<'a> {
    pub schema: SchemaDescriptor,
    #[serde(borrow)]
    pub namespace: Cow<'a, str>,
    #[serde(borrow, with = "serde_bytes")]
    pub identity: Cow<'a, [u8]>,
    /// None is an identity-only reference; 0=full, 1=patch, 2=partial.
    pub image: Option<u32>,
    #[serde(borrow, with = "serde_bytes")]
    pub payload: Cow<'a, [u8]>,
}
impl RecordValidation<'_> {
    pub fn identity(&self) -> anyhow::Result<RecordId> {
        Ok(RecordId::try_new(
            self.namespace.as_ref(),
            bytes::Bytes::copy_from_slice(&self.identity),
        )?)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeriveRequest<'a> {
    #[serde(borrow, with = "serde_bytes")]
    pub input: Cow<'a, [u8]>,
    #[serde(borrow, with = "serde_bytes")]
    pub output: Cow<'a, [u8]>,
}

pub fn check_port(
    descriptor: &ComponentDescriptor,
    port: &PortId,
    envelope: &ChangeEnvelope,
    direction: drasi_computation_contracts::PortDirection,
) -> anyhow::Result<()> {
    let declared = descriptor
        .ports()
        .iter()
        .find(|p| p.id() == port)
        .ok_or_else(|| anyhow::anyhow!("unknown native component port {port}"))?;
    anyhow::ensure!(
        declared.direction() == direction && declared.schema() == envelope.changes().schema(),
        "native port direction/schema mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{GraphChangeCodec, StreamId};

    #[test]
    fn envelope_operation_uses_borrowed_binary_payload_and_current_wire_version() {
        assert_eq!(abi::WIRE_VERSION, BinaryEnvelopeCodec::VERSION);
        let codec = codec(&[GraphChangeCodec::schema()]).unwrap();
        let output = OutputEnvelope {
            execution_context: Default::default(),
            port: PortId::try_new("out").unwrap(),
            envelope: GraphChangeCodec::encode_changes(
                &[],
                StreamId::try_new("source").unwrap(),
                1,
                None,
            )
            .unwrap(),
        };
        let packet = encode(&Envelope::output(&output, &codec).unwrap()).unwrap();
        let decoded: Envelope<'_> = decode(&packet).unwrap();
        assert!(matches!(decoded.envelope, Cow::Borrowed(_)));
        let restored = decoded.into_output(&codec).unwrap();
        assert_eq!(restored.envelope.id(), output.envelope.id());
        assert_eq!(
            restored.envelope.system().as_ref(),
            output.envelope.system().as_ref()
        );
        assert_eq!(restored.port, output.port);
        drop(packet);
        assert!(
            codec.encode(&restored.envelope).is_ok(),
            "restored data owns its storage"
        );
    }

    #[test]
    fn batches_validation_and_transaction_buffers_are_binary_and_borrowed() {
        let bytes = [0u8, 127, 128, 255];
        let request = RecordValidation {
            schema: GraphChangeCodec::schema().descriptor().clone(),
            namespace: Cow::Borrowed("record"),
            identity: Cow::Borrowed(&bytes),
            image: Some(0),
            payload: Cow::Borrowed(&bytes),
        };
        let packet = encode(&request).unwrap();
        let decoded: RecordValidation<'_> = decode(&packet).unwrap();
        assert!(matches!(decoded.identity, Cow::Borrowed(_)));
        assert!(matches!(decoded.payload, Cow::Borrowed(_)));
        assert_eq!(decoded.payload.as_ref(), &bytes);
        let request = DeriveRequest {
            input: Cow::Borrowed(&bytes),
            output: Cow::Borrowed(&bytes),
        };
        let packet = encode(&request).unwrap();
        let decoded: DeriveRequest<'_> = decode(&packet).unwrap();
        assert!(matches!(decoded.input, Cow::Borrowed(_)));
        assert!(matches!(decoded.output, Cow::Borrowed(_)));
        let packet = encode_buffer(&bytes).unwrap();
        assert_eq!(packet, [0xc4, 4, 0, 127, 128, 255]);
        assert_eq!(
            decode::<serde_bytes::ByteBuf>(&packet).unwrap().as_ref(),
            &bytes
        );
        let batch = OutputBatch {
            outputs: vec![Envelope {
                port: PortId::try_new("out").unwrap(),
                envelope: Cow::Borrowed(&bytes),
            }],
            pending: true,
            query_observation: None,
        };
        let packet = encode(&batch).unwrap();
        let decoded: OutputBatch<'_> = decode(&packet).unwrap();
        assert!(decoded.pending);
        assert!(matches!(decoded.outputs[0].envelope, Cow::Borrowed(_)));
    }

    #[test]
    fn operation_messages_reject_extra_and_impossible_values() {
        let mut bytes = encode(&true).unwrap();
        bytes.push(0xc0);
        assert!(decode::<bool>(&bytes).is_err());
        assert!(decode::<Vec<Envelope<'_>>>(&[0xdd, 255, 255, 255, 255]).is_err());
        assert!(decode::<serde_bytes::ByteBuf>(&[0xc6, 255, 255, 255, 255]).is_err());
    }
}
