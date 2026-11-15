// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Configuration, validation and update planning for `drasi.network/sse-sink`.

use anyhow::{ensure, Context, Result};
use axum::http::HeaderValue;
use drasi_computation_contracts::{OutputAliases, SinkOutputs};
use drasi_computation_network_common::{
    config::{self as shared, IngressSecurity, SecretString, SecuritySettings, MAX_CONNECTIONS},
    update::{ensure_same_component, inputs_changed, BoundaryPlanner},
};
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, UpdateBoundary},
    UpdateRequest,
};
use drasi_reaction_common::sse::{QueryConfig, SseRenderer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, net::IpAddr, time::Duration};

pub(crate) const MAX_BROADCAST_CAPACITY: usize = 1_048_576;
pub(crate) const MAX_PATHS: usize = 65_536;
pub(crate) const MAX_SUBSCRIBERS: usize = 65_536;
pub(crate) const MAX_CORS_ORIGINS: usize = 64;
pub(crate) const MAX_HEADER_READ_TIMEOUT_MS: u64 = 600_000;

fn port() -> u16 {
    8080
}
fn path() -> String {
    "/events".into()
}
fn heartbeat() -> u64 {
    30_000
}
fn keep_alive() -> u64 {
    15_000
}
fn capacity() -> usize {
    1024
}
fn max_paths() -> usize {
    1024
}
fn max_subscribers() -> usize {
    1024
}
fn header_read_timeout_ms() -> u64 {
    10_000
}
fn yes() -> bool {
    true
}

/// Settings for one native SSE sink.
///
/// Secret fields (`authToken`, `authTokenSecondary`, `tlsPrivateKeyPem`) are
/// never printed by `Debug`; `configuration()` still returns them because it is
/// the persistence hook.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SseSinkConfig {
    #[serde(default, skip_serializing_if = "OutputAliases::is_empty")]
    pub outputs: OutputAliases,
    #[serde(default = "shared::host")]
    pub host: String,
    #[serde(default = "port")]
    pub port: u16,
    #[serde(default = "path")]
    pub sse_path: String,
    #[serde(default = "heartbeat")]
    pub heartbeat_interval_ms: u64,
    #[serde(default = "keep_alive")]
    pub keep_alive_interval_ms: u64,
    #[serde(default)]
    pub event_ids: bool,
    #[serde(default = "capacity")]
    pub broadcast_capacity: usize,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, QueryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
    #[serde(default = "max_paths")]
    pub max_paths: usize,
    #[serde(default = "max_subscribers")]
    pub max_subscribers: usize,
    #[serde(default = "shared::max_connections")]
    pub max_connections: usize,
    #[serde(default = "header_read_timeout_ms")]
    pub header_read_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<SecretString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token_secondary: Option<SecretString>,
    #[serde(default = "yes")]
    pub allow_query_token: bool,
    #[serde(default)]
    pub allow_unauthenticated: bool,
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_per_second: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_burst: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_certificate_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_private_key_pem: Option<SecretString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_ca_pem: Option<String>,
}

