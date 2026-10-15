// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    config::SourceConfig,
    ingress::{Ingress, SourceRuntime},
    proto::source,
    wire,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_plugin_sdk::{NativeAdmission, Scope};
use drasi_lib::computation::v1::{
    ComponentDescriptor, ComputationComponent, EnvelopeSource, OutputEnvelope,
};
use futures_util::Stream;
use std::{pin::Pin, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tonic::{Request, Response, Status};

pub(crate) struct GrpcSource(SourceRuntime);
impl GrpcSource {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: SourceConfig,
        scope: Option<&Scope>,
        admission: Option<NativeAdmission>,
    ) -> Result<Self> {
        Ok(Self(SourceRuntime::new(
            descriptor, config, scope, admission,
        )?))
    }
}
#[async_trait]
impl ComputationComponent for GrpcSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.0.configuration()
    }
    async fn start(&mut self) -> Result<()> {
        self.0.start(serve).await
    }
    async fn stop(&mut self) -> Result<()> {
        self.0.stop().await
    }
}
#[async_trait]
impl EnvelopeSource for GrpcSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.0.next().await
    }
}

struct SourceService(Arc<Ingress>);
type ResponseStream<T> = Pin<Box<dyn Stream<Item = std::result::Result<T, Status>> + Send>>;
#[tonic::async_trait]
impl source::source_service_server::SourceService for SourceService {
    async fn submit_event(
        &self,
        request: Request<source::SubmitEventRequest>,
    ) -> std::result::Result<Response<source::SubmitEventResponse>, Status> {
        if self.0.durable().is_some() {
            return Err(Status::failed_precondition(
                "durable source requires AdmissionService v1",
            ));
        }
        let _permit = self
            .0
            .request_permit()
            .map_err(|error| Status::resource_exhausted(error.to_string()))?;
        let converted = request
            .into_inner()
            .event
            .context("event is required")
            .and_then(|event| wire::grpc_change(event, &self.0.config.source_id));
        let change = match converted {
            Ok(change) => change,
            Err(error) => {
                eprintln!("drasi.network gRPC invalid event: {error:#}");
                return Ok(Response::new(source::SubmitEventResponse {
                    success: false,
                    message: "Invalid event data".into(),
                    error: error.to_string(),
                    event_id: String::new(),
                }));
            }
        };
        self.0.admit(vec![change]).await.map_err(|error| {
            Status::unavailable(format!(
                "admission failed after {} events: {:#}",
                error.accepted, error.error
            ))
        })?;
        Ok(Response::new(source::SubmitEventResponse {
            success: true,
            message: "Event processed successfully".into(),
            error: String::new(),
            event_id: String::new(),
        }))
    }
    type StreamEventsStream = ResponseStream<source::StreamEventResponse>;
    async fn stream_events(
        &self,
        request: Request<tonic::Streaming<source::SourceChange>>,
    ) -> std::result::Result<Response<Self::StreamEventsStream>, Status> {
        if self.0.durable().is_some() {
            return Err(Status::failed_precondition(
                "durable source requires AdmissionService v1",
            ));
        }
        if self.0.cancel.is_cancelled() {
            return Err(Status::unavailable("source stopped"));
        }
        let permit = self
            .0
            .request_permit()
            .map_err(|error| Status::resource_exhausted(error.to_string()))?;
        let ingress = self.0.clone();
        let mut input = request.into_inner();
        let output = async_stream::try_stream! {
            let _permit = permit;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = ingress.cancel.cancelled() => Err(Status::cancelled("native source stopped")),
                    result = tokio::time::timeout(Duration::from_millis(ingress.config.timeout_ms), input.message()) =>
                        result.map_err(|_| Status::deadline_exceeded("native source stream idle timeout")).and_then(|value| value),
                }?;
                let Some(event) = event else { break; };
                let change = match wire::grpc_change(event, &ingress.config.source_id) {
                    Ok(change) => change,
                    Err(error) => {
                        eprintln!("drasi.network gRPC stream invalid event: {error:#}");
                        yield source::StreamEventResponse {
                            success:false, message:"Invalid event data".into(), error:error.to_string(), events_processed:0,
                        };
                        break;
                    }
                };
                ingress.admit(vec![change]).await.map_err(|error| Status::unavailable(format!(
                    "admission failed after {} events: {:#}", error.accepted, error.error)))?;
                // Delta counts: test-run-host sums responses in both dispatchers.
                yield source::StreamEventResponse {
                    success:true, message:"Event processed successfully".into(), error:String::new(), events_processed:1,
                };
            }
        };
        Ok(Response::new(Box::pin(output)))
    }
    type RequestBootstrapStream = ResponseStream<source::BootstrapResponse>;
    async fn request_bootstrap(
        &self,
        _: Request<source::BootstrapRequest>,
    ) -> std::result::Result<Response<Self::RequestBootstrapStream>, Status> {
        Err(Status::unimplemented(
            "native volatile ingress has no bootstrap provider",
        ))
    }
    async fn health_check(
        &self,
        _: Request<()>,
    ) -> std::result::Result<Response<source::HealthCheckResponse>, Status> {
        if self.0.cancel.is_cancelled() {
            return Err(Status::unavailable("source stopped"));
        }
        Ok(Response::new(source::HealthCheckResponse {
            status: source::health_check_response::Status::Healthy as i32,
            message: if self.0.durable().is_some() {
                "Native durable gRPC source is healthy"
            } else {
                "Native volatile gRPC source is healthy"
            }
            .into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }))
    }
}
async fn serve(listener: TcpListener, ingress: Arc<Ingress>) -> Result<()> {
    let cancel = ingress.cancel.clone();
    let service =
        source::source_service_server::SourceServiceServer::new(SourceService(ingress.clone()))
            .max_decoding_message_size(ingress.config.max_message_bytes);
    let admission = crate::proto::admission::admission_service_server::AdmissionServiceServer::new(
        crate::durable_grpc::AdmissionService(ingress.clone()),
    )
    .max_decoding_message_size(ingress.config.max_message_bytes);
    tonic::transport::Server::builder()
        .timeout(Duration::from_millis(ingress.config.timeout_ms))
        .add_service(service)
        .add_service(admission)
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            cancel.cancelled_owned(),
        )
        .await?;
    Ok(())
}
