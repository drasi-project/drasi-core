// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    config::{backoff, duration, GrpcSinkConfig},
    proto::reaction,
    wire,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_lib::computation::v1::{
    ComponentDescriptor, ComputationComponent, EnvelopeSink, InputEnvelope, SinkCompletion,
};
use std::time::Duration;
use tonic::Request;

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