impl SseSinkConfig {
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        ConfigDefinition::try_new([
            Field::new("outputs", Object, false, "Optional aliases for incoming outputs: a map of alias to {component, port?}. Every incoming graph edge is accepted whether or not it is aliased; the graph supplies the producing component, output port and stream. An unaliased output is named after its producing component. The alias names the output in routes and snapshot URLs.").constraint(drasi_computation_contracts::ConfigurationConstraint::output_aliases()),
            Field::new("host", String, false, "Literal IP address to listen on. A non-loopback address requires authToken, or allowUnauthenticated: true.").default(shared::host()),
            Field::new("port", Integer, false, "TCP port; zero requests an available local port.").default(port()).minimum(0).maximum(u16::MAX as u64),
            Field::new("ssePath", String, false, "Absolute event-stream path without query, fragment or control characters; /snapshot and its descendants are reserved.").default(path()),
            Field::new("heartbeatIntervalMs", Integer, false, "Interval in milliseconds between JSON heartbeat data frames ({\"type\":\"heartbeat\",\"ts\"}); suspended while paused.").default(heartbeat()).minimum(1),
            Field::new("keepAliveIntervalMs", Integer, false, "Interval in milliseconds between SSE comment keep-alives (': keep-alive'); continue while paused so proxies keep idle streams open.").default(keep_alive()).minimum(1),
            Field::new("eventIds", Boolean, false, "Send an 'id:' field with a monotonic per-path counter. Volatile: counters restart with each activation and Last-Event-ID is not replayed.").default(false),
            Field::new("broadcastCapacity", Integer, false, "Maximum retained notifications per path broadcast buffer; a lagging client is disconnected and must obtain a fresh snapshot.").default(capacity()).minimum(1).maximum(MAX_BROADCAST_CAPACITY as u64),
            Field::new("routes", Object, false, "Per-output-alias added/updated/deleted body templates and optional static or templated paths; full aliases take precedence over their last dotted segment.").default(serde_json::json!({})),
            Field::new("defaultTemplate", Object, false, "Fallback operation templates for outputs without a matching route; an empty body uses the default graph-change SSE payload."),
            Field::new("maxPaths", Integer, false, "Maximum number of configured and dynamically rendered event paths, including the base path. Idle dynamic paths are pruned when the bound is reached; exceeding it otherwise fails before broadcasting the input.").default(max_paths()).minimum(1).maximum(MAX_PATHS as u64),
            Field::new("maxSubscribers", Integer, false, "Maximum simultaneously connected event-stream subscribers across all paths; further subscriptions get 503.").default(max_subscribers()).minimum(1).maximum(MAX_SUBSCRIBERS as u64),
            Field::new("maxConnections", Integer, false, "Maximum simultaneously open client connections; further connections are closed immediately.").default(shared::max_connections()).minimum(1).maximum(MAX_CONNECTIONS as u64),
            Field::new("headerReadTimeoutMs", Integer, false, "Deadline in milliseconds for a client to send complete request headers (and finish a TLS handshake).").default(header_read_timeout_ms()).minimum(1).maximum(MAX_HEADER_READ_TIMEOUT_MS),
            Field::new("authToken", String, false, "Bearer token required on event-stream and snapshot routes (Authorization: Bearer <token>, or ?access_token= when allowQueryToken). At least 16 visible ASCII characters.").secret(),
            Field::new("authTokenSecondary", String, false, "Second accepted bearer token, for rotation without downtime. Requires authToken.").secret(),
            Field::new("allowQueryToken", Boolean, false, "Also accept the bearer token as the access_token query parameter, for browser EventSource clients that cannot set headers. Query strings may be logged by proxies.").default(true),
            Field::new("allowUnauthenticated", Boolean, false, "Permit a non-loopback listener without authToken, for deployments behind an authenticating proxy.").default(false),
            Field::new("corsAllowedOrigins", Array, false, "Origins (scheme://host[:port]) allowed to read cross-origin; [\"*\"] allows any origin. Empty sends no CORS headers.").default(serde_json::json!([])),
            Field::new("rateLimitPerSecond", Integer, false, "Per-client-address rate limit on new subscriptions, snapshot reads and preflights. Unset disables rate limiting.").minimum(1).maximum(shared::MAX_RATE_LIMIT as u64),
            Field::new("rateLimitBurst", Integer, false, "Per-client-address burst allowance. Defaults to rateLimitPerSecond.").minimum(1).maximum(shared::MAX_RATE_LIMIT as u64),
            Field::new("tlsCertificatePem", String, false, "PEM certificate chain served by the TLS listener. Requires tlsPrivateKeyPem."),
            Field::new("tlsPrivateKeyPem", String, false, "PEM private key for tlsCertificatePem.").secret(),
            Field::new("tlsClientCaPem", String, false, "PEM CA bundle; when set, clients must present a certificate issued by it (mutual TLS)."),
        ])
        .unwrap_or_else(|error| unreachable!("constant SSE sink configuration: {error}"))
    }

    /// Parses and validates a configuration, including the exposure rule.
    pub fn parse(value: Value) -> Result<Self> {
        if let Some(routes) = value.get("routes").and_then(Value::as_object) {
            for config in routes.values() {
                validate_template_fields(config)?;
            }
        }
        if let Some(config) = value
            .get("defaultTemplate")
            .filter(|value| !value.is_null())
        {
            validate_template_fields(config)?;
        }
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        self.outputs.validate()?;
        let ip = self.ip()?;
        ensure!(
            valid_path(&self.sse_path),
            "ssePath must be an absolute HTTP path without query, fragment or control characters and outside /snapshot"
        );
        shared::duration(self.heartbeat_interval_ms, "heartbeatIntervalMs")?;
        shared::duration(self.keep_alive_interval_ms, "keepAliveIntervalMs")?;
        ensure!(
            (1..=MAX_BROADCAST_CAPACITY).contains(&self.broadcast_capacity),
            "broadcastCapacity must be in 1..=1048576"
        );
        ensure!(
            (1..=MAX_PATHS).contains(&self.max_paths),
            "maxPaths must be in 1..=65536"
        );
        ensure!(
            (1..=MAX_SUBSCRIBERS).contains(&self.max_subscribers),
            "maxSubscribers must be in 1..=65536"
        );
        ensure!(
            (1..=MAX_CONNECTIONS).contains(&self.max_connections),
            "maxConnections must be in 1..=65536"
        );
        ensure!(
            (1..=MAX_HEADER_READ_TIMEOUT_MS).contains(&self.header_read_timeout_ms),
            "headerReadTimeoutMs must be in 1..=600000"
        );
        validate_cors(&self.cors_allowed_origins)?;
        let security = self.security()?;
        ensure!(
            ip.is_loopback() || !security.auth_tokens.is_empty() || self.allow_unauthenticated,
            "listening on non-loopback address {} requires authToken, or allowUnauthenticated: true behind an authenticating proxy",
            self.host
        );
        SseRenderer::validate(
            &self.routes,
            self.default_template.as_ref(),
            &self.routes.keys().cloned().collect::<Vec<_>>(),
        )?;
        let paths = self.renderer().static_paths();
        ensure!(
            paths.len() <= self.max_paths && paths.iter().all(|path| valid_path(path)),
            "configured SSE paths are invalid or exceed maxPaths"
        );
        Ok(())
    }

    pub(crate) fn ip(&self) -> Result<IpAddr> {
        self.host
            .parse()
            .map_err(|_| anyhow::anyhow!("host must be a literal IP address"))
    }

    /// The validated tokens, rate limit and TLS material.
    pub(crate) fn security(&self) -> Result<IngressSecurity> {
        SecuritySettings {
            auth_token: self.auth_token.as_ref(),
            auth_token_secondary: self.auth_token_secondary.as_ref(),
            allow_unauthenticated: self.allow_unauthenticated,
            rate_limit_per_second: self.rate_limit_per_second,
            rate_limit_burst: self.rate_limit_burst,
            tls_certificate_pem: self.tls_certificate_pem.as_deref(),
            tls_private_key_pem: self.tls_private_key_pem.as_ref(),
            tls_client_ca_pem: self.tls_client_ca_pem.as_deref(),
        }
        .build()
    }

    /// Checks that depend on the incoming edges: routes must name an output of
    /// this sink.
    pub(crate) fn validate_outputs(&self, outputs: &SinkOutputs) -> Result<()> {
        SseRenderer::validate(
            &self.routes,
            self.default_template.as_ref(),
            &outputs.aliases().map(str::to_owned).collect::<Vec<_>>(),
        )?;
        Ok(())
    }

    pub(crate) fn renderer(&self) -> SseRenderer {
        SseRenderer::new(
            self.sse_path.clone(),
            self.routes.clone(),
            self.default_template.clone(),
        )
    }

    pub(crate) fn heartbeat(&self) -> Duration {
        Duration::from_millis(self.heartbeat_interval_ms)
    }

    pub(crate) fn keep_alive(&self) -> Duration {
        Duration::from_millis(self.keep_alive_interval_ms)
    }
}

