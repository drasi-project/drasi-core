// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{Result, TelemetryError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const MAX_TRACE_STATE_BYTES: usize = 512;
pub const MAX_TRACE_STATE_MEMBERS: usize = 32;
pub const MAX_EXECUTION_CONTEXT_BYTES: usize = 1024;

/// Portable identity of the sending span, not a live span or a component identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "TraceContextFields")]
pub struct TraceContextV1 {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    trace_flags: u8,
    trace_state: Option<Arc<str>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceContextFields {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    trace_flags: u8,
    trace_state: Option<Arc<str>>,
}

impl TryFrom<TraceContextFields> for TraceContextV1 {
    type Error = TelemetryError;

    fn try_from(value: TraceContextFields) -> Result<Self> {
        Self::new(
            value.trace_id,
            value.span_id,
            value.trace_flags,
            value.trace_state,
        )
    }
}

impl TraceContextV1 {
    /// Decode W3C transport metadata without installing a tracing backend.
    pub fn from_w3c_headers(traceparent: &[u8], tracestate: Option<&[u8]>) -> Result<Self> {
        let invalid = || TelemetryError::Invalid("invalid W3C traceparent".into());
        if traceparent.len() < 55
            || traceparent[2] != b'-'
            || traceparent[35] != b'-'
            || traceparent[52] != b'-'
        {
            return Err(invalid());
        }
        let [version] = Self::decode_trace_hex(&traceparent[..2])?;
        if version == 0xff
            || (version == 0 && traceparent.len() != 55)
            || (traceparent.len() > 55 && (traceparent[55] != b'-' || traceparent.len() == 56))
        {
            return Err(invalid());
        }
        let trace_id = Self::decode_trace_hex(&traceparent[3..35])?;
        let span_id = Self::decode_trace_hex(&traceparent[36..52])?;
        let [flags] = Self::decode_trace_hex(&traceparent[53..55])?;
        let state = tracestate
            .map(|state| {
                if state.len() > MAX_TRACE_STATE_BYTES {
                    return Err(TelemetryError::Invalid("oversized W3C tracestate".into()));
                }
                let state = std::str::from_utf8(state)
                    .map_err(|_| TelemetryError::Invalid("invalid W3C tracestate".into()))?;
                validate_trace_state(state)?;
                Ok(Arc::from(state))
            })
            .transpose()?;
        Self::new(trace_id, span_id, flags, state)
    }

    pub fn new(
        trace_id: [u8; 16],
        span_id: [u8; 8],
        trace_flags: u8,
        trace_state: Option<Arc<str>>,
    ) -> Result<Self> {
        if trace_id == [0; 16] || span_id == [0; 8] {
            return Err(TelemetryError::Invalid(
                "trace and span IDs must be nonzero".into(),
            ));
        }
        if let Some(state) = &trace_state {
            validate_trace_state(state)?;
        }
        Ok(Self {
            trace_id,
            span_id,
            trace_flags,
            trace_state,
        })
    }

    fn decode_trace_hex<const N: usize>(value: &[u8]) -> Result<[u8; N]> {
        let digit = |byte| match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(TelemetryError::Invalid(
                "invalid W3C traceparent hex".into(),
            )),
        };
        let mut decoded = [0; N];
        for (output, pair) in decoded.iter_mut().zip(value.chunks_exact(2)) {
            *output = digit(pair[0])? * 16 + digit(pair[1])?;
        }
        Ok(decoded)
    }

    pub fn trace_id(&self) -> [u8; 16] {
        self.trace_id
    }

    pub fn span_id(&self) -> [u8; 8] {
        self.span_id
    }

    pub fn trace_flags(&self) -> u8 {
        self.trace_flags
    }

    pub fn sampled(&self) -> bool {
        self.trace_flags & 1 != 0
    }

    pub fn trace_state(&self) -> Option<&str> {
        self.trace_state.as_deref()
    }
}

fn validate_trace_state(state: &str) -> Result<()> {
    let invalid = || TelemetryError::Invalid("invalid or oversized W3C trace state".into());
    if state.is_empty() || state.len() > MAX_TRACE_STATE_BYTES {
        return Err(invalid());
    }
    let mut keys = Vec::new();
    for member in state.split(',') {
        let member = member.trim_matches([' ', '\t']);
        let (key, value) = member.split_once('=').ok_or_else(invalid)?;
        if keys.len() == MAX_TRACE_STATE_MEMBERS
            || !valid_trace_state_key(key)
            || keys.contains(&key)
            || value.is_empty()
            || value.len() > 256
            || value.ends_with(' ')
            || !value
                .bytes()
                .all(|byte| (0x20..=0x7e).contains(&byte) && byte != b',' && byte != b'=')
        {
            return Err(invalid());
        }
        keys.push(key);
    }
    Ok(())
}

