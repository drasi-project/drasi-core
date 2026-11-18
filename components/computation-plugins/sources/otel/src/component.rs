// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The OpenTelemetry source component: lifecycle, management and serving.

use crate::config::{OtelSourceConfig, Protocol};
use crate::engine::Engine;
use crate::grpc::{self, GrpcOptions};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{ComponentDescriptor, GraphChangeCodec, OutputEnvelope};
use drasi_computation_network_common::diag;
use drasi_computation_network_common::grpc_incoming::IncomingOptions;
use drasi_computation_network_common::ingress::{Ingress, SourceRuntime};
use drasi_computation_network_common::server::{self, ServerOptions};
use drasi_computation_network_common::update::{ensure_same_component, BoundaryPlanner};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleResult,
    PauseContext, ResumeContext, StartContext, StopContext, UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, NativeComponent, NativeManagement, NativeSource, Scope, UpdateRequest,
};
use std::{sync::Arc, time::Duration};
use tokio::net::TcpListener;

/// How often elapsed TTLs are swept.
pub(crate) const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) struct OtelSource {
    runtime: SourceRuntime,
    config: OtelSourceConfig,
    engine: Arc<Engine>,
}

impl OtelSource {
    pub fn new(
        descriptor: ComponentDescriptor,
        config: OtelSourceConfig,
        scope: Option<&Scope>,
    ) -> Result<Self> {
        config.validate_exposure()?;
        let engine = Arc::new(Engine::new(descriptor.id().as_str(), config.projection()));
        Ok(Self {
            runtime: SourceRuntime::new(descriptor, config.ingress()?, scope, None)?,
            config,
            engine,
        })
    }

    /// The bound listener addresses, in `protocols` order, while running.
    #[cfg(test)]
    pub fn local_addresses(&self) -> Vec<std::net::SocketAddr> {
        self.runtime.local_addresses()
    }

    #[cfg(test)]
    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }
}

/// Classifies each configuration change by the weakest safe boundary.
///
/// * Pause (in place): access control (tokens, the unauthenticated opt-in,
///   rate limits) and every projection setting except metric identity —
///   allowlists, destination attributes, span kinds, heartbeat names, log
///   filters, TTLs, caps and `rejectDerived`. They are read per request;
///   TTLs apply to elements refreshed afterwards and caps to new elements.
/// * Stop (restart): settings bound into the listeners at Start — host,
///   protocols, ports, deadlines, message size, capacities and TLS.
/// * Replace: the output stream, and `metricIdentityAttributes`, which
///   changes the identity of every Metric node. Reconstruction starts with
///   empty projection state, so no Metric from the old identity scheme
///   lingers in the source's caps.
pub(crate) fn plan(current: &OtelSourceConfig, proposed: &OtelSourceConfig) -> Assessment {
    use UpdateBoundary::*;
    let mut planner = BoundaryPlanner::default();
    let (c, p) = (current, proposed);
    planner.field("stream", c.stream != p.stream, Replace);
    planner.field(
        "metricIdentityAttributes",
        c.metric_identity_attributes != p.metric_identity_attributes,
        Replace,
    );
    planner.field("host", c.host != p.host, Stop);
    planner.field("protocols", c.protocols != p.protocols, Stop);
    planner.field("grpcPort", c.grpc_port != p.grpc_port, Stop);
    planner.field("httpPort", c.http_port != p.http_port, Stop);
    planner.field("timeoutMs", c.timeout_ms != p.timeout_ms, Stop);
    planner.field(
        "ingressCapacity",
        c.ingress_capacity != p.ingress_capacity,
        Stop,
    );
    planner.field(
        "maxMessageBytes",
        c.max_message_bytes != p.max_message_bytes,
        Stop,
    );
    planner.field(
        "maxConnections",
        c.max_connections != p.max_connections,
        Stop,
    );
    planner.field(
        "headerReadTimeoutMs",
        c.header_read_timeout_ms != p.header_read_timeout_ms,
        Stop,
    );
    planner.field(
        "tls",
        c.tls_certificate_pem != p.tls_certificate_pem
            || c.tls_private_key_pem != p.tls_private_key_pem
            || c.tls_client_ca_pem != p.tls_client_ca_pem,
        Stop,
    );
    planner.field(
        "authToken",
        c.auth_token != p.auth_token || c.auth_token_secondary != p.auth_token_secondary,
        Pause,
    );
    planner.field(
        "allowUnauthenticated",
        c.allow_unauthenticated != p.allow_unauthenticated,
        Pause,
    );
    planner.field(
        "rateLimit",
        c.rate_limit_per_second != p.rate_limit_per_second
            || c.rate_limit_burst != p.rate_limit_burst,
        Pause,
    );
    let mut current_projection = c.clone();
    current_projection.metric_identity_attributes = p.metric_identity_attributes.clone();
    planner.field(
        "projection",
        current_projection.projection() != p.projection(),
        Pause,
    );
    planner.assessment()
}