pub(crate) fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains(['?', '#'])
        && !path.chars().any(char::is_control)
        && path != "/snapshot"
        && !path.starts_with("/snapshot/")
}

/// `["*"]` alone, or distinct `http(s)://host[:port]` origins without a path.
fn validate_cors(origins: &[String]) -> Result<()> {
    ensure!(
        origins.len() <= MAX_CORS_ORIGINS,
        "corsAllowedOrigins accepts at most {MAX_CORS_ORIGINS} entries"
    );
    if origins.iter().any(|origin| origin == "*") {
        ensure!(
            origins.len() == 1,
            "corsAllowedOrigins: \"*\" must be the only entry"
        );
        return Ok(());
    }
    for (index, origin) in origins.iter().enumerate() {
        let rest = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))
            .with_context(|| {
                format!("CORS origin {origin:?} must start with http:// or https://")
            })?;
        ensure!(
            !rest.is_empty()
                && !rest.contains(['/', '?', '#', '@'])
                && rest.bytes().all(|byte| byte.is_ascii_graphic())
                && HeaderValue::from_str(origin).is_ok(),
            "CORS origin {origin:?} must be scheme://host[:port] without a path"
        );
        ensure!(
            !origins[..index]
                .iter()
                .any(|other| other.eq_ignore_ascii_case(origin)),
            "duplicate CORS origin {origin:?}"
        );
    }
    Ok(())
}

