// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{
    config::{backoff, duration, HttpSinkConfig},
    wire,
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeNotification, ChangeRoute, ComponentDescriptor, InputEnvelope, SinkBatching,
    SinkCompletion, SinkOutput, SinkOutputs,
};
use drasi_computation_network_common::{
    batching::AdaptivePolicy,
    config::{COMPONENT_HEADER, PORT_HEADER},
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSink};
use drasi_http_common::{
    adaptive_batcher::AdaptiveBatchConfig,
    reaction::{
        config::{HttpCallExt, HttpCallSpec, HttpReactionConfig, OperationType, TemplateRouting},
        output::BatchEnvelope,
        process::{
            build_handlebars, is_retryable_status, render_request_with_context, RenderedRequest,
        },
    },
};
use std::time::Duration;

fn insert_output_headers(
    headers: &mut std::collections::BTreeMap<String, String>,
    output: &SinkOutput,
) {
    headers.insert(
        COMPONENT_HEADER.into(),
        output.component.as_str().to_owned(),
    );
    headers.insert(PORT_HEADER.into(), output.port.as_str().to_owned());
}

pub(crate) struct HttpSink {
    descriptor: ComponentDescriptor,
    config: HttpSinkConfig,
    outputs: SinkOutputs,
    output: HttpReactionConfig,
    renderer: handlebars::Handlebars<'static>,
    adaptive: Option<AdaptivePolicy>,
    client: Option<reqwest::Client>,
}

impl HttpSink {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: HttpSinkConfig,
        outputs: SinkOutputs,
    ) -> Result<Self> {
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
            outputs,
            client: None,
        })
    }

    async fn deliver(&self, request: &RenderedRequest, output: &str, sequence: u64) -> Result<()> {
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
            eprintln!("drasi.network HTTP retry: output={output} sequence={sequence} attempt={} error={error:#}", attempt + 1);
            backoff(attempt).await;
        }
        unreachable!("retry loop always returns")
    }

    fn spec(&self, notification: &ChangeNotification) -> Option<&HttpCallSpec> {
        let operation = match notification.change().route() {
            ChangeRoute::Added => OperationType::Add,
            ChangeRoute::Updated => OperationType::Update,
            ChangeRoute::Deleted => OperationType::Delete,
        };
        self.output
            .get_template_spec(notification.alias(), operation)
    }

    fn render(&self, notification: &ChangeNotification) -> Result<RenderedRequest> {
        let alias = notification.alias();
        let mut spec = self
            .spec(notification)
            .cloned()
            .unwrap_or_else(|| HttpCallSpec {
                template: String::new(),
                extension: HttpCallExt {
                    url: if self.config.url.is_empty() {
                        format!("/changes/{alias}")
                    } else {
                        self.config.url.clone()
                    },
                    ..Default::default()
                },
            });
        let mut headers = self.config.headers.clone();
        insert_output_headers(&mut headers, notification.output());
        let mut combined = headers
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        for (name, value) in spec.extension.headers {
            let name: reqwest::header::HeaderName = name.parse()?;
            combined.insert(name.to_string(), value);
        }
        spec.extension.headers = combined;
        // The template context and the default body are the same truthful change.
        let body = notification.to_json()?;
        let context = body.as_object().context("change notification object")?;
        let request = render_request_with_context(
            &self.renderer,
            &self.output.base_url,
            &self.config.token,
            &spec,
            context,
            || Ok(serde_json::to_string(&body)?),
            alias,
            notification.change().operation(),
            self.descriptor.id().as_str(),
        )?
        .context("HTTP output template rejected the request")?;
        for (name, expected) in [
            (COMPONENT_HEADER, notification.output().component.as_str()),
            (PORT_HEADER, notification.output().port.as_str()),
        ] {
            ensure!(
                request
                    .headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    == Some(expected),
                "rendered {name} identity header mismatch"
            );
        }
        Ok(request)
    }

    /// One batch item: the route's body template when it renders JSON, else the
    /// default change notification.
    fn batch_item(&self, notification: &ChangeNotification) -> Result<serde_json::Value> {
        let default = notification.to_json()?;
        let Some(template) = self
            .spec(notification)
            .map(|spec| spec.template.as_str())
            .filter(|template| !template.is_empty())
        else {
            return Ok(default);
        };
        match self
            .renderer
            .render_template(template, &default)
            .map_err(anyhow::Error::from)
            .and_then(|rendered| Ok(serde_json::from_str(&rendered)?))
        {
            Ok(item) => Ok(item),
            Err(error) => {
                eprintln!(
                    "drasi.network HTTP batch body template for output '{}' ({}) failed: {error:#}; using the default change notification",
                    notification.alias(),
                    notification.change().operation()
                );
                Ok(default)
            }
        }
    }

    async fn send_batch(
        &self,
        output: &SinkOutput,
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
        headers.insert(COMPONENT_HEADER, output.component.as_str().parse()?);
        headers.insert(PORT_HEADER, output.port.as_str().parse()?);
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
            self.deliver(&request, &output.endpoint().to_string(), sequence)
                .await,
            self.descriptor.id().as_str(),
            &output.endpoint().to_string(),
            sequence,
        )
    }

    async fn process(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        ensure!(self.client.is_some(), "HTTP sink has not started");
        let notifications = inputs
            .iter()
            .map(|input| wire::notifications(input, &self.outputs))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let max_items = self
            .adaptive
            .as_ref()
            .map(|policy| policy.current().max_operations);
        let mut batch = Vec::new();
        let mut batch_bytes = 12usize;
        let mut batch_output: Option<SinkOutput> = None;
        let mut batch_sequence = 0;
        let mut count = 0;
        for notification in notifications {
            count += 1;
            let output = notification.output();
            let label = output.endpoint().to_string();
            let sequence = notification.sequence();
            if let Some(max_items) = max_items {
                let item = self.batch_item(&notification)?;
                let bytes = serde_json::to_vec(&item)?.len() + 1;
                if bytes + 12 > self.config.max_request_bytes {
                    self.config.failure_policy.delivery(
                        Err(anyhow::anyhow!("HTTP batch item exceeds maxRequestBytes")),
                        self.descriptor.id().as_str(),
                        &label,
                        sequence,
                    )?;
                    continue;
                }
                if !batch.is_empty()
                    && (batch_output.as_ref() != Some(output)
                        || batch.len() >= max_items
                        || batch_bytes + bytes > self.config.max_request_bytes)
                {
                    let previous = batch_output.as_ref().context("batch output")?;
                    self.send_batch(previous, batch_sequence, std::mem::take(&mut batch))
                        .await?;
                    batch_bytes = 12;
                }
                batch_output = Some(output.clone());
                batch_sequence = sequence;
                batch_bytes += bytes;
                batch.push(item);
            } else {
                let delivery = match self.render(&notification) {
                    Ok(request) => self.deliver(&request, &label, sequence).await,
                    Err(error) => Err(error),
                };
                self.config.failure_policy.delivery(
                    delivery,
                    self.descriptor.id().as_str(),
                    &label,
                    sequence,
                )?;
            }
        }
        if !batch.is_empty() {
            let output = batch_output.as_ref().context("batch output")?;
            self.send_batch(output, batch_sequence, batch).await?;
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
