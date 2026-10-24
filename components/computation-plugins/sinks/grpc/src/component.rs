// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    config::{backoff, duration, BatchingConfig, GrpcSinkConfig},
    proto::reaction,
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, InputEnvelope, SinkBatching, SinkCompletion,
};
use drasi_computation_network_common::{
    batching::{AdaptiveBatchConfig, AdaptivePolicy},
    config::normalized_headers,
    wire,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSink};
use drasi_grpc_common::output::{
    batch::{item_wire_size, merge_metadata, request_overhead, BatchKey, PendingBatch},
    config::GrpcReactionConfig,
    send::{categorize_error, latest_item_timestamp},
    templates::{build_proto_item, QueryEmissionContext, TemplateEngine},
};
use std::{collections::BTreeMap, time::Duration};
use tonic::Request;

type ReactionClient =
    reaction::reaction_service_client::ReactionServiceClient<tonic::transport::Channel>;

pub(crate) struct GrpcSink {
    descriptor: ComponentDescriptor,
    config: GrpcSinkConfig,
    output: GrpcReactionConfig,
    renderer: Option<TemplateEngine>,
    adaptive: Option<AdaptivePolicy>,
    client: Option<ReactionClient>,
}

impl GrpcSink {
    pub fn new(descriptor: ComponentDescriptor, config: GrpcSinkConfig) -> Result<Self> {
        let output = config.reaction_config();
        let renderer = output
            .output_templates
            .as_ref()
            .filter(|templates| templates.has_renderable_templates())
            .map(|_| TemplateEngine::new());
        let adaptive = output
            .batching
            .as_adaptive_config()
            .map(|adaptive| {
                let max_wait = Duration::from_millis(adaptive.adaptive_batch_timeout_ms);
                AdaptivePolicy::new(
                    AdaptiveBatchConfig {
                        min_batch_size: adaptive.adaptive_min_batch_size,
                        max_batch_size: adaptive.adaptive_max_batch_size,
                        throughput_window: Duration::from_millis(
                            adaptive.adaptive_window_size as u64 * 100,
                        ),
                        max_wait_time: max_wait,
                        min_wait_time: Duration::from_millis(100).min(max_wait),
                        adaptive_enabled: true,
                    },
                    config.max_batch_bytes,
                )
            })
            .transpose()?;
        Ok(Self {
            descriptor,
            config,
            output,
            renderer,
            adaptive,
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
        Ok(ReactionClient::new(channel)
            .max_encoding_message_size(self.config.max_request_bytes)
            .max_decoding_message_size(self.config.max_request_bytes))
    }

    async fn deliver(&mut self, batch: &PendingBatch) -> Result<()> {
        let metadata = batch.key.metadata_map();
        let expected = u32::try_from(batch.items.len())?;
        let message = reaction::ProcessResultsRequest {
            results: Some(reaction::QueryResult {
                query_id: batch.key.query_id.clone(),
                timestamp: latest_item_timestamp(&batch.items),
                results: batch.items.clone(),
            }),
            metadata: metadata.clone(),
        };
        for attempt in 0..=self.config.max_retries {
            let mut request = Request::new(message.clone());
            for (name, value) in &metadata {
                request.metadata_mut().insert(
                    name.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>()?,
                    value.parse()?,
                );
            }
            request.set_timeout(Duration::from_millis(self.config.timeout_ms));
            let client = self.client.as_mut().context("gRPC sink has not started")?;
            let result = tokio::time::timeout(
                Duration::from_millis(self.config.timeout_ms),
                client.process_results(request),
            )
            .await;
            let (error, kind) = match result {
                Ok(Ok(response)) => {
                    let response = response.into_inner();
                    if response.success {
                        ensure!(response.items_processed == expected,
                            "gRPC receiver acknowledged {} of {expected} items; batch is incomplete",
                            response.items_processed);
                        return Ok(());
                    }
                    (
                        anyhow::anyhow!("gRPC receiver rejected delivery: {}", response.error),
                        "Application",
                    )
                }
                Ok(Err(error)) => {
                    let kind = categorize_error(&error.to_string().to_lowercase());
                    (anyhow::Error::from(error), kind)
                }
                Err(error) => (
                    anyhow::Error::from(error).context("gRPC delivery timeout"),
                    "Timeout",
                ),
            };
            if attempt == self.config.max_retries {
                return Err(error);
            }
            eprintln!(
                "drasi.network gRPC retry: query={} sequence={} attempt={} error={error:#}",
                batch.key.query_id,
                batch.seen_seq,
                attempt + 1
            );
            if matches!(
                kind,
                "GoAway" | "Connection" | "BrokenPipe" | "ChannelClosed" | "Unavailable"
            ) {
                match self.connect().await {
                    Ok(client) => self.client = Some(client),
                    Err(error) => eprintln!("drasi.network gRPC reconnect failed: {error:#}"),
                }
            }
            if kind == "ResourceExhausted" {
                tokio::time::sleep(Duration::from_secs(5)).await;
            } else {
                backoff(attempt).await;
            }
        }
        unreachable!("retry loop always returns")
    }

    async fn flush(&mut self, batch: PendingBatch) -> Result<()> {
        let result = self.deliver(&batch).await;
        self.config.failure_policy.delivery(
            result,
            self.descriptor.id().as_str(),
            &batch.key.query_id,
            batch.seen_seq,
        )
    }

    fn item_limit(&self) -> usize {
        match &self.adaptive {
            Some(adaptive) => adaptive.current().max_operations,
            None => match self.output.batching {
                BatchingConfig::Fixed { batch_size, .. } => batch_size,
                BatchingConfig::Adaptive { .. } => unreachable!("adaptive policy installed"),
            },
        }
    }

    async fn process(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        ensure!(self.client.is_some(), "gRPC sink has not started");
        let results = inputs
            .iter()
            .map(|input| {
                wire::query_result(
                    input,
                    &self.config.query_id,
                    &self.config.stream,
                    &self.config.query_streams,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let item_limit = self.item_limit();
        let mut pending: Option<PendingBatch> = None;
        let mut pending_bytes = 0usize;
        let mut count = 0;
        for result in results {
            let emission = QueryEmissionContext {
                query_id: &result.query_id,
                sequence: result.sequence,
                timestamp: result.timestamp,
                metadata: &result.metadata,
            };
            let mut base = self.output.metadata.clone();
            base.insert("x-query-sequence".into(), result.query_id.clone());
            for (index, diff) in result.results.iter().enumerate() {
                let Some(built) =
                    build_proto_item(&self.output, self.renderer.as_ref(), &emission, diff)
                else {
                    continue;
                };
                let metadata = merge_metadata(&base, &built.request_metadata);
                let metadata = (|| -> Result<_> {
                    for (key, value) in &metadata {
                        let _: tonic::metadata::MetadataKey<tonic::metadata::Ascii> =
                            key.parse()?;
                        let _: tonic::metadata::MetadataValue<tonic::metadata::Ascii> =
                            value.parse()?;
                    }
                    Ok(normalized_headers(
                        &metadata.into_iter().collect::<BTreeMap<_, _>>(),
                        &result.query_id,
                    )?
                    .into_iter()
                    .collect())
                })();
                let metadata = match metadata {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        self.config.failure_policy.delivery(
                            Err(error),
                            self.descriptor.id().as_str(),
                            &result.query_id,
                            result.sequence,
                        )?;
                        continue;
                    }
                };
                let key = BatchKey::new(&result.query_id, metadata);
                let item_bytes = item_wire_size(&built.item);
                let overhead = request_overhead(&key);
                if overhead.saturating_add(item_bytes) > self.config.max_request_bytes {
                    self.config.failure_policy.delivery(
                        Err(anyhow::anyhow!("gRPC item exceeds maxRequestBytes")),
                        self.descriptor.id().as_str(),
                        &result.query_id,
                        result.sequence,
                    )?;
                    continue;
                }
                if pending.as_ref().is_some_and(|batch| {
                    !batch.can_accept(&key)
                        || batch.items.len() >= item_limit
                        || pending_bytes.saturating_add(item_bytes) > self.config.max_request_bytes
                }) {
                    self.flush(pending.take().expect("pending batch")).await?;
                }
                let terminal = index + 1 == result.results.len();
                match &mut pending {
                    Some(batch) => {
                        batch.push(built.item, terminal);
                        pending_bytes += item_bytes;
                    }
                    None => {
                        pending_bytes = overhead + item_bytes;
                        pending = Some(PendingBatch::new(key, built.item, terminal));
                    }
                }
                count += 1;
            }
        }
        if let Some(batch) = pending {
            self.flush(batch).await?;
        }
        if let Some(adaptive) = &mut self.adaptive {
            adaptive.record(count);
        }
        Ok(())
    }
}

#[async_trait]
impl NativeComponent for GrpcSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    async fn start(&mut self) -> Result<()> {
        ensure!(self.client.is_none(), "gRPC sink already started");
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
impl NativeSink for GrpcSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn batching(&self) -> Result<Option<SinkBatching>> {
        if let Some(adaptive) = &self.adaptive {
            return Ok(Some(adaptive.current()));
        }
        let BatchingConfig::Fixed {
            batch_size,
            batch_flush_timeout_ms,
        } = self.output.batching
        else {
            unreachable!("adaptive policy installed")
        };
        Ok((batch_size > 1).then_some(SinkBatching {
            max_inputs: batch_size.min(1024),
            max_operations: batch_size,
            max_bytes: self.config.max_batch_bytes,
            max_delay_ms: batch_flush_timeout_ms,
        }))
    }
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        self.batching()?
            .context("gRPC batching is disabled")?
            .validate_inputs(&inputs)?;
        self.process(inputs).await
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.process(vec![input]).await
    }
}
