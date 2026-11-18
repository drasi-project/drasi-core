// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! `drasi.network/http-delivery`: an external consumer that sends each input
//! batch once to a confirming endpoint through the host `DeliveryRunner`.
//! An operation completes only when the endpoint answers exactly `200` with
//! the batch's own `DeliveryBatchIdentity`.

use crate::{
    config::{check_url, duration, SecretString},
    delivery::{HttpDeliveryHandler, MAX_MESSAGE_BYTES, MAX_TIMEOUT},
    transport::{build_client, ClientSettings},
};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, DeliveryHandler, DeliveryItem, EnvelopeCodec, GraphChangeCodec,
    PermanentFailurePolicy,
};
use drasi_computation_network_common::{
    config::ClientTls,
    update::{ensure_same_component, BoundaryPlanner},
};
use drasi_computation_plugin_sdk::lifecycle::{
    nothing_to_do, ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext,
    LifecycleError, LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext,
    UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, NativeComponent, NativeManagement, UpdateRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{num::NonZeroUsize, time::Duration};

/// Default deadline for one delivery attempt.
pub const DEFAULT_DELIVERY_TIMEOUT_MS: u64 = 30_000;

fn timeout_ms() -> u64 {
    DEFAULT_DELIVERY_TIMEOUT_MS
}
fn connect_timeout_ms() -> u64 {
    crate::config::DEFAULT_CONNECT_TIMEOUT_MS
}
fn max_message_bytes() -> usize {
    MAX_MESSAGE_BYTES
}

/// Settings of the batch-confirming delivery consumer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpDeliveryConfig {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<SecretString>,
    #[serde(default = "timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "max_message_bytes")]
    pub max_message_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_certificate_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_private_key_pem: Option<SecretString>,
    /// `stall` (default) or `skip`: a batch the endpoint rejects permanently
    /// is recorded as completed in the delivery ledger and counted as skipped.
    #[serde(default)]
    pub on_permanent_failure: PermanentFailurePolicy,
}

impl HttpDeliveryConfig {
    /// Field descriptions, defaults and ranges published to hosts.
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        // Constant declarations; `metadata_is_valid` proves they are accepted.
        ConfigDefinition::try_new([
            Field::new("url", String, true, "Absolute http/https URL of the confirming delivery endpoint, for example https://receiver.example/drasi/delivery/v1.").default_description("Required; no default."),
            Field::new("token", String, false, "Bearer token sent as Authorization: Bearer <token>.").secret(),
            Field::new("timeoutMs", Integer, false, "Deadline for one delivery attempt, including reading the confirmation.").default(timeout_ms()).minimum(1).maximum(MAX_TIMEOUT.as_millis() as u64),
            Field::new("connectTimeoutMs", Integer, false, "Deadline for establishing a TCP and TLS connection.").default(connect_timeout_ms()).minimum(1),
            Field::new("maxMessageBytes", Integer, false, "Maximum serialized delivery request.").default(max_message_bytes()).minimum(1).maximum(MAX_MESSAGE_BYTES as u64),
            Field::new("tlsCaPem", String, false, "PEM CA certificates trusted in addition to the built-in roots."),
            Field::new("tlsClientCertificatePem", String, false, "PEM client certificate chain for mutual TLS; requires tlsClientPrivateKeyPem."),
            Field::new("tlsClientPrivateKeyPem", String, false, "PEM private key for tlsClientCertificatePem.").secret(),
            Field::on_permanent_failure(),
        ])
        .expect("constant HTTP delivery configuration")
    }

    pub fn parse(value: Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value)?;
        check_url(&config.url, "url")?;
        ensure!(
            (1..=MAX_TIMEOUT.as_millis() as u64).contains(&config.timeout_ms),
            "timeoutMs must be in 1..=60000"
        );
        duration(config.connect_timeout_ms, "connectTimeoutMs")?;
        ensure!(
            (1..=MAX_MESSAGE_BYTES).contains(&config.max_message_bytes),
            "maxMessageBytes must be in 1..={MAX_MESSAGE_BYTES}"
        );
        if let Some(token) = &config.token {
            ensure!(
                !token.expose().is_empty()
                    && reqwest::header::HeaderValue::from_str(&format!(
                        "Bearer {}",
                        token.expose()
                    ))
                    .is_ok(),
                "token must be a nonempty valid header value"
            );
        }
        config.client_tls().validate()?;
        Ok(config)
    }

    pub fn client_tls(&self) -> ClientTls {
        ClientTls {
            ca_pem: self.tls_ca_pem.clone(),
            certificate_pem: self.tls_client_certificate_pem.clone(),
            private_key_pem: self.tls_client_private_key_pem.clone(),
        }
    }

    fn message_bytes(&self) -> Result<NonZeroUsize> {
        NonZeroUsize::new(self.max_message_bytes).context("maxMessageBytes must be positive")
    }
}

