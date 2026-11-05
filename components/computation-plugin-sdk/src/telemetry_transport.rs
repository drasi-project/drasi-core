// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Negotiated context capsules. Event bytes and all base wire records are unchanged.

use crate::{abi, wire, CreateRequest, FactoryMetadata};
use drasi_computation_contracts::{
    telemetry::{ExecutionContext, ExecutionScope, TelemetryScope},
    ComponentSemanticKind,
};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

const CREATION_PREFIX: &[u8] = b"DTC1";
pub const CONTEXT_FRAME_OVERHEAD: usize = crate::telemetry::MAX_EXECUTION_CONTEXT_BYTES + 128;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Creation {
    execution_context: ByteBuf,
    semantic_kind: Option<ComponentSemanticKind>,
    request: ByteBuf,
}

pub fn encode_creation(request: &[u8]) -> anyhow::Result<Vec<u8>> {
    let component = ExecutionScope::current_component();
    let frame = Creation {
        execution_context: ExecutionContext::current().encode()?.into(),
        semantic_kind: component.and_then(|component| component.identity.semantic_kind()),
        request: request.to_vec().into(),
    };
    let mut bytes = CREATION_PREFIX.to_vec();
    bytes.extend(wire::encode(&frame)?);
    anyhow::ensure!(
        bytes.len() <= abi::MAX_MESSAGE_BYTES,
        "contextual creation exceeds frame limit"
    );
    Ok(bytes)
}

pub(crate) fn decode_creation(
    bytes: &[u8],
    factory: &FactoryMetadata,
) -> anyhow::Result<(CreateRequest, ExecutionScope, bool)> {
    let creation: Option<Creation> = bytes
        .strip_prefix(CREATION_PREFIX)
        .map(wire::decode)
        .transpose()?;
    let request = crate::activation_credentials::decode_create_request(
        creation
            .as_ref()
            .map_or(bytes, |frame| frame.request.as_ref()),
    )?;
    let context = creation
        .as_ref()
        .map(|frame| ExecutionContext::decode(&frame.execution_context))
        .transpose()?
        .unwrap_or_default();
    let binding = ExecutionScope::current_plugin_telemetry();
    let mut scope = ExecutionScope::new(context)
        .with_plugin_telemetry(binding.clone())
        .with_disabled_plugin_metrics(binding.is_none());
    if let Some(binding) = &binding {
        let config = binding.config();
        anyhow::ensure!(
            config.scope.component_id() == Some(&request.id)
                && config.scope.role() == Some(factory.role),
            "native telemetry binding does not match the constructed component"
        );
        if let Some(request_scope) = &request.scope {
            anyhow::ensure!(
                request_scope.instance_id == config.scope.instance_id()
                    && request_scope.graph_id == config.scope.graph_id()
                    && request_scope.generation == config.generation,
                "native telemetry binding does not match construction scope"
            );
        }
        scope = scope.with_component(config.scope.clone(), config.generation);
    } else if let Some(binding) = &request.scope {
        let descriptor = factory.descriptor(request.id.clone())?;
        scope = scope.with_component(
            TelemetryScope::graph(binding.instance_id.as_str(), binding.graph_id.as_str())?
                .component(
                    request.id.clone(),
                    factory.role,
                    creation
                        .as_ref()
                        .and_then(|frame| frame.semantic_kind)
                        .unwrap_or_else(|| {
                            ComponentSemanticKind::for_component(factory.role, &descriptor)
                        }),
                ),
            binding.generation,
        );
    }
    if binding.is_some() {
        scope = scope.with_adapter(Some(std::sync::Arc::new(
            crate::telemetry_capture::NativeContextAdapter,
        )));
    } else {
        #[cfg(feature = "otel-context")]
        {
            scope = scope.with_adapter(Some(std::sync::Arc::new(
                crate::telemetry::OpenTelemetryContextAdapter,
            )));
        }
    }
    Ok((request, scope, creation.is_some()))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    operation: Option<ByteBuf>,
    input_count: usize,
    inputs: Vec<ContextEntry>,
    payload: ByteBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    version: u32,
    output_count: usize,
    outputs: Vec<ContextEntry>,
    payload: ByteBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextEntry {
    index: usize,
    context: ByteBuf,
}

#[derive(Debug)]
pub struct OperationContexts {
    pub operation: ExecutionContext,
    pub inputs: Vec<ExecutionContext>,
}

impl OperationContexts {
    pub fn require_inputs(&self, total: usize) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.inputs.len() == total,
            "native input context count mismatch"
        );
        Ok(())
    }

    pub fn input(&self, index: usize, total: usize) -> anyhow::Result<ExecutionContext> {
        self.require_inputs(total)?;
        self.inputs
            .get(index)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("native input context index out of range"))
    }

    pub fn require_empty_inputs(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.inputs.is_empty(), "unexpected native input contexts");
        Ok(())
    }
}

