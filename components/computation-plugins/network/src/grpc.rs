// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    config::{backoff, duration, GrpcSinkConfig, SourceConfig},
    ingress::{Ingress, SourceRuntime},
    proto::{reaction, source},
    wire,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_plugin_sdk::Scope;
use drasi_lib::computation::v1::{
    ComponentDescriptor, ComputationComponent, EnvelopeSink, EnvelopeSource, InputEnvelope,
    OutputEnvelope, SinkCompletion,
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
    ) -> Result<Self> {
        Ok(Self(SourceRuntime::new(descriptor, config, scope)?))
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
            message: "Native volatile gRPC source is healthy".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }))
    }
}
async fn serve(listener: TcpListener, ingress: Arc<Ingress>) -> Result<()> {
    let cancel = ingress.cancel.clone();
    let service =
        source::source_service_server::SourceServiceServer::new(SourceService(ingress.clone()))
            .max_decoding_message_size(ingress.config.max_message_bytes);
    tonic::transport::Server::builder()
        .timeout(Duration::from_millis(ingress.config.timeout_ms))
        .add_service(service)
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::TcpListenerStream::new(listener),
            cancel.cancelled_owned(),
        )
        .await?;
    Ok(())
}

type ReactionClient =
    reaction::reaction_service_client::ReactionServiceClient<tonic::transport::Channel>;
pub(crate) struct GrpcSink {
    descriptor: ComponentDescriptor,
    config: GrpcSinkConfig,
    client: Option<ReactionClient>,
}
impl GrpcSink {
    pub fn new(descriptor: ComponentDescriptor, config: GrpcSinkConfig) -> Result<Self> {
        Ok(Self {
            descriptor,
            config,
            client: None,
        })
    }
    async fn connect(&self) -> Result<ReactionClient> {
        let endpoint = tonic::transport::Endpoint::from_shared(self.config.connection_uri())?
            .connect_timeout(duration(
                self.config.initial_connection_timeout_ms,
                "initialConnectionTimeoutMs",
            )?)
            .timeout(duration(self.config.timeout_ms, "timeoutMs")?);
        let channel = tokio::time::timeout(
            Duration::from_millis(self.config.initial_connection_timeout_ms),
            endpoint.connect(),
        )
        .await
        .context("gRPC connection timeout")??;
        Ok(ReactionClient::new(channel))
    }
    async fn deliver(&mut self, item: reaction::QueryResultItem) -> Result<()> {
        let sequence = item.sequence;
        let message = reaction::ProcessResultsRequest {
            results: Some(reaction::QueryResult {
                query_id: self.config.query_id.clone(),
                timestamp: item.timestamp,
                results: vec![item],
            }),
            metadata: self.config.metadata.clone().into_iter().collect(),
        };
        for attempt in 0..=self.config.max_retries {
            let mut request = Request::new(message.clone());
            for (name, value) in &self.config.metadata {
                request.metadata_mut().insert(
                    name.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>()?,
                    value.parse()?,
                );
            }
            request.set_timeout(Duration::from_millis(self.config.timeout_ms));
            let client = self.client.as_mut().context("gRPC sink has not started")?;
            let result = async {
                let response = tokio::time::timeout(
                    Duration::from_millis(self.config.timeout_ms),
                    client.process_results(request),
                )
                .await
                .context("gRPC delivery timeout")??
                .into_inner();
                anyhow::ensure!(
                    response.success,
                    "gRPC receiver rejected delivery: {}",
                    response.error
                );
                Ok(())
            }
            .await;
            match result {
                Ok(()) => return Ok(()),
                Err(error) if attempt == self.config.max_retries => return Err(error),
                Err(error) => {
                    eprintln!("drasi.network gRPC retry: query={} sequence={sequence} attempt={} error={error:#}",
                        self.config.query_id, attempt + 1);
                    // Tonic's owned channel reconnects on subsequent calls. Do not
                    // start a second untracked connection/retry worker.
                    backoff(attempt).await;
                }
            }
        }
        unreachable!("retry loop always returns")
    }
}
#[async_trait]
impl ComputationComponent for GrpcSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(self.client.is_none(), "gRPC sink already started");
        for attempt in 0..self.config.connection_retry_attempts {
            match self.connect().await {
                Ok(client) => {
                    self.client = Some(client);
                    return Ok(());
                }
                Err(error) if attempt + 1 == self.config.connection_retry_attempts => {
                    return Err(error)
                }
                Err(error) => {
                    eprintln!(
                        "drasi.network gRPC connect retry: component={} attempt={} error={error:#}",
                        self.descriptor.id(),
                        attempt + 1
                    );
                    backoff(attempt).await;
                }
            }
        }
        unreachable!("positive connection attempts")
    }
    async fn stop(&mut self) -> Result<()> {
        self.client = None;
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for GrpcSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        anyhow::ensure!(self.client.is_some(), "gRPC sink has not started");
        for notification in wire::notifications(&input, &self.config.query_id, &self.config.stream)?
        {
            let item = wire::proto_item(&notification, self.config.output_format)?;
            let result = self.deliver(item).await;
            self.config.failure_policy.delivery(
                result,
                self.descriptor.id().as_str(),
                &self.config.query_id,
                notification.sequence_id,
            )?;
        }
        Ok(())
    }
}
