// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The HTTP sink component: lifecycle, management, rendering and delivery.

use crate::{
    config::{duration, HttpSinkConfig, UnroutedChanges},
    render::Renderer,
    transport::{self, build_client, Attempts, ClientSettings, Request},
    wire,
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_contracts::{
    ChangeNotification, ChangeRoute, ComponentDescriptor, DeliveryFailure, InputEnvelope,
    SinkBatching, SinkCompletion, SinkOutput, SinkOutputs,
};
use drasi_computation_network_common::{
    batching::AdaptivePolicy,
    config::{delivery_id, COMPONENT_HEADER, PORT_HEADER},
    retry::InputOutcome,
    update::{ensure_same_component, inputs_changed, BoundaryPlanner},
};
use drasi_computation_plugin_sdk::lifecycle::{
    nothing_to_do, ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext,
    LifecycleError, LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext,
    UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, NativeComponent, NativeManagement, NativeSink, UpdateRequest,
};
use drasi_computation_template_common::change_context;
use drasi_http_common::reaction::{
    config::{
        parse_http_method, resolve_http_url, HttpCallSpec, HttpReactionConfig, OperationType,
        TemplateRouting,
    },
    output::BatchEnvelope,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use std::time::Duration;

/// Serialized size of an empty batch envelope, `{"batch":[]}`. Each item adds
/// its own bytes plus one separator.
pub(crate) const BATCH_ENVELOPE_OVERHEAD: usize = r#"{"batch":[]}"#.len();

/// State read per delivery; replaced atomically by an in-place update.
struct Live {
    routing: HttpReactionConfig,
    renderer: Renderer,
    /// Content type, configured and secret headers, and authorization.
    headers: HeaderMap,
}

impl Live {
    fn new(config: &HttpSinkConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        for (name, value) in &config.headers {
            headers.insert(name.parse::<HeaderName>()?, value.parse()?);
        }
        for (name, value) in &config.secret_headers {
            let mut value = HeaderValue::from_str(value.expose())?;
            value.set_sensitive(true);
            headers.insert(name.parse::<HeaderName>()?, value);
        }
        if let Some(token) = &config.token {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", token.expose()))?;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        Ok(Self {
            routing: config.reaction_config()?,
            renderer: Renderer::new(config.output_templates.as_ref())?,
            headers,
        })
    }
}

fn adaptive_policy(config: &HttpSinkConfig) -> Result<Option<AdaptivePolicy>> {
    config
        .adaptive
        .as_ref()
        .map(|adaptive| AdaptivePolicy::from_settings(adaptive, config.max_batch_bytes))
        .transpose()
}

pub(crate) struct HttpSink {
    descriptor: ComponentDescriptor,
    config: HttpSinkConfig,
    outputs: SinkOutputs,
    live: Live,
    adaptive: Option<AdaptivePolicy>,
    client: Option<reqwest::Client>,
    paused: bool,
}

impl HttpSink {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: HttpSinkConfig,
        outputs: SinkOutputs,
    ) -> Result<Self> {
        config.validate_outputs(&outputs)?;
        Ok(Self {
            descriptor,
            live: Live::new(&config)?,
            adaptive: adaptive_policy(&config)?,
            config,
            outputs,
            client: None,
            paused: false,
        })
    }

    fn client(&self) -> Result<&reqwest::Client> {
        self.client.as_ref().context("HTTP sink is not running")
    }

    fn attempts(&self) -> Attempts<'_> {
        Attempts {
            timeout: Duration::from_millis(self.config.timeout_ms),
            retry: self.config.retry_policy(),
            success: &self.config.success_status_codes,
        }
    }

    fn spec(&self, notification: &ChangeNotification) -> Option<&HttpCallSpec> {
        let operation = match notification.change().route() {
            ChangeRoute::Added => OperationType::Add,
            ChangeRoute::Updated => OperationType::Update,
            ChangeRoute::Deleted => OperationType::Delete,
        };
        self.live
            .routing
            .get_template_spec(notification.alias(), operation)
    }

    fn default_url(&self, alias: &str) -> String {
        if self.config.url.is_empty() {
            format!("{}/changes/{alias}", self.live.routing.base_url)
        } else {
            self.config.url.clone()
        }
    }

    /// Renders one change's request. Any failure (template, rendered URL or
    /// header, size) is a per-change error for the failure policy; nothing
    /// falls back to a default.
    fn render(&self, notification: &ChangeNotification) -> Result<Request> {
        let context = change_context(notification)?;
        let spec = self.spec(notification);
        let url = match spec.map(|spec| spec.extension.url.as_str()) {
            Some(url) if !url.is_empty() => {
                let rendered = self.live.renderer.render(url, &context)?;
                resolve_http_url(&self.live.routing.base_url, &rendered)
                    .context("rendered URL rejected")?
            }
            _ => self.default_url(notification.alias()),
        };
        let method = match spec {
            Some(spec) => parse_http_method(&spec.extension.method)?,
            None => reqwest::Method::POST,
        };
        let body = match spec.map(|spec| spec.template.as_str()) {
            Some(template) if !template.is_empty() => {
                self.live.renderer.render(template, &context)?
            }
            _ => serde_json::to_string(&context)?,
        };
        ensure!(
            body.len() <= self.config.max_request_bytes,
            "HTTP body exceeds maxRequestBytes"
        );
        let mut headers = self.live.headers.clone();
        for (name, template) in spec.iter().flat_map(|spec| &spec.extension.headers) {
            let value = self.live.renderer.render(template, &context)?;
            headers.insert(
                name.parse::<HeaderName>()?,
                HeaderValue::from_str(&value)
                    .with_context(|| format!("template header {name} rendered an invalid value"))?,
            );
        }
        insert_output_headers(&mut headers, notification.output())?;
        Ok(Request {
            method,
            url,
            headers,
            body: Bytes::from(body),
        })
    }

    /// One batch item: the route's body template, which must render JSON, or
    /// the default change notification when no body template applies.
    fn batch_item(&self, notification: &ChangeNotification) -> Result<serde_json::Value> {
        let context = change_context(notification)?;
        match self
            .spec(notification)
            .map(|spec| spec.template.as_str())
            .filter(|template| !template.is_empty())
        {
            Some(template) => self
                .live
                .renderer
                .render_json(template, &context)
                .context("batch body template"),
            None => Ok(context),
        }
    }

    async fn send_batch(
        &self,
        outcome: &mut InputOutcome<'_>,
        output: &SinkOutput,
        id: &str,
        sequence: u64,
        items: Vec<serde_json::Value>,
    ) -> Result<()> {
        let mut headers = self.live.headers.clone();
        insert_output_headers(&mut headers, output)?;
        let request = Request {
            method: reqwest::Method::POST,
            url: format!(
                "{}{}",
                self.live.routing.base_url,
                self.config
                    .batch_endpoint
                    .as_deref()
                    .context("batchEndpoint required")?
            ),
            headers,
            body: Bytes::from(serde_json::to_vec(&BatchEnvelope { batch: items })?),
        };
        let label = output.endpoint().to_string();
        let result =
            transport::deliver(self.client()?, &request, &self.attempts(), id, &label).await;
        outcome.record(result, &label, sequence)
    }

    async fn process(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        self.client()?;
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
        let id = self.descriptor.id().clone();
        let mut outcome = InputOutcome::new(self.config.on_permanent_failure, &id);
        let mut batch = Vec::new();
        let mut batch_bytes = BATCH_ENVELOPE_OVERHEAD;
        let mut batch_run: Option<(usize, usize)> = None;
        for (index, notification) in notifications.iter().enumerate() {
            let output = notification.output();
            let label = output.endpoint().to_string();
            let sequence = notification.sequence();
            if self.config.unrouted_changes == UnroutedChanges::Ignore
                && self.spec(notification).is_none()
            {
                continue;
            }
            let Some(max_items) = max_items else {
                let delivery = match self.render(notification) {
                    Ok(request) => {
                        let id = delivery_id(notification, notification);
                        transport::deliver(self.client()?, &request, &self.attempts(), &id, &label)
                            .await
                    }
                    // The same change renders the same way every time.
                    Err(error) => Err(DeliveryFailure::permanent(
                        error.context("HTTP request could not be rendered"),
                    )
                    .into()),
                };
                outcome.record(delivery, &label, sequence)?;
                continue;
            };
            let item = match self.batch_item(notification) {
                Ok(item) => item,
                Err(error) => {
                    outcome.record(
                        Err(DeliveryFailure::permanent(
                            error.context("HTTP batch item could not be rendered"),
                        )
                        .into()),
                        &label,
                        sequence,
                    )?;
                    continue;
                }
            };
            let bytes = serde_json::to_vec(&item)?.len() + 1;
            if bytes + BATCH_ENVELOPE_OVERHEAD > self.config.max_request_bytes {
                outcome.record(
                    Err(DeliveryFailure::permanent(anyhow::anyhow!(
                        "HTTP batch item exceeds maxRequestBytes"
                    ))
                    .into()),
                    &label,
                    sequence,
                )?;
                continue;
            }
            if let Some((first, last)) = batch_run {
                if notifications[first].output() != output
                    || batch.len() >= max_items
                    || batch_bytes + bytes > self.config.max_request_bytes
                {
                    let id = delivery_id(&notifications[first], &notifications[last]);
                    self.send_batch(
                        &mut outcome,
                        notifications[first].output(),
                        &id,
                        notifications[last].sequence(),
                        std::mem::take(&mut batch),
                    )
                    .await?;
                    batch_bytes = BATCH_ENVELOPE_OVERHEAD;
                    batch_run = None;
                }
            }
            batch_run = Some((batch_run.map_or(index, |(first, _)| first), index));
            batch_bytes += bytes;
            batch.push(item);
        }
        if let Some((first, last)) = batch_run {
            let id = delivery_id(&notifications[first], &notifications[last]);
            self.send_batch(
                &mut outcome,
                notifications[first].output(),
                &id,
                notifications[last].sequence(),
                batch,
            )
            .await?;
        }
        if let Some(adaptive) = &mut self.adaptive {
            adaptive.record(notifications.len());
        }
        outcome.finish()
    }
}