pub struct DecodedRequest {
    pub contexts: OperationContexts,
    pub payload: Vec<u8>,
}

pub struct DecodedReply {
    pub outputs: Vec<ExecutionContext>,
    pub payload: Vec<u8>,
}

fn encode_contexts(contexts: &[ExecutionContext]) -> anyhow::Result<Vec<ContextEntry>> {
    anyhow::ensure!(
        contexts.len() <= abi::telemetry::MAX_CONTEXTS,
        "too many native execution contexts"
    );
    contexts
        .iter()
        .enumerate()
        .filter(|(_, context)| !context.is_empty())
        .map(|(index, context)| {
            Ok(ContextEntry {
                index,
                context: context.encode()?.into(),
            })
        })
        .collect()
}

fn decode_contexts(
    count: usize,
    contexts: Vec<ContextEntry>,
) -> anyhow::Result<Vec<ExecutionContext>> {
    anyhow::ensure!(
        count <= abi::telemetry::MAX_CONTEXTS && contexts.len() <= count,
        "too many native execution contexts"
    );
    let mut result = vec![ExecutionContext::default(); count];
    let mut previous = None;
    for entry in contexts {
        anyhow::ensure!(
            entry.index < count && previous.is_none_or(|index| entry.index > index),
            "native context indices must be unique, ordered and in range"
        );
        let context = ExecutionContext::decode(&entry.context)?;
        anyhow::ensure!(!context.is_empty(), "redundant empty native context entry");
        result[entry.index] = context;
        previous = Some(entry.index);
    }
    Ok(result)
}

pub fn encode_request(
    operation: &ExecutionContext,
    inputs: &[ExecutionContext],
    payload: &[u8],
) -> anyhow::Result<Vec<u8>> {
    wire::encode(&Request {
        version: abi::telemetry::VERSION,
        operation: (!operation.is_empty())
            .then(|| operation.encode().map(ByteBuf::from))
            .transpose()?,
        input_count: inputs.len(),
        inputs: encode_contexts(inputs)?,
        payload: payload.to_vec().into(),
    })
}

pub fn decode_request(bytes: &[u8]) -> anyhow::Result<DecodedRequest> {
    let request: Request = wire::decode(bytes)?;
    anyhow::ensure!(
        request.version == abi::telemetry::VERSION,
        "unsupported context request version"
    );
    Ok(DecodedRequest {
        contexts: OperationContexts {
            operation: request
                .operation
                .as_ref()
                .map(|bytes| ExecutionContext::decode(bytes))
                .transpose()?
                .unwrap_or_default(),
            inputs: decode_contexts(request.input_count, request.inputs)?,
        },
        payload: request.payload.into_vec(),
    })
}

pub fn encode_reply(
    payload: Vec<u8>,
    outputs: &[ExecutionContext],
    contextual: bool,
) -> anyhow::Result<Vec<u8>> {
    if !contextual {
        anyhow::ensure!(
            outputs.iter().all(ExecutionContext::is_empty),
            "native execution context transport was not negotiated"
        );
        return Ok(payload);
    }
    wire::encode(&Reply {
        version: abi::telemetry::VERSION,
        output_count: outputs.len(),
        outputs: encode_contexts(outputs)?,
        payload: payload.into(),
    })
}

pub fn decode_reply(bytes: &[u8]) -> anyhow::Result<DecodedReply> {
    let reply: Reply = wire::decode(bytes)?;
    anyhow::ensure!(
        reply.version == abi::telemetry::VERSION,
        "unsupported context reply version"
    );
    Ok(DecodedReply {
        outputs: decode_contexts(reply.output_count, reply.outputs)?,
        payload: reply.payload.into_vec(),
    })
}

pub(crate) fn input_context(
    contexts: Option<&OperationContexts>,
    index: usize,
    total: usize,
) -> anyhow::Result<ExecutionContext> {
    contexts
        .map(|contexts| contexts.input(index, total))
        .transpose()
        .map(|context| context.unwrap_or_default())
}

