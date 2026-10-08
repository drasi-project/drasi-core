// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use drasi_lib::computation::v1::{
    AdmissionReceipt, AdmissionRejection, BinaryEnvelopeCodec, ChangeEnvelope, ComponentId,
    GraphProducerIdentity, PipeError, ProducerSession, ProducerStatus,
};
use serde::{Deserialize, Serialize};

use crate::{
    abi::{self, services::SourceAdmissionV1},
    transport::{checked_table, take_reply, take_status, Failure, OperationFuture},
    wire,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryServices {
    pub version: u32,
    pub source_admission: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdmissionErrorKind {
    NotEnabled,
    Busy,
    Closed,
    CapacityExhausted,
    SessionExpired,
    Sequence,
    ReceiptExpired,
    PayloadConflict,
    PendingObligations,
    IdentifierTooLong,
    AcceptanceUnknown,
    MetadataUnknown,
    Invalid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionError {
    pub kind: AdmissionErrorKind,
    pub message: String,
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}
impl std::error::Error for AdmissionError {}

impl From<PipeError> for AdmissionError {
    fn from(error: PipeError) -> Self {
        use AdmissionErrorKind as Kind;
        let kind = match &error {
            PipeError::Closed => Kind::Closed,
            PipeError::CapacityExhausted => Kind::CapacityExhausted,
            PipeError::AcceptanceUnknown { .. } => Kind::AcceptanceUnknown,
            PipeError::AcknowledgementUnknown { .. } => Kind::MetadataUnknown,
            PipeError::Backend(error) => match error.downcast_ref::<AdmissionRejection>() {
                Some(AdmissionRejection::NotEnabled) => Kind::NotEnabled,
                Some(AdmissionRejection::Busy) => Kind::Busy,
                Some(AdmissionRejection::SessionExpired) => Kind::SessionExpired,
                Some(AdmissionRejection::Sequence { .. }) => Kind::Sequence,
                Some(AdmissionRejection::ReceiptExpired(_)) => Kind::ReceiptExpired,
                Some(AdmissionRejection::PayloadConflict(_)) => Kind::PayloadConflict,
                Some(AdmissionRejection::PendingObligations) => Kind::PendingObligations,
                Some(AdmissionRejection::IdentifierTooLong) => Kind::IdentifierTooLong,
                None => Kind::Invalid,
            },
            _ => Kind::Invalid,
        };
        Self {
            kind,
            message: error.to_string(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptRequest {
    pub session: ProducerSession,
    pub sequence: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmitRequest<'a> {
    pub session: ProducerSession,
    pub sequence: u64,
    #[serde(borrow, with = "serde_bytes")]
    pub envelope: std::borrow::Cow<'a, [u8]>,
}

struct RetainedAdmission(SourceAdmissionV1);
// The callbacks, not this side, dereference host state. The contract permits
// concurrent retain/release and independent bounded requests.
unsafe impl Send for RetainedAdmission {}
unsafe impl Sync for RetainedAdmission {}
impl Drop for RetainedAdmission {
    fn drop(&mut self) {
        unsafe { self.0.release.expect("validated")(self.0.context) };
    }
}

#[derive(Clone)]
pub struct NativeAdmission {
    raw: Arc<RetainedAdmission>,
    codec: Arc<BinaryEnvelopeCodec>,
    identity: Arc<GraphProducerIdentity>,
}

impl NativeAdmission {
    /// # Safety
    /// The producer must obey the version-1 admission callback contract and
    /// remain callable until the retained capability is released.
    pub unsafe fn from_borrowed(
        raw: *const SourceAdmissionV1,
        codec: Arc<BinaryEnvelopeCodec>,
    ) -> Result<Self, Failure> {
        let raw = *unsafe { checked_table(raw)? };
        if raw.context.is_null()
            || raw.retain.is_none()
            || raw.release.is_none()
            || raw.request.is_none()
            || raw.report_failure.is_none()
            || raw.describe.is_none()
        {
            return Err(Failure::protocol("incomplete native admission capability"));
        }
        let description = unsafe { take_reply(raw.describe.expect("validated")(raw.context))? };
        if description.len() > abi::MAX_METADATA_BYTES {
            return Err(Failure::protocol(
                "native admission description is too large",
            ));
        }
        let identity: GraphProducerIdentity = wire::decode(&description)?;
        if !identity.persistent() || identity.incarnation().is_nil() {
            return Err(Failure::protocol(
                "native admission requires a persistent producer identity",
            ));
        }
        unsafe { raw.retain.expect("validated")(raw.context) };
        Ok(Self {
            raw: Arc::new(RetainedAdmission(raw)),
            codec,
            identity: Arc::new(identity),
        })
    }

    pub fn identity(&self) -> &GraphProducerIdentity {
        &self.identity
    }

    pub fn report_failure(&self, reason: &str) -> Result<(), Failure> {
        if reason.is_empty() || reason.len() > 4096 {
            return Err(Failure::protocol(
                "source failure must contain 1..=4096 UTF-8 bytes",
            ));
        }
        let table = &self.raw.0;
        unsafe {
            take_status(table.report_failure.expect("validated")(
                table.context,
                abi::BorrowedBytes::new(reason.as_bytes()),
            ))
        }
    }

    fn begin(
        &self,
        code: u32,
        input: &[u8],
        uncertainty: AdmissionErrorKind,
    ) -> Result<OperationFuture, AdmissionError> {
        let before = |error: Failure| AdmissionError {
            kind: match error.code {
                abi::status::BUSY => AdmissionErrorKind::Busy,
                abi::status::CLOSED => AdmissionErrorKind::Closed,
                _ => AdmissionErrorKind::Invalid,
            },
            message: error.to_string(),
        };
        let table = &self.raw.0;
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(table.request.expect("validated")(
                table.context,
                code,
                abi::BorrowedBytes::new(input),
                &mut operation,
            ))
            .map_err(before)?;
        }
        unsafe { OperationFuture::new(operation) }.map_err(|error| AdmissionError {
            kind: uncertainty,
            message: error.to_string(),
        })
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        code: u32,
        input: &[u8],
        uncertainty: AdmissionErrorKind,
    ) -> Result<T, AdmissionError> {
        let operation = self.begin(code, input, uncertainty)?;
        let after = |error: String| AdmissionError {
            kind: uncertainty,
            message: error,
        };
        let output = operation.await.map_err(|error| after(error.to_string()))?;
        wire::decode::<Result<T, AdmissionError>>(&output)
            .map_err(|error| after(error.to_string()))?
    }

    pub async fn register_producer(
        &self,
        producer: &ComponentId,
    ) -> Result<ProducerSession, AdmissionError> {
        self.call(
            abi::services::admission::REGISTER,
            &encode(producer)?,
            AdmissionErrorKind::MetadataUnknown,
        )
        .await
    }
    pub async fn retire_producer(&self, session: &ProducerSession) -> Result<(), AdmissionError> {
        self.call(
            abi::services::admission::RETIRE,
            &encode(session)?,
            AdmissionErrorKind::MetadataUnknown,
        )
        .await
    }
    pub async fn producer_status(
        &self,
        session: &ProducerSession,
    ) -> Result<ProducerStatus, AdmissionError> {
        self.call(
            abi::services::admission::STATUS,
            &encode(session)?,
            AdmissionErrorKind::Closed,
        )
        .await
    }
    pub async fn admission_receipt(
        &self,
        session: &ProducerSession,
        sequence: u64,
    ) -> Result<Option<AdmissionReceipt>, AdmissionError> {
        self.call(
            abi::services::admission::RECEIPT,
            &encode(&ReceiptRequest {
                session: session.clone(),
                sequence,
            })?,
            AdmissionErrorKind::Closed,
        )
        .await
    }
    pub async fn admit(
        &self,
        session: &ProducerSession,
        sequence: u64,
        input: &ChangeEnvelope,
    ) -> Result<AdmissionReceipt, AdmissionError> {
        let envelope = self.codec.encode(input).map_err(invalid)?;
        self.call(
            abi::services::admission::ADMIT,
            &encode(&AdmitRequest {
                session: session.clone(),
                sequence,
                envelope: envelope.into(),
            })?,
            AdmissionErrorKind::AcceptanceUnknown,
        )
        .await
    }
}

fn invalid(error: impl std::fmt::Display) -> AdmissionError {
    AdmissionError {
        kind: AdmissionErrorKind::Invalid,
        message: error.to_string(),
    }
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>, AdmissionError> {
    wire::encode(value).map_err(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport;
    use drasi_lib::computation::v1::GraphChangeCodec;
    use std::{
        ffi::c_void,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct State {
        retains: AtomicUsize,
        releases: AtomicUsize,
        mode: AtomicUsize,
    }
    unsafe extern "C" fn retain(context: *mut c_void) {
        let state = unsafe { &*context.cast::<State>() };
        state.retains.fetch_add(1, Ordering::SeqCst);
        unsafe { Arc::increment_strong_count(context.cast::<State>()) };
    }
    unsafe extern "C" fn release(context: *mut c_void) {
        let state = unsafe { &*context.cast::<State>() };
        state.releases.fetch_add(1, Ordering::SeqCst);
        unsafe { Arc::decrement_strong_count(context.cast::<State>()) };
    }
    unsafe extern "C" fn failure(_: *mut c_void, _: abi::BorrowedBytes) -> abi::Status {
        abi::Status::ok()
    }
    unsafe extern "C" fn describe(_: *mut c_void) -> abi::Reply {
        transport::reply_boundary(|| {
            let identity: GraphProducerIdentity = serde_json::from_value(serde_json::json!({
                "construction_scope":"scope","graph_id":"graph","component_id":"source","stream":"in",
                "incarnation":"00000000-0000-0000-0000-000000000001","persistent":true
            })).unwrap();
            wire::encode(&identity).map_err(Failure::from)
        })
    }
    unsafe extern "C" fn request(
        context: *mut c_void,
        _: u32,
        _: abi::BorrowedBytes,
        out: *mut abi::OperationHandle,
    ) -> abi::Status {
        transport::status_boundary(|| {
            let mode = unsafe { &*context.cast::<State>() }
                .mode
                .load(Ordering::SeqCst);
            if mode == 0 {
                return Err(Failure::new(abi::status::BUSY, "not submitted"));
            }
            let operation = transport::export_operation(
                async move {
                    match mode {
                        1 => Err(Failure::failed("response lost")),
                        2 => Ok(vec![0xc1]),
                        _ => wire::encode(&Result::<(), AdmissionError>::Err(AdmissionError {
                            kind: AdmissionErrorKind::PendingObligations,
                            message: "pending".into(),
                        }))
                        .map_err(Failure::from),
                    }
                },
                None,
            );
            unsafe { transport::write_out(out, operation) }
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn callback_ownership_and_definite_versus_uncertain_failures() {
        let state = Arc::new(State {
            retains: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
            mode: AtomicUsize::new(0),
        });
        let mut table = SourceAdmissionV1 {
            header: abi::Header::new::<SourceAdmissionV1>(),
            context: Arc::as_ptr(&state) as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            request: Some(request),
            report_failure: None,
            describe: Some(describe),
        };
        let codec = Arc::new(wire::codec(&[GraphChangeCodec::schema()]).unwrap());
        assert!(unsafe { NativeAdmission::from_borrowed(&table, codec.clone()) }.is_err());
        assert_eq!(state.retains.load(Ordering::SeqCst), 0);
        table.report_failure = Some(failure);
        let service = unsafe { NativeAdmission::from_borrowed(&table, codec) }.unwrap();
        let copy = service.clone();
        assert_eq!(state.retains.load(Ordering::SeqCst), 1);
        let producer = ComponentId::try_new("client").unwrap();
        let session = ProducerSession {
            incarnation: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
            producer: producer.clone(),
            epoch: 1,
        };
        assert_eq!(
            service.register_producer(&producer).await.unwrap_err().kind,
            AdmissionErrorKind::Busy
        );
        for mode in [1, 2] {
            state.mode.store(mode, Ordering::SeqCst);
            assert_eq!(
                service.register_producer(&producer).await.unwrap_err().kind,
                AdmissionErrorKind::MetadataUnknown
            );
            assert_eq!(
                service.retire_producer(&session).await.unwrap_err().kind,
                AdmissionErrorKind::MetadataUnknown
            );
            assert_eq!(
                service.producer_status(&session).await.unwrap_err().kind,
                AdmissionErrorKind::Closed
            );
            assert_eq!(
                service
                    .admission_receipt(&session, 1)
                    .await
                    .unwrap_err()
                    .kind,
                AdmissionErrorKind::Closed
            );
            let input = GraphChangeCodec::encode_changes(
                &[],
                drasi_lib::computation::v1::StreamId::try_new("in").unwrap(),
                1,
                None,
            )
            .unwrap();
            assert_eq!(
                service.admit(&session, 1, &input).await.unwrap_err().kind,
                AdmissionErrorKind::AcceptanceUnknown
            );
        }
        state.mode.store(3, Ordering::SeqCst);
        assert_eq!(
            service.retire_producer(&session).await.unwrap_err().kind,
            AdmissionErrorKind::PendingObligations
        );
        assert!(service.report_failure("").is_err());
        assert!(service.report_failure(&"a".repeat(4097)).is_err());
        service.report_failure("listener failed").unwrap();
        drop(service);
        assert_eq!(state.releases.load(Ordering::SeqCst), 0);
        drop(copy);
        assert_eq!(state.releases.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&state), 1);
    }
}
