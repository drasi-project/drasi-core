// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{abi, telemetry::*, transport, wire};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::sync::{Arc, Mutex};

const MAX_CONTEXT_BYTES: usize = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    trace: Option<TraceContextV1>,
    owner: Option<[u8; 16]>,
    lease: Option<u64>,
    unavailable: Option<HandledBranchUnavailable>,
    output: Option<HandledBranchOutput>,
}

pub fn encode_context(context: &ExecutionContext) -> anyhow::Result<Vec<u8>> {
    let token = context
        .handled_branch()
        .and_then(HandledBranchContext::token);
    let unavailable = match context.handled_branch() {
        Some(HandledBranchContext::Unavailable(reason)) => Some(*reason),
        _ => None,
    };
    Ok(drasi_computation_contracts::encode_bounded_messagepack(
        &Context {
            trace: context.trace().cloned(),
            owner: token.map(|token| token.owner),
            lease: token.map(|token| token.lease),
            unavailable,
            output: token.and_then(|_| context.handled_output().cloned()),
        },
        MAX_CONTEXT_BYTES,
    )?)
}

pub fn decode_context(
    bytes: &[u8],
    transport: &dyn HandledBranchTransport,
) -> anyhow::Result<ExecutionContext> {
    let record: Context =
        drasi_computation_contracts::decode_bounded_messagepack(bytes, MAX_CONTEXT_BYTES)?;
    anyhow::ensure!(
        record.owner.is_some() == record.lease.is_some()
            && (record.owner.is_none() || record.unavailable.is_none()),
        "invalid native timing carrier"
    );
    let mut context = record.trace.map(ExecutionContext::new).unwrap_or_default();
    if let Some(owner) = record.owner {
        let token = HandledBranchToken {
            owner,
            lease: record.lease.expect("validated"),
        };
        anyhow::ensure!(
            owner != [0; 16] && token.lease > 0,
            "invalid native timing token"
        );
        let acquired = match transport.acquire(token) {
            Ok(context) => context,
            Err(TelemetryError::Capacity(_)) => {
                HandledBranchContext::Unavailable(HandledBranchUnavailable::Capacity)
            }
            Err(TelemetryError::Retired) => {
                HandledBranchContext::Unavailable(HandledBranchUnavailable::Retired)
            }
            Err(error) => return Err(error.into()),
        };
        context = context.with_handled_branch(acquired);
    } else if let Some(reason) = record.unavailable {
        context = context.with_handled_branch(HandledBranchContext::Unavailable(reason));
    }
    if let Some(output) = record.output {
        anyhow::ensure!(
            record.owner.is_some()
                && !output.query_id.is_empty()
                && output.query_id.len() <= 1024
                && output.producer_epoch != [0; 16]
                && output.sequence > 0
                && output.required_rows > 0,
            "invalid native timing output identity"
        );
        if context
            .handled_branch()
            .and_then(HandledBranchContext::token)
            .is_some()
        {
            context = context.with_handled_output(output);
        }
    }
    Ok(context)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    operation: ByteBuf,
    inputs: Vec<ByteBuf>,
    payload: ByteBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    version: u32,
    outputs: Vec<ByteBuf>,
    payload: ByteBuf,
}

pub fn encode_request(
    operation: &ExecutionContext,
    inputs: &[ExecutionContext],
    payload: &[u8],
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        inputs.len() <= abi::telemetry::MAX_CONTEXTS,
        "too many timing inputs"
    );
    wire::encode(&Request {
        version: abi::telemetry::VERSION_V3,
        operation: encode_context(operation)?.into(),
        inputs: inputs
            .iter()
            .map(|context| encode_context(context).map(Into::into))
            .collect::<anyhow::Result<_>>()?,
        payload: payload.to_vec().into(),
    })
}

