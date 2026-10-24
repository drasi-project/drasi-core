// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    config::{backoff, duration, HttpSinkConfig},
    wire,
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, InputEnvelope, SinkBatching, SinkCompletion,
};
use drasi_computation_network_common::batching::AdaptivePolicy;
use drasi_computation_plugin_sdk::{NativeComponent, NativeSink};
use drasi_http_common::{
    adaptive_batcher::AdaptiveBatchConfig,
    reaction::{
        config::{HttpCallExt, HttpCallSpec, HttpReactionConfig, TemplateRouting},
        output::{BatchEnvelope, DefaultChangeNotification},
        process::{
            build_handlebars, is_retryable_status, render_batch_item, render_request,
            RenderedRequest,
        },
    },
};
use std::time::Duration;

pub(crate) struct HttpSink {
    descriptor: ComponentDescriptor,
    config: HttpSinkConfig,
    output: HttpReactionConfig,
    renderer: handlebars::Handlebars<'static>,
    adaptive: Option<AdaptivePolicy>,
    client: Option<reqwest::Client>,
}

impl HttpSink {
    pub fn new(descriptor: ComponentDescriptor, config: HttpSinkConfig) -> Result<Self> {
        let adaptive = config
            .adaptive
            .as_ref()
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
                        min_wait_time: Duration::from_millis(1).min(max_wait),
                        adaptive_enabled: true,
                    },
                    config.max_batch_bytes,
                )
            })
            .transpose()?;
        Ok(Self {
            descriptor,
            output: config.reaction_config()?,
            renderer: build_handlebars(),
            adaptive,
            config,
            client: None,
        })
    }

    async fn deliver(&self, request: &RenderedRequest, query: &str, sequence: u64) -> Result<()> {
        let client = self.client.as_ref().context("HTTP sink has not started")?;
        ensure!(
            request.body.len() <= self.config.max_request_bytes,
            "HTTP body exceeds maxRequestBytes"
        );
        for attempt in 0..=self.config.max_retries {
            let response = client
                .request(request.method.clone(), &request.url)
                .headers(request.headers.clone())
                .body(request.body.clone())
                .send()
                .await;
            let error = match response {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => {
                    let error = anyhow::anyhow!("HTTP delivery returned {}", response.status());
                    if !is_retryable_status(response.status()) {
                        return Err(error);
                    }
                    error
                }
                Err(error) => error.without_url().into(),
            };
            if attempt == self.config.max_retries {
                return Err(error);
            }
            eprintln!("drasi.network HTTP retry: query={query} sequence={sequence} attempt={} error={error:#}", attempt + 1);
            backoff(attempt).await;
        }
        unreachable!("retry loop always returns")
    }

    fn render(&self, notification: &DefaultChangeNotification) -> Result<RenderedRequest> {
        let mut spec = self
            .output
            .get_template_spec(&notification.query_id, notification.operation_type())
            .cloned()
            .unwrap_or_else(|| HttpCallSpec {
                template: String::new(),
                extension: HttpCallExt {
                    url: if self.config.url.is_empty() {
                        format!("/changes/{}", notification.query_id)
                    } else {
                        self.config.url.clone()
                    },
                    ..Default::default()
                },
            });
        let mut headers = self.config.headers.clone();
        headers.insert("x-query-sequence".into(), notification.query_id.clone());
        let mut combined = headers
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        for (name, value) in spec.extension.headers {
            let name: reqwest::header::HeaderName = name.parse()?;
            combined.insert(name.to_string(), value);
        }
        spec.extension.headers = combined;
        let request = render_request(
            &self.renderer,
            &self.output.base_url,
            &self.config.token,
            &spec,
            notification,
            &notification.query_id,
            self.descriptor.id().as_str(),
        )?
        .context("HTTP output template rejected the request")?;
        ensure!(
            request
                .headers
                .get("x-query-sequence")
                .and_then(|value| value.to_str().ok())
                == Some(notification.query_id.as_str()),
            "rendered query identity header mismatch"
        );
        Ok(request)
    }

    async fn send_batch(
        &self,
        query: &str,
        sequence: u64,
        items: Vec<serde_json::Value>,
    ) -> Result<()> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &self.config.headers {
            headers.insert(name.parse::<reqwest::header::HeaderName>()?, value.parse()?);
        }
        if let Some(token) = &self.config.token {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {token}").parse()?,
            );
        }
        headers.insert("x-query-sequence", query.parse()?);
        let request = RenderedRequest {
            method: reqwest::Method::POST,
            url: format!(
                "{}{}",
                self.output.base_url,
                self.config
                    .batch_endpoint
                    .as_deref()
                    .context("batchEndpoint required")?
            ),
            headers,
            body: serde_json::to_string(&BatchEnvelope { batch: items })?,
        };
        self.config.failure_policy.delivery(
            self.deliver(&request, query, sequence).await,
            self.descriptor.id().as_str(),
            query,
            sequence,
        )
    }

    async fn process(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        ensure!(self.client.is_some(), "HTTP sink has not started");
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
        let max_items = self
            .adaptive
            .as_ref()
            .map(|policy| policy.current().max_operations);
        let mut batch = Vec::new();
        let mut batch_bytes = 12usize;
        let mut batch_query = String::new();
        let mut batch_sequence = 0;
        let mut count = 0;
        for result in results {
            for diff in &result.results {
                let Some(notification) = DefaultChangeNotification::from_diff(&result, diff) else {
                    continue;
                };
                count += 1;
                if let Some(max_items) = max_items {
                    let item = render_batch_item(
                        &self.renderer,
                        &self.output,
                        &notification,
                        self.descriptor.id().as_str(),
                    );
                    let bytes = serde_json::to_vec(&item)?.len() + 1;
                    if bytes + 12 > self.config.max_request_bytes {
                        self.config.failure_policy.delivery(
                            Err(anyhow::anyhow!("HTTP batch item exceeds maxRequestBytes")),
                            self.descriptor.id().as_str(),
                            &notification.query_id,
                            notification.sequence_id,
                        )?;
                        continue;
                    }
                    if !batch.is_empty()
                        && (batch_query != notification.query_id
                            || batch.len() >= max_items
                            || batch_bytes + bytes > self.config.max_request_bytes)
                    {
                        self.send_batch(&batch_query, batch_sequence, std::mem::take(&mut batch))
                            .await?;
                        batch_bytes = 12;
                    }
                    batch_query = notification.query_id;
                    batch_sequence = notification.sequence_id;
                    batch_bytes += bytes;
                    batch.push(item);
                } else {
                    let delivery = match self.render(&notification) {
                        Ok(request) => {
                            self.deliver(&request, &notification.query_id, notification.sequence_id)
                                .await
                        }
                        Err(error) => Err(error),
                    };
                    self.config.failure_policy.delivery(
                        delivery,
                        self.descriptor.id().as_str(),
                        &notification.query_id,
                        notification.sequence_id,
                    )?;
                }
            }
        }
        if !batch.is_empty() {
            self.send_batch(&batch_query, batch_sequence, batch).await?;
        }
        if let Some(adaptive) = &mut self.adaptive {
            adaptive.record(count);
        }
        Ok(())
    }
}

#[async_trait]
impl NativeComponent for HttpSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    async fn start(&mut self) -> Result<()> {
        ensure!(self.client.is_none(), "HTTP sink already started");
        self.client = Some(
            reqwest::Client::builder()
                .timeout(duration(self.config.timeout_ms, "timeoutMs")?)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        );
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.client = None;
        Ok(())
    }
}

#[async_trait]
impl NativeSink for HttpSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn batching(&self) -> Result<Option<SinkBatching>> {
        Ok(self.adaptive.as_ref().map(AdaptivePolicy::current))
    }
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        self.batching()?
            .context("HTTP adaptive batching is disabled")?
            .validate_inputs(&inputs)?;
        self.process(inputs).await
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.process(vec![input]).await
    }
}