fn insert_output_headers(headers: &mut HeaderMap, output: &SinkOutput) -> Result<()> {
    headers.insert(COMPONENT_HEADER, output.component.as_str().parse()?);
    headers.insert(PORT_HEADER, output.port.as_str().parse()?);
    Ok(())
}

/// Classifies each configuration change by the weakest safe boundary.
///
/// * Pause (in place): everything read per delivery or per host collection —
///   templates, headers, secret headers, token, timeout, request size,
///   retries, retry budget, success statuses, unrouted-change handling,
///   adaptive bounds and maxBatchBytes (the host reads `batching()` before
///   every collection).
/// * Stop (restart): destination and connection settings bound into the
///   client — url, baseUrl, batchEndpoint, TLS, connect timeout and the
///   connection pool. Enabling or disabling `adaptive` also changes
///   `batchEndpoint`, so it restarts.
/// * Replace: output aliases, changed input bindings, or
///   `onPermanentFailure` (part of the description captured at construction).
pub(crate) fn plan(
    current: &HttpSinkConfig,
    proposed: &HttpSinkConfig,
    inputs_changed: bool,
) -> Assessment {
    use UpdateBoundary::*;
    let mut planner = BoundaryPlanner::default();
    let (c, p) = (current, proposed);
    planner.field("inputs", inputs_changed, Replace);
    planner.field("outputs", c.outputs != p.outputs, Replace);
    planner.field("url", c.url != p.url, Stop);
    planner.field("baseUrl", c.base_url != p.base_url, Stop);
    planner.field("batchEndpoint", c.batch_endpoint != p.batch_endpoint, Stop);
    planner.field("adaptive", c.adaptive != p.adaptive, Pause);
    planner.field(
        "maxBatchBytes",
        c.max_batch_bytes != p.max_batch_bytes,
        Pause,
    );
    planner.field(
        "tls",
        c.tls_ca_pem != p.tls_ca_pem
            || c.tls_client_certificate_pem != p.tls_client_certificate_pem
            || c.tls_client_private_key_pem != p.tls_client_private_key_pem,
        Stop,
    );
    planner.field(
        "connectTimeoutMs",
        c.connect_timeout_ms != p.connect_timeout_ms,
        Stop,
    );
    planner.field(
        "pool",
        c.pool_idle_timeout_ms != p.pool_idle_timeout_ms
            || c.pool_max_idle_per_host != p.pool_max_idle_per_host,
        Stop,
    );
    planner.field(
        "outputTemplates",
        c.output_templates != p.output_templates,
        Pause,
    );
    planner.field("headers", c.headers != p.headers, Pause);
    planner.field("secretHeaders", c.secret_headers != p.secret_headers, Pause);
    planner.field("token", c.token != p.token, Pause);
    planner.field("timeoutMs", c.timeout_ms != p.timeout_ms, Pause);
    planner.field(
        "maxRequestBytes",
        c.max_request_bytes != p.max_request_bytes,
        Pause,
    );
    planner.field("maxRetries", c.max_retries != p.max_retries, Pause);
    planner.field(
        "retryBudgetMs",
        c.retry_budget_ms != p.retry_budget_ms,
        Pause,
    );
    // The policy is part of the description captured at construction.
    planner.field(
        "onPermanentFailure",
        c.on_permanent_failure != p.on_permanent_failure,
        Replace,
    );
    planner.field(
        "unroutedChanges",
        c.unrouted_changes != p.unrouted_changes,
        Pause,
    );
    planner.field(
        "successStatusCodes",
        c.success_status_codes != p.success_status_codes,
        Pause,
    );
    planner.assessment()
}

