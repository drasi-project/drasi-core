// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{durable::invalid, ingress::Ingress, proto::admission as proto, wire};
use drasi_computation_plugin_sdk::{AdmissionError, AdmissionErrorKind};
use drasi_lib::computation::v1::{AdmissionReceipt, ComponentId, ProducerSession, ProducerStatus};
use futures_util::Stream;
use std::{pin::Pin, sync::Arc, time::Duration};
use tonic::{Request, Response, Status};

pub(crate) struct AdmissionService(pub Arc<Ingress>);

impl From<ProducerSession> for proto::ProducerSession {
    fn from(value: ProducerSession) -> Self {
        Self {
            incarnation: value.incarnation.to_string(),
            producer: value.producer.to_string(),
            epoch: value.epoch,
        }
    }
}
impl From<AdmissionReceipt> for proto::Receipt {
    fn from(value: AdmissionReceipt) -> Self {
        Self {
            session: Some(value.session.into()),
            sequence: value.sequence,
            position: value.position,
        }
    }
}
impl From<ProducerStatus> for proto::ProducerStatus {
    fn from(value: ProducerStatus) -> Self {
        Self {
            session: Some(value.session.into()),
            next_sequence: value.next_sequence,
            earliest_receipt: value.earliest_receipt,
        }
    }
}
impl From<AdmissionError> for proto::AdmissionError {
    fn from(value: AdmissionError) -> Self {
        use AdmissionErrorKind::*;
        let kind = match value.kind {
            NotEnabled => proto::ErrorKind::NotEnabled,
            Busy => proto::ErrorKind::Busy,
            Closed => proto::ErrorKind::Closed,
            CapacityExhausted => proto::ErrorKind::CapacityExhausted,
            SessionExpired => proto::ErrorKind::SessionExpired,
            Sequence => proto::ErrorKind::Sequence,
            ReceiptExpired => proto::ErrorKind::ReceiptExpired,
            PayloadConflict => proto::ErrorKind::PayloadConflict,
            PendingObligations => proto::ErrorKind::PendingObligations,
            IdentifierTooLong => proto::ErrorKind::IdentifierTooLong,
            AcceptanceUnknown => proto::ErrorKind::AcceptanceUnknown,
            MetadataUnknown => proto::ErrorKind::MetadataUnknown,
            Invalid => proto::ErrorKind::Invalid,
        };
        eprintln!("drasi.network durable gRPC request failed: {value}");
        Self {
            kind: kind as i32,
            message: value.message,
        }
    }
}
fn session(value: Option<proto::ProducerSession>) -> Result<ProducerSession, AdmissionError> {
    let value = value.ok_or_else(|| invalid("producer session is required"))?;
    Ok(ProducerSession {
        incarnation: value.incarnation.parse().map_err(invalid)?,
        producer: ComponentId::try_new(value.producer).map_err(invalid)?,
        epoch: value.epoch,
    })
}
fn check(ingress: &Ingress, source: &str) -> Result<(), AdmissionError> {
    if source != ingress.config.source_id {
        return Err(invalid("source ID mismatch"));
    }
    ingress.admission_service()?;
    Ok(())
}
async fn admit(
    ingress: &Ingress,
    request: proto::AdmitRequest,
) -> Result<AdmissionReceipt, AdmissionError> {
    check(ingress, &request.source_id)?;
    let session = session(request.session)?;
    let change = wire::grpc_change(
        request.event.ok_or_else(|| invalid("event is required"))?,
        &ingress.config.source_id,
    )
    .map_err(invalid)?;
    let outcome = ingress
        .admit_durable(&session, request.sequence, vec![change])
        .await?;
    if let Some(error) = outcome.error {
        return Err(error);
    }
    outcome
        .receipts
        .into_iter()
        .next()
        .ok_or_else(|| invalid("admission returned no receipt"))
}
fn admit_response(result: Result<AdmissionReceipt, AdmissionError>) -> proto::AdmitResponse {
    proto::AdmitResponse {
        result: Some(match result {
            Ok(value) => proto::admit_response::Result::Receipt(value.into()),
            Err(error) => proto::admit_response::Result::Error(error.into()),
        }),
    }
}

