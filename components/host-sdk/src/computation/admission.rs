// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Weak,
    },
};

use drasi_computation_plugin_sdk::{
    abi,
    admission::{AdmissionError, AdmitRequest, ReceiptRequest},
    transport::{self, Failure},
    wire,
};
use drasi_lib::computation::v1::{BinaryEnvelopeCodec, PipeError, SourceAdmission};

const REQUEST_LIMIT: usize = 16;

struct State {
    active: AtomicBool,
    outstanding: AtomicUsize,
    admission: Weak<SourceAdmission>,
    codec: Arc<BinaryEnvelopeCodec>,
}

pub(super) struct AdmissionBinding {
    state: Arc<State>,
}

impl AdmissionBinding {
    pub(super) fn new(admission: &Arc<SourceAdmission>, codec: Arc<BinaryEnvelopeCodec>) -> Self {
        Self {
            state: Arc::new(State {
                active: AtomicBool::new(true),
                outstanding: AtomicUsize::new(0),
                admission: Arc::downgrade(admission),
                codec,
            }),
        }
    }

    pub(super) fn table(&self) -> abi::services::SourceAdmissionV1 {
        abi::services::SourceAdmissionV1 {
            header: abi::Header::new::<abi::services::SourceAdmissionV1>(),
            context: Arc::as_ptr(&self.state) as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            request: Some(request),
            report_failure: Some(report_failure),
            describe: Some(describe),
        }
    }
}

impl Drop for AdmissionBinding {
    fn drop(&mut self) {
        self.state.active.store(false, Ordering::Release);
    }
}

struct RequestGuard(Arc<State>);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

unsafe extern "C" fn retain(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::increment_strong_count(context.cast::<State>()) });
}
unsafe extern "C" fn release(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::decrement_strong_count(context.cast::<State>()) });
}

unsafe extern "C" fn describe(context: *mut c_void) -> abi::Reply {
    transport::reply_boundary(|| {
        if context.is_null() {
            return Err(Failure::protocol("null native admission handle"));
        }
        let state = unsafe { &*context.cast::<State>() };
        if !state.active.load(Ordering::Acquire) {
            return Err(Failure::closed());
        }
        let admission = state.admission.upgrade().ok_or_else(Failure::closed)?;
        wire::encode(admission.identity()).map_err(Failure::from)
    })
}

unsafe extern "C" fn report_failure(
    context: *mut c_void,
    input: abi::BorrowedBytes,
) -> abi::Status {
    transport::status_boundary(|| {
        if context.is_null() {
            return Err(Failure::protocol("null native admission handle"));
        }
        let state = unsafe { &*context.cast::<State>() };
        if !state.active.load(Ordering::Acquire) {
            return Err(Failure::closed());
        }
        let admission = state.admission.upgrade().ok_or_else(Failure::closed)?;
        let reason = std::str::from_utf8(unsafe { transport::borrowed_bytes(input, 4096)? })
            .map_err(|error| Failure::protocol(error.to_string()))?;
        admission
            .report_failure(reason)
            .map_err(|error| match error {
                PipeError::Closed => Failure::closed(),
                error => Failure::failed(error.to_string()),
            })
    })
}

