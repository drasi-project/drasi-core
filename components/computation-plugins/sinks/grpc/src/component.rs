// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The gRPC sink component: lifecycle, management and delivery.

use crate::{
    config::{ascii_metadata, GrpcReactionConfig, GrpcSinkConfig, OutputFormat},
    proto::reaction,
    render::Renderer,
    transport::{self, ChangeClient, Failure},
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeNotification, ComponentDescriptor, DeliveryFailure, InputEnvelope,
    PermanentFailurePolicy, SinkBatching, SinkCompletion, SinkOutputs,
};
use drasi_computation_network_common::{
    batching::AdaptivePolicy,
    config::{delivery_id, sanitize, COMPONENT_HEADER, DELIVERY_ID_HEADER, PORT_HEADER},
    diag,
    retry::{InputOutcome, Retry, RetryPolicy},
    update::{ensure_same_component, inputs_changed, BoundaryPlanner},
    wire,
};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleError,
    LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext, UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, NativeComponent, NativeManagement, NativeSink, UpdateRequest,
};
use drasi_grpc_common::{
    convert_json_to_proto_struct,
    output::changes::{
        build_proto_change, change_wire_size, latest_change_timestamp, request_overhead,
        ChangeBatchKey,
    },
};
use std::{collections::BTreeMap, time::Duration};
use tonic::{
    metadata::{Ascii, MetadataKey, MetadataMap, MetadataValue},
    transport::Endpoint,
    Request,
};

/// Settings read per delivery. Rebuilt as a whole at an in-place update, so a
/// delivery never sees a mix of old and new settings.
struct Live {
    /// Change projection without templates, in the configured output format.
    projection: GrpcReactionConfig,
    /// Change projection for a templated body: the canonical payload is
    /// replaced, so it is not built.
    templated: GrpcReactionConfig,
    renderer: Option<Renderer>,
    adaptive: Option<AdaptivePolicy>,
    fixed_batch_size: usize,
    fixed_flush_ms: u64,
    max_batch_bytes: usize,
    max_request_bytes: usize,
    /// Lowercase configured metadata.
    metadata: BTreeMap<String, String>,
    /// Secret metadata, sent only as headers.
    secret: MetadataMap,
    timeout: Duration,
    retry: RetryPolicy,
    on_permanent_failure: PermanentFailurePolicy,
}

impl Live {
    fn new(config: &GrpcSinkConfig) -> Result<Self> {
        let projection = GrpcReactionConfig {
            output_templates: None,
            ..config.reaction_config()
        };
        let templated = GrpcReactionConfig {
            output_format: OutputFormat::Proto,
            ..projection.clone()
        };
        let adaptive = config
            .adaptive
            .as_ref()
            .map(|adaptive| AdaptivePolicy::from_settings(adaptive, config.max_batch_bytes))
            .transpose()?;
        let (fixed_batch_size, fixed_flush_ms) = (config.batch_size, config.batch_flush_timeout_ms);
        let mut secret = MetadataMap::new();
        for (name, value) in config.secret_headers()? {
            let key: MetadataKey<Ascii> = name.parse()?;
            let value: MetadataValue<Ascii> = value
                .expose()
                .parse()
                .map_err(|_| anyhow::anyhow!("secretMetadata entry {name} is invalid"))?;
            secret.insert(key, value);
        }
        Ok(Self {
            projection,
            templated,
            renderer: Renderer::new(config.output_templates.as_ref())?,
            adaptive,
            fixed_batch_size,
            fixed_flush_ms,
            max_batch_bytes: config.max_batch_bytes,
            max_request_bytes: config.max_request_bytes,
            metadata: config.normalized_metadata()?,
            secret,
            timeout: Duration::from_millis(config.timeout_ms),
            retry: RetryPolicy {
                max_retries: config.max_retries,
                budget: Duration::from_millis(config.retry_budget_ms),
            },
            on_permanent_failure: config.on_permanent_failure,
        })
    }

    fn batching(&self) -> Option<SinkBatching> {
        if let Some(adaptive) = &self.adaptive {
            return Some(adaptive.current());
        }
        (self.fixed_batch_size > 1).then_some(SinkBatching {
            max_inputs: self.fixed_batch_size.min(SinkBatching::MAX_INPUTS),
            max_operations: self.fixed_batch_size,
            max_bytes: self.max_batch_bytes,
            max_delay_ms: self.fixed_flush_ms,
        })
    }

    fn item_limit(&self) -> usize {
        match &self.adaptive {
            Some(adaptive) => adaptive.current().max_operations,
            None => self.fixed_batch_size,
        }
    }
}

