// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{abi, telemetry::*, transport, wire};
use serde::{Deserialize, Serialize};
use std::{ffi::c_void, sync::Arc};

/// MessagePack reply, never a Rust owner or a framework-specific span.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryBeginReply {
    pub owner: [u8; 16],
    pub lease: u64,
    pub context: Vec<u8>,
}

pub(crate) struct QueryClient(abi::telemetry::HostQueryTelemetryV4);
// Only the host dereferences context; the negotiated callbacks are thread-safe.
unsafe impl Send for QueryClient {}
unsafe impl Sync for QueryClient {}

impl std::fmt::Debug for QueryClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("NativeQueryTelemetry")
    }
}

impl Drop for QueryClient {
    fn drop(&mut self) {
        unsafe { self.0.release.expect("validated")(self.0.context) };
    }
}

fn invalid(error: impl std::fmt::Display) -> TelemetryError {
    TelemetryError::Invalid(error.to_string())
}

impl QueryClient {
    pub(crate) unsafe fn new(
        raw: *const abi::telemetry::HostQueryTelemetryV4,
    ) -> anyhow::Result<Arc<Self>> {
        let table = *unsafe { transport::checked_table(raw)? };
        anyhow::ensure!(
            table.version == abi::telemetry::VERSION_V4
                && table.reserved == 0
                && !table.context.is_null()
                && table.retain.is_some()
                && table.release.is_some()
                && table.begin.is_some()
                && table.in_scope.is_some()
                && table.finish.is_some()
                && table.event.is_some(),
            "incompatible native query observation callbacks"
        );
        unsafe { table.retain.expect("validated")(table.context) };
        Ok(Arc::new(Self(table)))
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        request: &QueryObservationRequest,
        limit: usize,
    ) -> Result<Option<Box<dyn QueryObservation>>> {
        let bytes = encode_telemetry(request, limit)?;
        let reply = unsafe {
            transport::take_reply(self.0.begin.expect("validated")(
                self.0.context,
                abi::BorrowedBytes::new(&bytes),
            ))
        }
        .map_err(invalid)?;
        if reply.len() > MAX_EXECUTION_CONTEXT_BYTES + 256 {
            return Err(TelemetryError::Invalid(
                "oversized query observation reply".into(),
            ));
        }
        let reply: Option<QueryBeginReply> = wire::decode(&reply).map_err(invalid)?;
        reply
            .map(|reply| {
                if reply.lease == 0 || reply.owner == [0; 16] {
                    return Err(TelemetryError::Invalid(
                        "invalid query observation token".into(),
                    ));
                }
                let mut observation = Observation {
                    client: self.clone(),
                    token: abi::telemetry::QueryToken {
                        owner: reply.owner,
                        lease: reply.lease,
                    },
                    context: ExecutionContext::default(),
                    finished: false,
                    limit,
                };
                observation.context = ExecutionContext::decode(&reply.context)?;
                Ok(Box::new(observation) as Box<dyn QueryObservation>)
            })
            .transpose()
    }

    pub(crate) fn event(&self, event: QueryObservationEvent, limit: usize) -> Result<()> {
        let bytes = encode_telemetry(&event, limit)?;
        unsafe {
            transport::take_status(self.0.event.expect("validated")(
                self.0.context,
                abi::BorrowedBytes::new(&bytes),
            ))
        }
        .map_err(invalid)
    }
}

struct Observation {
    client: Arc<QueryClient>,
    token: abi::telemetry::QueryToken,
    context: ExecutionContext,
    finished: bool,
    limit: usize,
}

impl QueryObservation for Observation {
    fn context(&self) -> ExecutionContext {
        self.context.clone()
    }

    fn in_scope(&self, work: &mut dyn FnMut()) -> Result<()> {
        if self.finished {
            return Err(TelemetryError::Retired);
        }
        struct Call<'a> {
            work: &'a mut dyn FnMut(),
            called: bool,
            repeated: bool,
        }
        unsafe extern "C" fn invoke(state: *mut c_void) -> abi::Status {
            transport::status_boundary(|| {
                let call = unsafe { &mut *state.cast::<Call<'_>>() };
                if call.called {
                    call.repeated = true;
                    return Err(transport::Failure::protocol(
                        "query scope callback invoked twice",
                    ));
                }
                call.called = true;
                (call.work)();
                Ok(())
            })
        }
        let mut call = Call {
            work,
            called: false,
            repeated: false,
        };
        unsafe {
            transport::take_status(self.client.0.in_scope.expect("validated")(
                self.client.0.context,
                self.token,
                invoke,
                (&mut call as *mut Call<'_>).cast(),
            ))
        }
        .map_err(invalid)?;
        if !call.called || call.repeated {
            return Err(TelemetryError::Invalid(
                "query scope callback must be invoked exactly once".into(),
            ));
        }
        Ok(())
    }

    fn finish(&mut self, result: QueryObservationResult) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        let bytes = encode_telemetry(&result, self.limit)?;
        self.finished = true;
        unsafe {
            transport::take_status(self.client.0.finish.expect("validated")(
                self.client.0.context,
                self.token,
                abi::BorrowedBytes::new(&bytes),
            ))
        }
        .map_err(invalid)
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        if let Err(error) = self.finish(QueryObservationResult {
            outcome: QueryObservationOutcome::Cancelled,
            result_count: None,
        }) {
            log::error!("Cancelling native query transport observation: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn retain(_: *mut c_void) {}
    unsafe extern "C" fn release(_: *mut c_void) {}
    unsafe extern "C" fn begin(_: *mut c_void, _: abi::BorrowedBytes) -> abi::Reply {
        transport::reply_boundary(|| {
            wire::encode(&Option::<QueryBeginReply>::None).map_err(transport::Failure::from)
        })
    }
    unsafe extern "C" fn scope(
        _: *mut c_void,
        _: abi::telemetry::QueryToken,
        callback: abi::telemetry::ScopedCall,
        state: *mut c_void,
    ) -> abi::Status {
        unsafe { callback(state) }
    }
    unsafe extern "C" fn finish(
        _: *mut c_void,
        _: abi::telemetry::QueryToken,
        _: abi::BorrowedBytes,
    ) -> abi::Status {
        abi::Status::ok()
    }
    unsafe extern "C" fn event(_: *mut c_void, _: abi::BorrowedBytes) -> abi::Status {
        abi::Status::ok()
    }

    #[test]
    fn query_host_table_requires_exact_complete_versioned_callbacks() {
        let mut context = 0u8;
        let valid = abi::telemetry::HostQueryTelemetryV4 {
            header: abi::Header::new::<abi::telemetry::HostQueryTelemetryV4>(),
            version: abi::telemetry::VERSION_V4,
            reserved: 0,
            context: (&mut context as *mut u8).cast(),
            retain: Some(retain),
            release: Some(release),
            begin: Some(begin),
            in_scope: Some(scope),
            finish: Some(finish),
            event: Some(event),
        };
        assert!(unsafe { QueryClient::new(&valid) }.is_ok());
        for fault in 0..10 {
            let mut invalid = valid;
            match fault {
                0 => invalid.header.size += 1,
                1 => invalid.version += 1,
                2 => invalid.reserved = 1,
                3 => invalid.context = std::ptr::null_mut(),
                4 => invalid.retain = None,
                5 => invalid.release = None,
                6 => invalid.begin = None,
                7 => invalid.in_scope = None,
                8 => invalid.finish = None,
                _ => invalid.event = None,
            }
            assert!(unsafe { QueryClient::new(&invalid) }.is_err());
        }
    }
}