/// Pure planning used by the factory and the live instance. The proposed
/// configuration must be valid for the proposed incoming edges.
pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure_same_component(request)?;
    let current = HttpSinkConfig::parse(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = HttpSinkConfig::parse(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    let outputs = SinkOutputs::from_inputs(&request.proposed.inputs, &proposed.outputs)?;
    proposed.validate_outputs(&outputs)?;
    Ok(plan(&current, &proposed, inputs_changed(request)))
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
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
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    /// Templates, headers, TLS material and routes were validated at
    /// construction; nothing is bound here.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }
    /// Builds the HTTP client. A pre-cancelled Start builds nothing and leaves
    /// the sink startable.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.client.is_some() {
            return Err(anyhow::anyhow!("HTTP sink already started").into());
        }
        let tls = self.config.client_tls();
        let client = build_client(&ClientSettings {
            connect_timeout: duration(self.config.connect_timeout_ms, "connectTimeoutMs")?,
            pool_idle_timeout: duration(self.config.pool_idle_timeout_ms, "poolIdleTimeoutMs")?,
            pool_max_idle_per_host: self.config.pool_max_idle_per_host,
            tls: &tls,
        })?;
        self.client = Some(client);
        self.paused = false;
        Ok(())
    }
    /// The sink has no worker of its own: the host stops calling `handle`
    /// and waits for the in-flight one before Pause completes (D-6). Pause
    /// marks the in-place update boundary.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        self.paused = true;
        Ok(())
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.paused = false;
        Ok(())
    }
    /// Drops the client and its pooled connections. An in-flight delivery,
    /// including its retries, is owned by the host's `handle` future: Drain
    /// lets it finish within `retryBudgetMs`, Abort drops it.
    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        self.client = None;
        self.paused = false;
        Ok(())
    }
    /// The sink owns no external state, so `Erase` is an idempotent no-op
    /// (D-1). Any client left by an unfinished stop is dropped.
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        self.client = None;
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for HttpSink {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        assess(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current =
            HttpSinkConfig::parse(update.request.current.configuration.clone()).map_err(refused)?;
        let proposed = HttpSinkConfig::parse(update.request.proposed.configuration.clone())
            .map_err(refused)?;
        if current != self.config {
            return Err(refused(
                "the sink configuration changed after the update was assessed",
            ));
        }
        let assessment = assess(&update.request).map_err(refused)?;
        let Some(plan) = assessment.plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        let required = UpdateBoundary::from(plan.mode);
        let boundary = ctx.boundary();
        if required == UpdateBoundary::Replace || boundary < required {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: format!("the update requires the {required:?} boundary"),
            });
        }
        match boundary {
            UpdateBoundary::Pause if self.client.is_some() && !self.paused => {
                return Err(refused("in-place updates require a completed pause"));
            }
            UpdateBoundary::Stop if self.client.is_some() => {
                return Err(refused("restart updates require a stopped sink"));
            }
            UpdateBoundary::Replace => {
                return Err(refused("Replace is not an instance update"));
            }
            _ => {}
        }
        let live = Live::new(&proposed).map_err(refused)?;
        // A changed policy starts from the idle tier; an unchanged one keeps
        // its throughput history.
        if self.config.adaptive != proposed.adaptive
            || self.config.max_batch_bytes != proposed.max_batch_bytes
        {
            self.adaptive = adaptive_policy(&proposed).map_err(refused)?;
        }
        self.live = live;
        self.config = proposed;
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
    /// Accepts any nonempty batch within the host's input bound, even one
    /// collected under an earlier tier; `process` splits it at the current
    /// item target and `maxRequestBytes`.
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        ensure!(
            self.adaptive.is_some(),
            "HTTP adaptive batching is disabled"
        );
        ensure!(
            !inputs.is_empty() && inputs.len() <= SinkBatching::MAX_INPUTS,
            "sink batch is empty or exceeds the host input bound"
        );
        self.process(inputs).await
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.process(vec![input]).await
    }
}