/// The receiver connection while the sink is started.
struct Connection {
    endpoint: Endpoint,
    client: ChangeClient,
}

/// One change ready to send.
struct Item {
    key: ChangeBatchKey,
    change: reaction::Change,
    bytes: usize,
    notification: ChangeNotification,
}

/// One request: a contiguous run of changes from one output with one set of
/// request metadata.
struct Batch {
    key: ChangeBatchKey,
    changes: Vec<reaction::Change>,
    bytes: usize,
    first: ChangeNotification,
    last: Option<ChangeNotification>,
}

impl Batch {
    fn last(&self) -> &ChangeNotification {
        self.last.as_ref().unwrap_or(&self.first)
    }
}

/// The two `/` separators of `{component}/{port}/{range}`.
const DELIVERY_ID_SEPARATORS: usize = 2;
/// The longest range `{u64}.{u64}-{u64}.{u64}`: four 20-digit numbers and
/// three punctuation bytes.
const DELIVERY_ID_RANGE_BYTES: usize = 4 * 20 + 3;
/// Upper bound on the protobuf framing of one metadata map entry (entry,
/// key and value tags and length prefixes).
const METADATA_ENTRY_FRAMING_BYTES: usize = 8;

/// Upper bound on the encoded `x-drasi-delivery-id` map entry for a key.
fn delivery_id_bytes(key: &ChangeBatchKey) -> usize {
    DELIVERY_ID_HEADER.len()
        + key.component.as_str().len()
        + key.port.as_str().len()
        + DELIVERY_ID_SEPARATORS
        + DELIVERY_ID_RANGE_BYTES
        + METADATA_ENTRY_FRAMING_BYTES
}

pub(crate) struct GrpcSink {
    descriptor: ComponentDescriptor,
    config: GrpcSinkConfig,
    outputs: SinkOutputs,
    live: Live,
    connection: Option<Connection>,
    paused: bool,
}

