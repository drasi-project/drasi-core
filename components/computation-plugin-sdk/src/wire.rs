// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Version-1 payloads. Discovery/configuration use JSON. Operation payloads use
//! named MessagePack fields; envelope bytes are exactly EnvelopeCodec format 1.

use crate::{abi, metadata::Capabilities};
use drasi_lib::computation::v1::{
    ChangeEnvelope, ComponentDescriptor, ComponentId, EnvelopeCodec, ImplementationIdentity,
    InputEnvelope, OutputEnvelope, PortId, RecordId, Schema, SchemaDescriptor,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{num::NonZeroUsize, sync::Arc};

pub fn encode<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    let bytes = rmp_serde::to_vec_named(value)?;
    anyhow::ensure!(
        bytes.len() <= abi::MAX_MESSAGE_BYTES,
        "native message exceeds size limit"
    );
    Ok(bytes)
}
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> anyhow::Result<T> {
    anyhow::ensure!(
        bytes.len() <= abi::MAX_MESSAGE_BYTES,
        "native message exceeds size limit"
    );
    Ok(rmp_serde::from_slice(bytes)?)
}
pub fn codec(schemas: &[Arc<Schema>]) -> anyhow::Result<EnvelopeCodec> {
    let mut codec =
        EnvelopeCodec::new(NonZeroUsize::new(abi::MAX_MESSAGE_BYTES).expect("constant"));
    for schema in schemas {
        codec.register_schema(schema.clone())?;
    }
    Ok(codec)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub instance_id: String,
    pub graph_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    pub id: ComponentId,
    pub implementation: ImplementationIdentity,
    pub configuration_version: u32,
    pub configuration: serde_json::Value,
    pub scope: Option<Scope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceMetadata {
    pub descriptor: ComponentDescriptor,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub port: PortId,
    pub envelope: Vec<u8>,
}
impl Envelope {
    pub fn input(input: &InputEnvelope, codec: &EnvelopeCodec) -> anyhow::Result<Self> {
        Ok(Self {
            port: input.port.clone(),
            envelope: codec.encode(&input.envelope)?.to_vec(),
        })
    }
    pub fn output(output: &OutputEnvelope, codec: &EnvelopeCodec) -> anyhow::Result<Self> {
        Ok(Self {
            port: output.port.clone(),
            envelope: codec.encode(&output.envelope)?.to_vec(),
        })
    }
    pub fn into_input(self, codec: &EnvelopeCodec) -> anyhow::Result<InputEnvelope> {
        Ok(InputEnvelope {
            port: self.port,
            envelope: codec.decode(&self.envelope)?,
        })
    }
    pub fn into_output(self, codec: &EnvelopeCodec) -> anyhow::Result<OutputEnvelope> {
        Ok(OutputEnvelope {
            port: self.port,
            envelope: codec.decode(&self.envelope)?,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputBatch {
    pub outputs: Vec<Envelope>,
    pub pending: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordValidation {
    pub schema: SchemaDescriptor,
    pub namespace: String,
    pub identity: Vec<u8>,
    /// None is an identity-only reference; 0=full, 1=patch, 2=partial.
    pub image: Option<u32>,
    pub payload: Vec<u8>,
}
impl RecordValidation {
    pub fn identity(&self) -> anyhow::Result<RecordId> {
        Ok(RecordId::try_new(
            self.namespace.as_str(),
            bytes::Bytes::copy_from_slice(&self.identity),
        )?)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeriveRequest {
    pub input: Vec<u8>,
    pub output: Vec<u8>,
}

pub fn check_port(
    descriptor: &ComponentDescriptor,
    port: &PortId,
    envelope: &ChangeEnvelope,
    direction: drasi_lib::computation::v1::PortDirection,
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