pub(crate) fn decode_request(
    bytes: &[u8],
    transport: &dyn HandledBranchTransport,
) -> anyhow::Result<crate::telemetry_transport::DecodedRequest> {
    let request: Request = wire::decode(bytes)?;
    anyhow::ensure!(
        request.version == abi::telemetry::VERSION_V3
            && request.inputs.len() <= abi::telemetry::MAX_CONTEXTS,
        "invalid timing request"
    );
    Ok(crate::telemetry_transport::DecodedRequest {
        contexts: crate::telemetry_transport::OperationContexts {
            operation: decode_context(&request.operation, transport)?,
            inputs: request
                .inputs
                .iter()
                .map(|context| decode_context(context, transport))
                .collect::<anyhow::Result<_>>()?,
        },
        payload: request.payload.into_vec(),
    })
}

pub(crate) fn encode_reply(
    payload: Vec<u8>,
    outputs: &[ExecutionContext],
    transport: &dyn HandledBranchTransport,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        outputs.len() <= abi::telemetry::MAX_CONTEXTS,
        "too many timing outputs"
    );
    let bytes = wire::encode(&Reply {
        version: abi::telemetry::VERSION_V3,
        outputs: outputs
            .iter()
            .map(|context| encode_context(context).map(Into::into))
            .collect::<anyhow::Result<_>>()?,
        payload: payload.into(),
    })?;
    transport.keep_reply(outputs)?;
    Ok(bytes)
}

pub fn decode_reply(
    bytes: &[u8],
    transport: &dyn HandledBranchTransport,
) -> anyhow::Result<crate::telemetry_transport::DecodedReply> {
    let reply: Reply = wire::decode(bytes)?;
    anyhow::ensure!(
        reply.version == abi::telemetry::VERSION_V3
            && reply.outputs.len() <= abi::telemetry::MAX_CONTEXTS,
        "invalid timing reply"
    );
    Ok(crate::telemetry_transport::DecodedReply {
        outputs: reply
            .outputs
            .iter()
            .map(|context| decode_context(context, transport))
            .collect::<anyhow::Result<_>>()?,
        payload: reply.payload.into_vec(),
    })
}

#[derive(Debug)]
struct HostLease(abi::telemetry::HostTimingV3);
unsafe impl Send for HostLease {}
unsafe impl Sync for HostLease {}
impl Drop for HostLease {
    fn drop(&mut self) {
        unsafe { self.0.release_context.expect("validated")(self.0.context) };
    }
}

#[derive(Debug)]
pub(crate) struct Client {
    host: Arc<HostLease>,
    replies: Mutex<Vec<ExecutionContext>>,
}

impl Client {
    pub(crate) unsafe fn new(
        table: *const abi::telemetry::HostTimingV3,
    ) -> anyhow::Result<Arc<Self>> {
        let host = *unsafe { transport::checked_table(table)? };
        anyhow::ensure!(
            host.version == abi::telemetry::VERSION_V3
                && host.reserved == 0
                && !host.context.is_null()
                && host.retain_context.is_some()
                && host.release_context.is_some()
                && host.acquire.is_some()
                && host.release.is_some(),
            "invalid native timing callback table"
        );
        unsafe { host.retain_context.expect("validated")(host.context) };
        Ok(Arc::new(Self {
            host: Arc::new(HostLease(host)),
            replies: Mutex::new(Vec::new()),
        }))
    }
}

fn to_abi(token: HandledBranchToken) -> abi::telemetry::TimingToken {
    abi::telemetry::TimingToken {
        owner: token.owner,
        lease: token.lease,
    }
}

impl HandledBranchTransport for Client {
    fn acquire(&self, token: HandledBranchToken) -> Result<HandledBranchContext> {
        let mut acquired = abi::telemetry::TimingToken::default();
        let status = unsafe {
            self.host.0.acquire.expect("validated")(
                self.host.0.context,
                to_abi(token),
                &mut acquired,
            )
        };
        match status {
            abi::status::OK => Ok(HandledBranchContext::Tracked(Arc::new(NativeLease {
                host: self.host.clone(),
                token: HandledBranchToken {
                    owner: acquired.owner,
                    lease: acquired.lease,
                },
            }))),
            abi::status::CLOSED => Err(TelemetryError::Retired),
            abi::status::BUSY => Err(TelemetryError::Capacity("native timing ownership")),
            _ => Err(TelemetryError::Invalid(
                "rejected native timing token".into(),
            )),
        }
    }
    fn keep_reply(&self, contexts: &[ExecutionContext]) -> Result<()> {
        let mut replies = self.replies.lock().map_err(|_| TelemetryError::Poisoned)?;
        if !replies.is_empty() {
            return Err(TelemetryError::Invalid(
                "native timing reply encoded twice".into(),
            ));
        }
        replies.extend_from_slice(contexts);
        Ok(())
    }
}