fn validate_template_fields(config: &Value) -> Result<()> {
    for (operation, spec) in config
        .as_object()
        .context("SSE templates must be an object")?
    {
        ensure!(
            matches!(operation.as_str(), "added" | "updated" | "deleted"),
            "unsupported SSE template operation: {operation}"
        );
        if spec.is_null() {
            continue;
        }
        for field in spec
            .as_object()
            .context("SSE operation template must be an object")?
            .keys()
        {
            ensure!(
                matches!(field.as_str(), "template" | "path"),
                "unsupported SSE template field: {field}"
            );
        }
    }
    Ok(())
}

/// Classifies each configuration change by the weakest safe boundary.
///
/// * Pause (in place): values read per delivery or per request — templates,
///   routes, heartbeat and keep-alive intervals, event ids, tokens, CORS,
///   rate limits and the subscriber cap.
/// * Stop (restart): anything bound when the listener starts — address,
///   port, TLS, base path, broadcast capacity, path bound, connection limit
///   and header deadline.
/// * Replace: output aliases (the bound inputs are checked separately).
pub(crate) fn plan(current: &SseSinkConfig, proposed: &SseSinkConfig) -> Assessment {
    use UpdateBoundary::*;
    let (c, p) = (current, proposed);
    let mut planner = BoundaryPlanner::default();
    planner.field("outputs", c.outputs != p.outputs, Replace);
    planner.field("host", c.host != p.host, Stop);
    planner.field("port", c.port != p.port, Stop);
    planner.field(
        "tls",
        c.tls_certificate_pem != p.tls_certificate_pem
            || c.tls_private_key_pem != p.tls_private_key_pem
            || c.tls_client_ca_pem != p.tls_client_ca_pem,
        Stop,
    );
    planner.field("ssePath", c.sse_path != p.sse_path, Stop);
    planner.field(
        "broadcastCapacity",
        c.broadcast_capacity != p.broadcast_capacity,
        Stop,
    );
    planner.field("maxPaths", c.max_paths != p.max_paths, Stop);
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
        "templates",
        c.routes != p.routes || c.default_template != p.default_template,
        Pause,
    );
    planner.field(
        "heartbeatIntervalMs",
        c.heartbeat_interval_ms != p.heartbeat_interval_ms,
        Pause,
    );
    planner.field(
        "keepAliveIntervalMs",
        c.keep_alive_interval_ms != p.keep_alive_interval_ms,
        Pause,
    );
    planner.field("eventIds", c.event_ids != p.event_ids, Pause);
    planner.field(
        "authToken",
        c.auth_token != p.auth_token || c.auth_token_secondary != p.auth_token_secondary,
        Pause,
    );
    planner.field(
        "allowQueryToken",
        c.allow_query_token != p.allow_query_token,
        Pause,
    );
    planner.field(
        "allowUnauthenticated",
        c.allow_unauthenticated != p.allow_unauthenticated,
        Pause,
    );
    planner.field(
        "corsAllowedOrigins",
        c.cors_allowed_origins != p.cors_allowed_origins,
        Pause,
    );
    planner.field(
        "rateLimit",
        c.rate_limit_per_second != p.rate_limit_per_second
            || c.rate_limit_burst != p.rate_limit_burst,
        Pause,
    );
    planner.field(
        "maxSubscribers",
        c.max_subscribers != p.max_subscribers,
        Pause,
    );
    planner.assessment()
}

/// Pure planning used by the factory and the live instance. A change to the
/// incoming edges or aliases replaces the component; an invalid proposal is
/// an error, never an implicit Replace.
pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure_same_component(request)?;
    let current = SseSinkConfig::parse(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = SseSinkConfig::parse(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    let outputs = SinkOutputs::from_inputs(&request.proposed.inputs, &proposed.outputs)
        .context("proposed outputs")?;
    proposed.validate_outputs(&outputs)?;
    if inputs_changed(request) {
        return Ok(Assessment::supported(
            UpdateBoundary::Replace,
            drasi_computation_contracts::ComponentUpdateData::Preserve,
        ));
    }
    Ok(plan(&current, &proposed))
}