fn valid_trace_state_key(key: &str) -> bool {
    let key_char = |byte: u8| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'_' | b'-' | b'*' | b'/')
    };
    if key.is_empty() || key.len() > 256 {
        return false;
    }
    if let Some((tenant, system)) = key.split_once('@') {
        !tenant.is_empty()
            && tenant.len() <= 241
            && tenant.as_bytes()[0].is_ascii_alphanumeric()
            && tenant.bytes().all(key_char)
            && !system.is_empty()
            && system.len() <= 14
            && system.as_bytes()[0].is_ascii_lowercase()
            && system.bytes().all(key_char)
    } else {
        key.as_bytes()[0].is_ascii_lowercase() && key.bytes().all(key_char)
    }
}

/// Transient execution metadata. Persist the event, never this carrier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionContext {
    trace: Option<Arc<TraceContextV1>>,
    handled_branch: Option<super::HandledBranchContext>,
    handled_output: Option<Arc<super::HandledBranchOutput>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionContextRecord {
    version: u32,
    trace: Option<TraceContextV1>,
}

impl ExecutionContext {
    /// Missing metadata clears prior context. Malformed metadata is diagnosed
    /// and ignored so tracing cannot reject otherwise valid source data.
    pub fn from_w3c_headers(traceparent: Option<&[u8]>, tracestate: Option<&[u8]>) -> Self {
        let Some(traceparent) = traceparent else {
            return Self::default();
        };
        match TraceContextV1::from_w3c_headers(traceparent, tracestate) {
            Ok(trace) => Self::new(trace),
            Err(error) => {
                log::warn!(target: "drasi::telemetry",
                    "Ignoring malformed incoming trace context; starting a fresh trace: {error}");
                Self::default()
            }
        }
    }

    pub fn new(trace: TraceContextV1) -> Self {
        Self {
            trace: Some(Arc::new(trace)),
            handled_branch: None,
            handled_output: None,
        }
    }

    pub fn trace(&self) -> Option<&TraceContextV1> {
        self.trace.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        self.trace.is_none() && self.handled_branch.is_none()
    }

    pub fn handled_branch(&self) -> Option<&super::HandledBranchContext> {
        self.handled_branch.as_ref()
    }

    pub fn handled_output(&self) -> Option<&super::HandledBranchOutput> {
        self.handled_output.as_deref()
    }

    pub fn with_handled_output(mut self, output: super::HandledBranchOutput) -> Self {
        self.handled_output = Some(Arc::new(output));
        self
    }

    pub fn with_handled_branch(mut self, context: super::HandledBranchContext) -> Self {
        self.handled_branch = Some(context);
        self.handled_output = None;
        self
    }

    /// Select a new span identity without replacing the input's timing owner.
    pub fn with_trace_from(mut self, context: &Self) -> Self {
        self.trace = context.trace.clone();
        self
    }

    /// Only for an explicitly unsupported propagation boundary. The caller must
    /// diagnose coverage loss and prevent ambient timing inheritance on receipt.
    pub fn without_handled_branch(mut self) -> Self {
        self.handled_branch = None;
        self.handled_output = None;
        self
    }