/// Pure planning used by the factory and the live instance.
pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure_same_component(request)?;
    let current = OtelSourceConfig::from_value(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = OtelSourceConfig::from_value(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    proposed.validate_exposure()?;
    Ok(plan(&current, &proposed))
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

/// Everything the serving future needs, captured at Start.
struct ServeOptions {
    protocols: Vec<Protocol>,
    grpc: GrpcOptions,
    http: ServerOptions,
    engine: Arc<Engine>,
}

#[async_trait]
impl NativeComponent for OtelSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.runtime.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.config)?)
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    /// Configuration was fully validated at construction (secrets arrive
    /// resolved); there is nothing further to check before binding.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        Ok(())
    }
    /// Binds every configured listener before returning, then serves them
    /// with the TTL sweeper as one owned worker.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        let ingress = self.config.ingress()?;
        let header_read_timeout = Duration::from_millis(self.config.header_read_timeout_ms);
        let grpc_tls = ingress
            .security
            .tls
            .as_ref()
            .map(|tls| tls.server_config(vec![b"h2".to_vec()]))
            .transpose()?;
        let http_tls = ingress
            .security
            .tls
            .as_ref()
            .map(|tls| tls.server_config(vec![b"h2".to_vec(), b"http/1.1".to_vec()]))
            .transpose()?;
        let options = ServeOptions {
            protocols: self.config.protocols.clone(),
            grpc: GrpcOptions {
                incoming: IncomingOptions {
                    tls: grpc_tls,
                    max_connections: self.config.max_connections,
                    header_read_timeout,
                },
                timeout: grpc::transport_timeout(ingress.timeout()),
            },
            http: ServerOptions {
                tls: http_tls,
                max_connections: self.config.max_connections,
                header_read_timeout,
            },
            engine: self.engine.clone(),
        };
        let ports = self.config.ports();
        let deadline = ctx.deadline();
        self.runtime
            .start_listeners(
                ctx.cancellation(),
                deadline,
                &ports,
                move |listeners, ingress| serve(listeners, ingress, options),
            )
            .await
    }
    /// Refuses new exports (gRPC `UNAVAILABLE`, HTTP 503), defers TTL expiry,
    /// and returns once in-flight admissions settle.
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult {
        self.runtime.pause(ctx.cancellation(), ctx.deadline()).await
    }
    /// Reopens admission; expiries that fell due while paused are swept at
    /// the next tick.
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.runtime.resume()
    }
    /// Stops accepting, lets (`Drain`) or interrupts (`Abort`) in-flight
    /// exports, and joins the listeners and sweeper within the deadline. The
    /// projection state is kept for a later Start of this object, reconciled
    /// with any admitted changes the stop discarded undelivered.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        let result = self.runtime.stop(ctx.mode(), ctx.deadline()).await;
        if result.is_ok() {
            self.reconcile_discarded().await;
            let counters = self.engine.counters();
            diag!(
                Info,
                "OTel source stopped: component={} accepted={} rejected={} dropped={} expired={}",
                self.runtime.descriptor.id(),
                counters.accepted,
                counters.rejected,
                counters.dropped,
                counters.expired
            );
        }
        result
    }
    /// The source owns no persistent state: its projection state lives in
    /// this object only. Erasing disposal is therefore an idempotent no-op.
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        if self.runtime.is_running() {
            return Err(anyhow::anyhow!("dispose requires a stopped source").into());
        }
        Ok(())
    }
}