/// Pause (in place): token, timeout and message limit. Stop (restart): URL,
/// TLS and connect timeout.
pub(crate) fn plan(current: &HttpDeliveryConfig, proposed: &HttpDeliveryConfig) -> Assessment {
    use UpdateBoundary::*;
    let mut planner = BoundaryPlanner::default();
    let (c, p) = (current, proposed);
    planner.field("url", c.url != p.url, Stop);
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
        "onPermanentFailure",
        c.on_permanent_failure != p.on_permanent_failure,
        Replace,
    );
    planner.field("token", c.token != p.token, Pause);
    planner.field("timeoutMs", c.timeout_ms != p.timeout_ms, Pause);
    planner.field(
        "maxMessageBytes",
        c.max_message_bytes != p.max_message_bytes,
        Pause,
    );
    planner.assessment()
}

pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure_same_component(request)?;
    let current = HttpDeliveryConfig::parse(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = HttpDeliveryConfig::parse(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    Ok(plan(&current, &proposed))
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

/// The consumer object. It owns a client only while started.
pub struct HttpDeliveryConsumer {
    descriptor: ComponentDescriptor,
    config: HttpDeliveryConfig,
    handler: Option<HttpDeliveryHandler>,
    paused: bool,
}

impl HttpDeliveryConsumer {
    pub fn new(descriptor: ComponentDescriptor, config: HttpDeliveryConfig) -> Result<Self> {
        Ok(Self {
            descriptor,
            config,
            handler: None,
            paused: false,
        })
    }

    fn codec() -> Result<EnvelopeCodec> {
        let mut codec = EnvelopeCodec::new(
            NonZeroUsize::new(drasi_computation_plugin_sdk::abi::MAX_MESSAGE_BYTES)
                .context("message bound")?,
        );
        codec.register_schema(GraphChangeCodec::schema())?;
        Ok(codec)
    }

    fn handler(&self) -> Result<HttpDeliveryHandler> {
        let tls = self.config.client_tls();
        let client = build_client(&ClientSettings {
            connect_timeout: duration(self.config.connect_timeout_ms, "connectTimeoutMs")?,
            pool_idle_timeout: Duration::from_millis(crate::config::DEFAULT_POOL_IDLE_TIMEOUT_MS),
            pool_max_idle_per_host: crate::config::DEFAULT_POOL_MAX_IDLE_PER_HOST,
            tls: &tls,
        })?;
        let mut handler = HttpDeliveryHandler::new(
            client,
            check_url(&self.config.url, "url")?,
            Self::codec()?,
            self.config.message_bytes()?,
            Duration::from_millis(self.config.timeout_ms),
        )?
        .with_permanent_failure(self.config.on_permanent_failure);
        handler.set_options(
            self.config.message_bytes()?,
            Duration::from_millis(self.config.timeout_ms),
            self.config.token.as_ref(),
        )?;
        Ok(handler)
    }
}

#[async_trait]
impl NativeComponent for HttpDeliveryConsumer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }
    /// Builds the client and handler. A pre-cancelled Start builds nothing.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.handler.is_some() {
            return Err(anyhow::anyhow!("HTTP delivery consumer already started").into());
        }
        self.handler = Some(self.handler()?);
        self.paused = false;
        Ok(())
    }
    /// Host-driven: the runner calls no handler while paused (D-6).
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        self.paused = true;
        Ok(())
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.paused = false;
        Ok(())
    }
    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        self.handler = None;
        self.paused = false;
        Ok(())
    }
    /// Delivery progress belongs to the host runner; the consumer owns no
    /// external state, so `Erase` is an idempotent no-op.
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        self.handler = None;
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for HttpDeliveryConsumer {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        assess(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current = HttpDeliveryConfig::parse(update.request.current.configuration.clone())
            .map_err(refused)?;
        let proposed = HttpDeliveryConfig::parse(update.request.proposed.configuration.clone())
            .map_err(refused)?;
        if current != self.config {
            return Err(refused(
                "the consumer configuration changed after the update was assessed",
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
        if boundary < required {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: format!("the update requires the {required:?} boundary"),
            });
        }
        match boundary {
            UpdateBoundary::Pause => {
                if self.handler.is_some() && !self.paused {
                    return Err(refused("in-place updates require a completed pause"));
                }
                if let Some(handler) = &mut self.handler {
                    handler
                        .set_options(
                            proposed.message_bytes().map_err(refused)?,
                            Duration::from_millis(proposed.timeout_ms),
                            proposed.token.as_ref(),
                        )
                        .map_err(refused)?;
                }
            }
            UpdateBoundary::Stop if self.handler.is_some() => {
                return Err(refused("restart updates require a stopped consumer"));
            }
            UpdateBoundary::Stop => {}
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        self.config = proposed;
        Ok(())
    }
}

#[async_trait]
impl DeliveryHandler for HttpDeliveryConsumer {
    async fn handle(&mut self, item: DeliveryItem<'_>) -> Result<()> {
        ensure!(
            item.port.as_str() == "in",
            "unknown HTTP delivery input port"
        );
        self.handler
            .as_mut()
            .context("HTTP delivery consumer is not running")?
            .handle(item)
            .await
    }
}