unsafe extern "C" fn request(
    context: *mut c_void,
    code: u32,
    input: abi::BorrowedBytes,
    out: *mut abi::OperationHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if context.is_null() || out.is_null() {
            return Err(Failure::protocol("null native admission handle or output"));
        }
        let state = unsafe { &*context.cast::<State>() };
        if !state.active.load(Ordering::Acquire) {
            return Err(Failure::closed());
        }
        if !(abi::services::admission::REGISTER..=abi::services::admission::ADMIT).contains(&code) {
            return Err(Failure::new(
                abi::status::UNSUPPORTED,
                "unknown admission request",
            ));
        }
        state
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < REQUEST_LIMIT).then_some(count + 1)
            })
            .map_err(|_| {
                Failure::new(
                    abi::status::BUSY,
                    "native admission operation limit reached",
                )
            })?;
        unsafe { Arc::increment_strong_count(context.cast::<State>()) };
        let guard = RequestGuard(unsafe { Arc::from_raw(context.cast::<State>()) });
        let input = unsafe { transport::borrowed_bytes(input, abi::MAX_MESSAGE_BYTES)? }.to_vec();
        let operation = transport::export_operation(
            async move {
                if !guard.0.active.load(Ordering::Acquire) {
                    return Err(Failure::closed());
                }
                let admission = guard.0.admission.upgrade().ok_or_else(Failure::closed)?;
                // Only mailbox operations are polled here. Storage stays on the host
                // graph's runtime, never under the calling plugin's Tokio context.
                apply(&admission, &guard.0.codec, code, &input).await
            },
            None,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

fn response<T: serde::Serialize>(result: Result<T, PipeError>) -> Result<Vec<u8>, Failure> {
    wire::encode(&result.map_err(AdmissionError::from)).map_err(Failure::from)
}

async fn apply(
    admission: &SourceAdmission,
    codec: &BinaryEnvelopeCodec,
    code: u32,
    input: &[u8],
) -> Result<Vec<u8>, Failure> {
    use abi::services::admission::*;
    match code {
        REGISTER => response(admission.register_producer(wire::decode(input)?).await),
        RETIRE => response(admission.retire_producer(&wire::decode(input)?).await),
        STATUS => response(admission.producer_status(&wire::decode(input)?).await),
        RECEIPT => {
            let request: ReceiptRequest = wire::decode(input)?;
            response(
                admission
                    .admission_receipt(&request.session, request.sequence)
                    .await,
            )
        }
        ADMIT => {
            let request: AdmitRequest<'_> = wire::decode(input)?;
            let envelope = codec
                .decode(&request.envelope)
                .map_err(anyhow::Error::from)?;
            response(
                admission
                    .admit(&request.session, request.sequence, &envelope)
                    .await,
            )
        }
        _ => Err(Failure::protocol("unknown admission request")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::transport::{take_status, OperationFuture};
    use drasi_lib::computation::v1::GraphChangeCodec;

    #[test]
    fn unpolled_requests_are_bounded_and_cancellation_releases_capacity() {
        let binding = AdmissionBinding {
            state: Arc::new(State {
                active: AtomicBool::new(true),
                outstanding: AtomicUsize::new(0),
                admission: Weak::new(),
                codec: Arc::new(wire::codec(&[GraphChangeCodec::schema()]).unwrap()),
            }),
        };
        let table = binding.table();
        let mut operations = Vec::new();
        for _ in 0..REQUEST_LIMIT {
            let mut operation = abi::OperationHandle::null();
            unsafe {
                take_status(table.request.unwrap()(
                    table.context,
                    abi::services::admission::STATUS,
                    abi::BorrowedBytes::empty(),
                    &mut operation,
                ))
                .unwrap();
                operations.push(OperationFuture::new(operation).unwrap());
            }
        }
        assert_eq!(
            binding.state.outstanding.load(Ordering::Acquire),
            REQUEST_LIMIT
        );
        let mut rejected = abi::OperationHandle::null();
        let error = unsafe {
            take_status(table.request.unwrap()(
                table.context,
                abi::services::admission::STATUS,
                abi::BorrowedBytes::empty(),
                &mut rejected,
            ))
        }
        .unwrap_err();
        assert_eq!(error.code, abi::status::BUSY);
        assert!(rejected.state.is_null());
        drop(operations);
        assert_eq!(binding.state.outstanding.load(Ordering::Acquire), 0);
        unsafe { table.retain.unwrap()(table.context) };
        drop(binding);
        let error = unsafe {
            take_status(table.request.unwrap()(
                table.context,
                abi::services::admission::STATUS,
                abi::BorrowedBytes::empty(),
                &mut rejected,
            ))
        }
        .unwrap_err();
        assert_eq!(error.code, abi::status::CLOSED);
        unsafe { table.release.unwrap()(table.context) };
    }
}