struct NativeLease {
    host: Arc<HostLease>,
    token: HandledBranchToken,
}
impl std::fmt::Debug for NativeLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("NativeTimingLease")
            .field(&self.token)
            .finish()
    }
}
impl HandledBranchLease for NativeLease {
    fn token(&self) -> HandledBranchToken {
        self.token
    }
}
impl Drop for NativeLease {
    fn drop(&mut self) {
        let status = unsafe {
            self.host.0.release.expect("validated")(self.host.0.context, to_abi(self.token))
        };
        if status != abi::status::OK {
            log::error!("Native timing lease release failed: {status}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_frame_preserves_host_ownership_and_releases_on_decode_failure() {
        let tracker = HandledBranchTracker::new(Default::default()).unwrap();
        let origin = tracker.start().unwrap();
        let resolver = match &origin {
            HandledBranchContext::Tracked(lease) => lease.transport().unwrap(),
            _ => unreachable!(),
        };
        let context = ExecutionContext::default()
            .with_handled_branch(origin)
            .with_handled_output(HandledBranchOutput {
                query_id: "query".into(),
                producer_epoch: [1; 16],
                sequence: 7,
                required_rows: 2,
            });
        let bytes = encode_context(&context).unwrap();
        let decoded = decode_context(&bytes, resolver.as_ref()).unwrap();
        assert_eq!(decoded.handled_output(), context.handled_output());
        assert_ne!(
            decoded.handled_branch().unwrap().token(),
            context.handled_branch().unwrap().token()
        );
        assert_eq!(tracker.diagnostics().unwrap().records, 3);
        drop(decoded);
        let mut malformed: Context =
            drasi_computation_contracts::decode_bounded_messagepack(&bytes, MAX_CONTEXT_BYTES)
                .unwrap();
        malformed.output.as_mut().unwrap().query_id = "".into();
        let malformed =
            drasi_computation_contracts::encode_bounded_messagepack(&malformed, MAX_CONTEXT_BYTES)
                .unwrap();
        assert!(decode_context(&malformed, resolver.as_ref()).is_err());
        assert_eq!(tracker.diagnostics().unwrap().records, 2);
        drop(context);
        assert_eq!(tracker.diagnostics().unwrap().records, 0);
    }

    #[test]
    fn saturated_import_remains_unavailable_and_round_trips_without_failing_business_data() {
        let tracker = HandledBranchTracker::new(HandledBranchLimits {
            max_records: 2,
            ..Default::default()
        })
        .unwrap();
        let origin = tracker.start().unwrap();
        let resolver = match &origin {
            HandledBranchContext::Tracked(lease) => lease.transport().unwrap(),
            _ => unreachable!(),
        };
        let context = ExecutionContext::default()
            .with_handled_branch(origin)
            .with_handled_output(HandledBranchOutput {
                query_id: "query".into(),
                producer_epoch: [1; 16],
                sequence: 7,
                required_rows: 2,
            });
        let decoded =
            decode_context(&encode_context(&context).unwrap(), resolver.as_ref()).unwrap();
        assert!(matches!(
            decoded.handled_branch(),
            Some(HandledBranchContext::Unavailable(
                HandledBranchUnavailable::Capacity
            ))
        ));
        assert!(decoded.handled_output().is_none());
        let reply = encode_reply(vec![1, 2, 3], &[decoded], resolver.as_ref()).unwrap();
        let reply = decode_reply(&reply, resolver.as_ref()).unwrap();
        assert_eq!(reply.payload, [1, 2, 3]);
        assert!(matches!(
            reply.outputs[0].handled_branch(),
            Some(HandledBranchContext::Unavailable(
                HandledBranchUnavailable::Capacity
            ))
        ));
        assert_eq!(tracker.diagnostics().unwrap().records, 2);
    }
}