#[tonic::async_trait]
impl proto::admission_service_server::AdmissionService for AdmissionService {
    async fn register_producer(
        &self,
        request: Request<proto::RegisterRequest>,
    ) -> Result<Response<proto::RegisterResponse>, Status> {
        let _permit = self
            .0
            .request_permit()
            .map_err(|error| Status::resource_exhausted(error.to_string()))?;
        let request = request.into_inner();
        let result = async {
            check(&self.0, &request.source_id)?;
            let producer = ComponentId::try_new(request.producer).map_err(invalid)?;
            self.0
                .admission_call(
                    AdmissionErrorKind::MetadataUnknown,
                    self.0.admission_service()?.register_producer(&producer),
                )
                .await
        }
        .await;
        Ok(Response::new(proto::RegisterResponse {
            result: Some(match result {
                Ok(value) => proto::register_response::Result::Session(value.into()),
                Err(error) => proto::register_response::Result::Error(error.into()),
            }),
        }))
    }
    async fn producer_status(
        &self,
        request: Request<proto::SessionRequest>,
    ) -> Result<Response<proto::StatusResponse>, Status> {
        let _permit = self
            .0
            .request_permit()
            .map_err(|error| Status::resource_exhausted(error.to_string()))?;
        let request = request.into_inner();
        let result = async {
            check(&self.0, &request.source_id)?;
            let session = session(request.session)?;
            self.0
                .admission_call(
                    AdmissionErrorKind::Closed,
                    self.0.admission_service()?.producer_status(&session),
                )
                .await
        }
        .await;
        Ok(Response::new(proto::StatusResponse {
            result: Some(match result {
                Ok(value) => proto::status_response::Result::Status(value.into()),
                Err(error) => proto::status_response::Result::Error(error.into()),
            }),
        }))
    }
    async fn retire_producer(
        &self,
        request: Request<proto::SessionRequest>,
    ) -> Result<Response<proto::RetireResponse>, Status> {
        let _permit = self
            .0
            .request_permit()
            .map_err(|error| Status::resource_exhausted(error.to_string()))?;
        let request = request.into_inner();
        let result = async {
            check(&self.0, &request.source_id)?;
            let session = session(request.session)?;
            self.0
                .admission_call(
                    AdmissionErrorKind::MetadataUnknown,
                    self.0.admission_service()?.retire_producer(&session),
                )
                .await
        }
        .await;
        Ok(Response::new(proto::RetireResponse {
            result: Some(match result {
                Ok(()) => proto::retire_response::Result::Retired(true),
                Err(error) => proto::retire_response::Result::Error(error.into()),
            }),
        }))
    }
    async fn admission_receipt(
        &self,
        request: Request<proto::ReceiptRequest>,
    ) -> Result<Response<proto::ReceiptResponse>, Status> {
        let _permit = self
            .0
            .request_permit()
            .map_err(|error| Status::resource_exhausted(error.to_string()))?;
        let request = request.into_inner();
        let result = async {
            check(&self.0, &request.source_id)?;
            let session = session(request.session)?;
            self.0
                .admission_call(
                    AdmissionErrorKind::Closed,
                    self.0
                        .admission_service()?
                        .admission_receipt(&session, request.sequence),
                )
                .await
        }
        .await;
        Ok(Response::new(proto::ReceiptResponse {
            result: Some(match result {
                Ok(Some(value)) => proto::receipt_response::Result::Receipt(value.into()),
                Ok(None) => proto::receipt_response::Result::NotAccepted(true),
                Err(error) => proto::receipt_response::Result::Error(error.into()),
            }),
        }))
    }
    async fn admit(
        &self,
        request: Request<proto::AdmitRequest>,
    ) -> Result<Response<proto::AdmitResponse>, Status> {
        let _permit = self
            .0
            .request_permit()
            .map_err(|error| Status::resource_exhausted(error.to_string()))?;
        Ok(Response::new(admit_response(
            admit(&self.0, request.into_inner()).await,
        )))
    }
    type StreamEventsStream =
        Pin<Box<dyn Stream<Item = Result<proto::AdmitResponse, Status>> + Send>>;
    async fn stream_events(
        &self,
        request: Request<tonic::Streaming<proto::AdmitRequest>>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        let permit = self
            .0
            .request_permit()
            .map_err(|error| Status::resource_exhausted(error.to_string()))?;
        let ingress = self.0.clone();
        let mut input = request.into_inner();
        let output = async_stream::try_stream! {
            let _permit = permit;
            loop {
                let request = tokio::select! {
                    biased;
                    _ = ingress.cancel.cancelled() => Err(Status::cancelled("native source stopped")),
                    result = tokio::time::timeout(Duration::from_millis(ingress.config.timeout_ms), input.message()) =>
                        result.map_err(|_| Status::deadline_exceeded("admission stream idle timeout")).and_then(|value| value),
                }?;
                let Some(request) = request else { break; };
                let result = admit(&ingress, request).await;
                let failed = result.is_err();
                yield admit_response(result);
                if failed { break; }
            }
        };
        Ok(Response::new(Box::pin(output)))
    }
}