impl OtelSource {
    async fn reconcile_discarded(&mut self) {
        let mut changes = Vec::new();
        for output in self.runtime.take_discarded() {
            match GraphChangeCodec::decode_changes(&output.envelope) {
                Ok(decoded) => changes.extend(decoded),
                Err(error) => diag!(
                    Warn,
                    "OTel source could not reconcile a discarded envelope: component={} error={error}",
                    self.runtime.descriptor.id()
                ),
            }
        }
        if changes.is_empty() {
            return;
        }
        let reconciled = self.engine.reconcile_discarded(&changes).await;
        diag!(
            Info,
            "OTel source reconciled discarded changes: component={} changes={} elements={reconciled}",
            self.runtime.descriptor.id(),
            changes.len()
        );
    }
}

#[async_trait]
impl NativeManagement for OtelSource {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        assess(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current = OtelSourceConfig::from_value(update.request.current.configuration.clone())
            .map_err(refused)?;
        let proposed = OtelSourceConfig::from_value(update.request.proposed.configuration.clone())
            .map_err(refused)?;
        if current != self.config {
            return Err(refused(
                "the source configuration changed after the update was assessed",
            ));
        }
        let Some(plan) = plan(&current, &proposed).plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        proposed.validate_exposure().map_err(refused)?;
        let ingress = proposed.ingress().map_err(refused)?;
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if self.runtime.is_running() && !self.runtime.is_paused() {
                    return Err(refused("in-place updates require a completed pause"));
                }
                self.runtime.update_live(ingress)?;
            }
            UpdateBoundary::Stop => {
                if self.runtime.is_running() {
                    return Err(refused("restart updates require a stopped source"));
                }
                self.runtime.reconfigure(ingress, None)?;
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        self.engine.update_settings(proposed.projection());
        self.config = proposed;
        Ok(())
    }
}

#[async_trait]
impl NativeSource for OtelSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.runtime.next().await
    }
}

/// Serves each protocol on its listener and sweeps elapsed TTLs, until the
/// activation stops. A listener failure ends the activation.
async fn serve(
    listeners: Vec<TcpListener>,
    ingress: Arc<Ingress>,
    options: ServeOptions,
) -> Result<()> {
    let ServeOptions {
        protocols,
        grpc: grpc_options,
        http: http_options,
        engine,
    } = options;
    let mut grpc_listener = None;
    let mut http_listener = None;
    for (protocol, listener) in protocols.into_iter().zip(listeners) {
        match protocol {
            Protocol::Grpc => grpc_listener = Some(listener),
            Protocol::Http => http_listener = Some(listener),
        }
    }
    let grpc = async {
        match grpc_listener {
            Some(listener) => grpc::serve(listener, ingress.clone(), engine.clone(), grpc_options)
                .await
                .context("OTLP/gRPC listener"),
            None => Ok(()),
        }
    };
    let http = async {
        match http_listener {
            Some(listener) => {
                let router = crate::http::router(ingress.clone(), engine.clone());
                let shutdown = ingress.shutdown.clone().cancelled_owned();
                server::serve(listener, router, shutdown, http_options)
                    .await
                    .context("OTLP/HTTP listener")
            }
            None => Ok(()),
        }
    };
    let sweeper = async {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = ingress.shutdown.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(failure) = engine.sweep(&ingress).await {
                        diag!(Debug, "TTL sweep deferred: component={} reason={failure}", ingress.source_id);
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::try_join!(grpc, http, sweeper)?;
    Ok(())
}