pub(crate) fn export_operation(
    future: impl std::future::Future<Output = Result<Vec<u8>, crate::transport::Failure>>
        + Send
        + 'static,
    runtime: Option<std::sync::Arc<tokio::runtime::Runtime>>,
    damaged: std::sync::Arc<crate::transport::PanicState>,
    cleanup: bool,
    scope: Option<ExecutionScope>,
) -> abi::OperationHandle {
    let scope = scope
        .or_else(|| ExecutionScope::current_plugin_telemetry().map(|_| ExecutionScope::capture()));
    match scope {
        Some(scope) => crate::transport::export_component_operation(
            scope.scope(future),
            runtime,
            damaged,
            cleanup,
        ),
        None => crate::transport::export_component_operation(future, runtime, damaged, cleanup),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::TraceContextV1;

    fn context(id: u8) -> ExecutionContext {
        ExecutionContext::new(TraceContextV1::new([id; 16], [id; 8], 0, None).unwrap())
    }

    #[test]
    fn operation_and_input_contexts_are_distinct_and_event_payload_is_unchanged() {
        let payload = vec![0, 0xff, 1, 2];
        let inputs = [context(2), ExecutionContext::default()];
        let request =
            decode_request(&encode_request(&context(1), &inputs, &payload).unwrap()).unwrap();
        assert_eq!(request.payload, payload);
        assert_eq!(request.contexts.operation, context(1));
        assert_eq!(request.contexts.input(0, 2).unwrap(), context(2));
        assert!(request.contexts.input(1, 2).unwrap().is_empty());
        assert!(request.contexts.input(0, 1).is_err());
        let reply = decode_reply(&encode_reply(payload.clone(), &inputs, true).unwrap()).unwrap();
        assert_eq!(reply.payload, payload);
        assert_eq!(reply.outputs, inputs);
        assert!(encode_reply(payload.clone(), &[context(1)], false).is_err());
        assert_eq!(
            encode_reply(payload.clone(), &[ExecutionContext::default()], false).unwrap(),
            payload
        );
    }

    #[test]
    fn malformed_and_oversized_context_capsules_are_rejected() {
        let frame = Request {
            version: 2,
            operation: None,
            input_count: 0,
            inputs: Vec::new(),
            payload: Vec::new().into(),
        };
        assert!(decode_request(&wire::encode(&frame).unwrap()).is_err());
        let frame = Request {
            version: 1,
            operation: Some(vec![0; crate::telemetry::MAX_EXECUTION_CONTEXT_BYTES + 1].into()),
            input_count: 0,
            inputs: Vec::new(),
            payload: Vec::new().into(),
        };
        assert!(decode_request(&wire::encode(&frame).unwrap()).is_err());
        assert!(encode_request(
            &ExecutionContext::default(),
            &vec![ExecutionContext::default(); abi::telemetry::MAX_CONTEXTS + 1],
            &[]
        )
        .is_err());
        for indices in [vec![0, 0], vec![1, 0], vec![2]] {
            let frame = Request {
                version: 1,
                operation: None,
                input_count: 2,
                inputs: indices
                    .into_iter()
                    .map(|index| ContextEntry {
                        index,
                        context: context(1).encode().unwrap().into(),
                    })
                    .collect(),
                payload: Vec::new().into(),
            };
            assert!(decode_request(&wire::encode(&frame).unwrap()).is_err());
        }
    }

    #[test]
    fn frozen_base_envelope_wire_matches_its_pre_context_bytes() {
        let frame = wire::Envelope {
            port: drasi_computation_contracts::PortId::try_new("in").unwrap(),
            envelope: std::borrow::Cow::Borrowed(&[1, 2, 3]),
        };
        assert_eq!(
            wire::encode(&frame).unwrap(),
            [
                0x82, 0xa4, b'p', b'o', b'r', b't', 0xa2, b'i', b'n', 0xa8, b'e', b'n', b'v', b'e',
                b'l', b'o', b'p', b'e', 0xc4, 3, 1, 2, 3,
            ]
        );
    }

    #[test]
    fn sparse_context_framing_fits_the_declared_message_budget() {
        let state = format!("a={},b={}", "x".repeat(256), "y".repeat(251));
        let maximum = ExecutionContext::new(
            TraceContextV1::new([255; 16], [255; 8], 255, Some(state.into())).unwrap(),
        );
        for count in [0, 1, 16, 1024] {
            for traced in [false, true] {
                let contexts = (0..count)
                    .map(|index| {
                        if traced && index % 2 == 0 {
                            maximum.clone()
                        } else {
                            ExecutionContext::default()
                        }
                    })
                    .collect::<Vec<_>>();
                let payload = vec![0x80; 65_536];
                let encoded = encode_request(&maximum, &contexts, &payload).unwrap();
                let budget = payload.len()
                    + CONTEXT_FRAME_OVERHEAD
                    + contexts
                        .iter()
                        .map(ExecutionContext::transport_size)
                        .sum::<usize>();
                assert!(encoded.len() <= budget, "{} > {budget}", encoded.len());
                assert_eq!(decode_request(&encoded).unwrap().contexts.inputs, contexts);
            }
        }
    }
}