    /// Explicit transport encoding; this is not an event or persistence codec.
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.handled_branch.is_some() {
            return Err(TelemetryError::Unsupported(
                "handled-branch context in trace-only V1 transport",
            ));
        }
        crate::encode_bounded_messagepack(
            &ExecutionContextRecord {
                version: 1,
                trace: self.trace.as_deref().cloned(),
            },
            MAX_EXECUTION_CONTEXT_BYTES,
        )
        .map_err(|error| TelemetryError::Invalid(error.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let record: ExecutionContextRecord =
            crate::decode_bounded_messagepack(bytes, MAX_EXECUTION_CONTEXT_BYTES)
                .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
        if record.version != 1 {
            return Err(TelemetryError::Unsupported("execution context version"));
        }
        Ok(Self {
            trace: record.trace.map(Arc::new),
            handled_branch: None,
            handled_output: None,
        })
    }

    /// Maximum additional transport bytes for a nonempty context and its framing.
    pub fn transport_size(&self) -> usize {
        if self.is_empty() {
            0
        } else if self.handled_branch.is_some() {
            4096 + 32
        } else {
            MAX_EXECUTION_CONTEXT_BYTES + 32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(state: Option<&str>, flags: u8) -> Result<ExecutionContext> {
        TraceContextV1::new([1; 16], [2; 8], flags, state.map(Arc::from)).map(ExecutionContext::new)
    }

    #[test]
    fn incoming_w3c_context_preserves_identity_flags_and_state() {
        for flags in ["00", "01", "03"] {
            let parent = format!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-{flags}");
            let context =
                ExecutionContext::from_w3c_headers(Some(parent.as_bytes()), Some(b"vendor=value"));
            let trace = context.trace().unwrap();
            assert_eq!(
                trace.trace_id(),
                [
                    0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e,
                    0x0e, 0x47, 0x36
                ]
            );
            assert_eq!(
                trace.span_id(),
                [0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7]
            );
            assert_eq!(trace.sampled(), flags != "00");
            assert_eq!(trace.trace_flags(), u8::from_str_radix(flags, 16).unwrap());
            assert_eq!(trace.trace_state(), Some("vendor=value"));
        }
    }

    #[test]
    fn incoming_w3c_context_rejects_malformed_metadata_without_rejecting_data() {
        let parent = b"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        assert!(ExecutionContext::from_w3c_headers(None, Some(b"vendor=value")).is_empty());
        for invalid in [
            "",
            "4bf92f3577b34da6a3ce929d0e0e4736",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-gg",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01x",
        ] {
            assert!(
                TraceContextV1::from_w3c_headers(invalid.as_bytes(), None).is_err(),
                "{invalid}"
            );
            assert!(ExecutionContext::from_w3c_headers(Some(invalid.as_bytes()), None).is_empty());
        }
        for invalid in [b"vendor=".as_slice(), b"a=b,a=c", &[0xff], &[b'x'; 513]] {
            assert!(ExecutionContext::from_w3c_headers(Some(parent), Some(invalid)).is_empty());
        }
        assert!(TraceContextV1::from_w3c_headers(
            b"01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-future",
            None,
        )
        .is_ok());
    }

    #[test]
    fn portable_context_preserves_flags_and_distinguishes_absence() {
        for original in [
            ExecutionContext::default(),
            context(None, 0).unwrap(),
            context(Some("vendor=value,1tenant@system=other"), 0xff).unwrap(),
        ] {
            let encoded = original.encode().unwrap();
            assert!(encoded.len() <= MAX_EXECUTION_CONTEXT_BYTES);
            assert_eq!(ExecutionContext::decode(&encoded).unwrap(), original);
        }
        assert!(!context(None, 0).unwrap().is_empty());
        assert!(!context(None, 0).unwrap().trace().unwrap().sampled());
        assert!(context(None, 1).unwrap().trace().unwrap().sampled());
    }

    #[test]
    fn trace_state_validates_w3c_bounds_and_duplicate_keys() {
        for invalid in [
            "",
            "a=",
            "a=b,a=c",
            "A=b",
            "a=b=c",
            "a=b,,c=d",
            "a=b\n",
            "a@123=b",
            "a@toolongtoolongtoolong=b",
            "a=\u{00e9}",
        ] {
            assert!(context(Some(invalid), 1).is_err(), "{invalid:?}");
        }
        let members = (0..32)
            .map(|n| format!("a{n}=b"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(context(Some(&members), 1).is_ok());
        assert!(context(Some(&format!("{members},last=b")), 1).is_err());
        let boundary = format!("a={},b={}", "x".repeat(256), "y".repeat(251));
        assert_eq!(boundary.len(), 512);
        assert!(context(Some(&boundary), 1).is_ok());
        assert!(context(Some(&format!("{boundary}z")), 1).is_err());
        assert!(context(Some(&format!("a={}", "x".repeat(257))), 1).is_err());
    }

    #[test]
    fn decoding_cannot_bypass_validation_or_record_limits() {
        assert!(TraceContextV1::new([0; 16], [1; 8], 1, None).is_err());
        assert!(TraceContextV1::new([1; 16], [0; 8], 1, None).is_err());
        let invalid = serde_json::json!({
            "trace_id": vec![0u8; 16], "span_id": vec![1u8; 8], "trace_flags": 1, "trace_state": null
        });
        assert!(serde_json::from_value::<TraceContextV1>(invalid).is_err());
        let bytes = crate::encode_bounded_messagepack(
            &serde_json::json!({"version": 2, "trace": null}),
            MAX_EXECUTION_CONTEXT_BYTES,
        )
        .unwrap();
        assert!(ExecutionContext::decode(&bytes).is_err());
        let bytes = crate::encode_bounded_messagepack(
            &serde_json::json!({"version": 1, "trace": null, "extra": 1}),
            MAX_EXECUTION_CONTEXT_BYTES,
        )
        .unwrap();
        assert!(ExecutionContext::decode(&bytes).is_err());
        assert!(ExecutionContext::decode(&vec![0; MAX_EXECUTION_CONTEXT_BYTES + 1]).is_err());
    }
}