impl GrpcSink {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: GrpcSinkConfig,
        outputs: SinkOutputs,
    ) -> Result<Self> {
        config.validate_outputs(&outputs)?;
        transport::endpoint(&config)?;
        Ok(Self {
            live: Live::new(&config)?,
            descriptor,
            config,
            outputs,
            connection: None,
            paused: false,
        })
    }

    fn running(&self) -> bool {
        self.connection.is_some()
    }

    /// Connects during Start, retrying within the retry policy, and returns
    /// as soon as the Start operation is cancelled or its deadline passes.
    async fn connect_at_start(
        &self,
        endpoint: &Endpoint,
        ctx: &StartContext,
    ) -> LifecycleResult<tonic::transport::Channel> {
        let deadline = ctx.deadline();
        let expired = || async move {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        let mut retry = Retry::new(self.live.retry);
        loop {
            let attempt = tokio::select! {
                biased;
                _ = ctx.cancellation().cancelled() => return Err(LifecycleError::cancelled()),
                _ = expired() => return Err(LifecycleError::retryable(anyhow::anyhow!(
                    "the Start deadline passed before the gRPC receiver accepted a connection"
                ))),
                attempt = endpoint.connect() => attempt,
            };
            let error = match attempt {
                Ok(channel) => return Ok(channel),
                Err(error) => error,
            };
            let Some(delay) = retry.next_delay(None) else {
                return Err(LifecycleError::retryable(
                    anyhow::Error::from(error).context(format!(
                        "gRPC receiver connection failed after {} retries",
                        retry.attempt()
                    )),
                ));
            };
            diag!(
                Warn,
                "gRPC sink connect retry: component={} attempt={} error={}",
                self.descriptor.id(),
                retry.attempt(),
                sanitize(&format!("{error:#}"))
            );
            tokio::select! {
                biased;
                _ = ctx.cancellation().cancelled() => return Err(LifecycleError::cancelled()),
                _ = expired() => return Err(LifecycleError::retryable(anyhow::anyhow!(
                    "the Start deadline passed before the gRPC receiver accepted a connection"
                ))),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// Projects one change: rendered templates, the protobuf item and its
    /// request metadata (configured, then rendered, then the identity
    /// metadata naming the real producing output).
    fn build(
        &self,
        notification: &ChangeNotification,
    ) -> Result<(ChangeBatchKey, reaction::Change)> {
        let rendered = match &self.live.renderer {
            Some(renderer) => renderer.render(notification)?,
            None => Default::default(),
        };
        let projection = if rendered.body.is_some() {
            &self.live.templated
        } else {
            &self.live.projection
        };
        let mut change = build_proto_change(projection, None, notification)?.change;
        if let Some(body) = &rendered.body {
            change.payload = Some(convert_json_to_proto_struct(body));
        }
        let mut metadata = self.live.metadata.clone();
        for (name, value) in rendered.metadata {
            metadata.insert(name.to_ascii_lowercase(), value);
        }
        metadata.insert(
            COMPONENT_HEADER.into(),
            notification.output().component.as_str().to_owned(),
        );
        metadata.insert(
            PORT_HEADER.into(),
            notification.output().port.as_str().to_owned(),
        );
        for (name, value) in &metadata {
            ascii_metadata(name, value).with_context(|| {
                format!("request metadata {name} is not valid ASCII gRPC metadata")
            })?;
        }
        Ok((
            ChangeBatchKey::new(notification, metadata.into_iter().collect()),
            change,
        ))
    }

    async fn process(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        ensure!(self.running(), "gRPC sink is not started");
        let notifications = inputs
            .iter()
            .map(|input| wire::notifications(input, &self.outputs))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten();
        // Project every change before sending anything, so a template or
        // size failure under `stall` has no remote effect.
        let id = self.descriptor.id().clone();
        let mut outcome = InputOutcome::new(self.live.on_permanent_failure, &id);
        let mut items = Vec::new();
        for notification in notifications {
            // The same change projects the same way every time.
            let built = self
                .build(&notification)
                .and_then(|(key, change)| {
                    let bytes = change_wire_size(&change);
                    ensure!(
                        request_overhead(&key) + delivery_id_bytes(&key) + bytes
                            <= self.live.max_request_bytes,
                        "gRPC change exceeds maxRequestBytes"
                    );
                    Ok((key, change, bytes))
                })
                .map_err(|error| anyhow::Error::from(DeliveryFailure::permanent(error)));
            match built {
                Ok((key, change, bytes)) => items.push(Item {
                    key,
                    change,
                    bytes,
                    notification,
                }),
                Err(error) => outcome.record(
                    Err(error),
                    &notification.output().endpoint().to_string(),
                    notification.sequence(),
                )?,
            }
        }
        let count = items.len();
        let limit = self.live.item_limit();
        let mut pending: Option<Batch> = None;
        for item in items {
            if let Some(batch) = pending.take() {
                if batch.key == item.key
                    && batch.changes.len() < limit
                    && batch.bytes + item.bytes <= self.live.max_request_bytes
                {
                    pending = Some(Batch {
                        bytes: batch.bytes + item.bytes,
                        changes: {
                            let mut changes = batch.changes;
                            changes.push(item.change);
                            changes
                        },
                        last: Some(item.notification),
                        ..batch
                    });
                    continue;
                }
                self.flush(&mut outcome, batch).await?;
            }
            pending = Some(Batch {
                bytes: request_overhead(&item.key) + delivery_id_bytes(&item.key) + item.bytes,
                key: item.key,
                changes: vec![item.change],
                first: item.notification,
                last: None,
            });
        }
        if let Some(batch) = pending {
            self.flush(&mut outcome, batch).await?;
        }
        if let Some(adaptive) = &mut self.live.adaptive {
            adaptive.record(count);
        }
        outcome.finish()
    }

    async fn flush(&mut self, outcome: &mut InputOutcome<'_>, batch: Batch) -> Result<()> {
        let label = batch.key.endpoint();
        let sequence = batch.last().sequence();
        let result = self.deliver(batch).await;
        outcome.record(result, &label, sequence)
    }

    /// Sends one request until it is acknowledged exactly, fails permanently,
    /// or the retry policy is exhausted. The loop keeps no state outside this
    /// future, so the host cancelling the delivery drops it at once.
    async fn deliver(&mut self, batch: Batch) -> Result<()> {
        let id = delivery_id(&batch.first, batch.last());
        let mut metadata = batch.key.metadata_map();
        metadata.insert(DELIVERY_ID_HEADER.into(), id);
        let mut headers = self.live.secret.clone();
        for (name, value) in &metadata {
            let key: MetadataKey<Ascii> = name.parse()?;
            headers.insert(key, value.parse()?);
        }
        let expected = u32::try_from(batch.changes.len())?;
        let message = reaction::ProcessChangesRequest {
            changes: Some(reaction::ChangeBatch {
                output: Some(batch.key.output_identity()),
                timestamp: latest_change_timestamp(&batch.changes),
                changes: batch.changes,
            }),
            metadata,
        };
        let mut retry = Retry::new(self.live.retry);
        loop {
            let connection = self
                .connection
                .as_mut()
                .context("gRPC sink is not started")?;
            let mut request = Request::from_parts(
                headers.clone(),
                tonic::Extensions::default(),
                message.clone(),
            );
            request.set_timeout(self.live.timeout);
            let status = match connection.client.process_changes(request).await {
                Ok(response) => {
                    let response = response.into_inner();
                    // The receiver protocol makes both answers final for this request.
                    if !response.success {
                        return Err(DeliveryFailure::permanent(anyhow::anyhow!(
                            "gRPC receiver rejected the delivery (success=false; not retried): {}",
                            sanitize(&response.error)
                        ))
                        .into());
                    }
                    if response.items_processed != expected {
                        return Err(DeliveryFailure::permanent(anyhow::anyhow!(
                            "gRPC receiver acknowledged {} of {expected} changes; the request is incomplete (not retried)",
                            response.items_processed
                        ))
                        .into());
                    }
                    return Ok(());
                }
                Err(status) => status,
            };
            let error = anyhow::anyhow!(
                "gRPC ProcessChanges failed with {:?}: {}",
                status.code(),
                sanitize(status.message())
            );
            let (reconnect, requested) = match transport::classify(&status) {
                Failure::Permanent => {
                    return Err(anyhow::Error::from(DeliveryFailure::permanent(error))
                        .context("permanent gRPC failure; not retried"))
                }
                Failure::Retryable { reconnect, delay } => (reconnect, delay),
            };
            if reconnect {
                connection.client =
                    transport::client(connection.endpoint.connect_lazy(), &self.config);
            }
            let Some(delay) = retry.next_delay(requested) else {
                // The graph retries the input in place, no sooner than asked.
                let failure = match requested {
                    Some(delay) => DeliveryFailure::transient_after(error, delay),
                    None => DeliveryFailure::transient(error),
                };
                return Err(anyhow::Error::from(failure).context(format!(
                    "gRPC delivery failed after {} retries",
                    retry.attempt()
                )));
            };
            diag!(
                Warn,
                "gRPC sink retry: component={} output={} attempt={} error={error:#}",
                self.descriptor.id(),
                sanitize(&batch.key.endpoint()),
                retry.attempt()
            );
            tokio::time::sleep(delay).await;
        }
    }
}

/// Classifies each configuration change by the weakest safe boundary.
///
/// * Pause (in place): everything read per delivery: templates, metadata,
///   secret metadata, the call deadline, retries, the retry budget,
///   batching, the output format and `requireConnectedAtStart` (read only by
///   the next Start).
/// * Stop (restart): settings bound into the channel at Start: endpoint, TLS,
///   connect timeout, keepalive, compression and `maxRequestBytes`.
/// * Replace: output aliases and `onPermanentFailure` (part of the
///   description captured at construction).
pub(crate) fn plan(current: &GrpcSinkConfig, proposed: &GrpcSinkConfig) -> Assessment {
    use UpdateBoundary::*;
    let mut planner = BoundaryPlanner::default();
    let (c, p) = (current, proposed);
    planner.field("outputs", c.outputs != p.outputs, Replace);
    planner.field("endpoint", c.endpoint != p.endpoint, Stop);
    planner.field(
        "tls",
        c.tls_ca_pem != p.tls_ca_pem
            || c.tls_client_certificate_pem != p.tls_client_certificate_pem
            || c.tls_client_private_key_pem != p.tls_client_private_key_pem
            || c.tls_domain_name != p.tls_domain_name,
        Stop,
    );
    planner.field(
        "connectTimeoutMs",
        c.connect_timeout_ms != p.connect_timeout_ms,
        Stop,
    );
    planner.field(
        "keepalive",
        c.keepalive_interval_ms != p.keepalive_interval_ms
            || c.keepalive_timeout_ms != p.keepalive_timeout_ms,
        Stop,
    );
    planner.field("compression", c.compression != p.compression, Stop);
    planner.field(
        "maxRequestBytes",
        c.max_request_bytes != p.max_request_bytes,
        Stop,
    );
    planner.field("metadata", c.metadata != p.metadata, Pause);
    planner.field(
        "secretMetadata",
        c.secret_metadata != p.secret_metadata,
        Pause,
    );
    planner.field("timeoutMs", c.timeout_ms != p.timeout_ms, Pause);
    planner.field(
        "retries",
        c.max_retries != p.max_retries || c.retry_budget_ms != p.retry_budget_ms,
        Pause,
    );
    // The policy is part of the description captured at construction.
    planner.field(
        "onPermanentFailure",
        c.on_permanent_failure != p.on_permanent_failure,
        Replace,
    );
    planner.field(
        "requireConnectedAtStart",
        c.require_connected_at_start != p.require_connected_at_start,
        Pause,
    );
    planner.field(
        "batching",
        c.adaptive != p.adaptive
            || c.batch_size != p.batch_size
            || c.batch_flush_timeout_ms != p.batch_flush_timeout_ms
            || c.max_batch_bytes != p.max_batch_bytes,
        Pause,
    );
    planner.field(
        "outputTemplates",
        c.output_templates != p.output_templates,
        Pause,
    );
    planner.field("outputFormat", c.output_format != p.output_format, Pause);
    planner.assessment()
}

/// Pure planning used by the factory and the live instance. A change to the
/// bound inputs replaces the component.
pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure_same_component(request)?;
    let current = GrpcSinkConfig::parse(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = GrpcSinkConfig::parse(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    if !request.proposed.inputs.is_empty() {
        let outputs = SinkOutputs::from_inputs(&request.proposed.inputs, &proposed.outputs)?;
        proposed.validate_outputs(&outputs)?;
    }
    if inputs_changed(request) {
        return Ok(Assessment::supported(
            UpdateBoundary::Replace,
            drasi_computation_contracts::ComponentUpdateData::Preserve,
        ));
    }
    Ok(plan(&current, &proposed))
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

#[async_trait]
impl NativeComponent for GrpcSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.config.to_value()
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    /// Templates, metadata, TLS material and routes were compiled and
    /// validated at construction; nothing is contacted.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        Ok(())
    }
    /// Builds the channel. By default the receiver is not contacted: the
    /// first delivery connects. With `requireConnectedAtStart`, connects
    /// within the retry policy, the Start deadline and its cancellation.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.running() {
            return Err(anyhow::anyhow!("gRPC sink is already started").into());
        }
        let endpoint = transport::endpoint(&self.config)?;
        let channel = if self.config.require_connected_at_start {
            self.connect_at_start(&endpoint, ctx).await?
        } else {
            endpoint.connect_lazy()
        };
        self.connection = Some(Connection {
            client: transport::client(channel, &self.config),
            endpoint,
        });
        self.paused = false;
        Ok(())
    }
    /// Host-driven: the sink owns no worker. The host calls no `handle`
    /// while paused and settles the in-flight one (whose retries are bounded
    /// by `retryBudgetMs`) before Pause completes.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        self.paused = self.running();
        Ok(())
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.paused = false;
        Ok(())
    }
    /// Closes the channel. No delivery runs concurrently with Stop: the host
    /// awaits (Drain) or drops (Abort) the in-flight one first, so both modes
    /// complete at once. Stop before Start succeeds.
    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        self.connection = None;
        self.paused = false;
        Ok(())
    }
    /// The sink owns no destination-side state; `Preserve` and `Erase` are
    /// idempotent no-ops.
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        self.connection = None;
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for GrpcSink {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        assess(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current =
            GrpcSinkConfig::parse(update.request.current.configuration.clone()).map_err(refused)?;
        let proposed = GrpcSinkConfig::parse(update.request.proposed.configuration.clone())
            .map_err(refused)?;
        if current != self.config {
            return Err(refused(
                "the sink configuration changed after the update was assessed",
            ));
        }
        let Some(plan) = assess(&update.request).map_err(refused)?.plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if self.running() && !self.paused {
                    return Err(refused("in-place updates require a completed pause"));
                }
            }
            UpdateBoundary::Stop => {
                if self.running() {
                    return Err(refused("restart updates require a stopped sink"));
                }
                transport::endpoint(&proposed).map_err(refused)?;
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        proposed.validate_outputs(&self.outputs).map_err(refused)?;
        let live = Live::new(&proposed).map_err(refused)?;
        self.live = live;
        self.config = proposed;
        Ok(())
    }
}

#[async_trait]
impl NativeSink for GrpcSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn batching(&self) -> Result<Option<SinkBatching>> {
        Ok(self.live.batching())
    }
    /// Accepts any nonempty host batch. Requests are split by the current
    /// item and byte limits, so a batch collected under an earlier policy
    /// (before an in-place update) is still delivered.
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        ensure!(!inputs.is_empty(), "empty gRPC sink batch");
        self.process(inputs).await
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.process(vec![input]).await
    }
}
