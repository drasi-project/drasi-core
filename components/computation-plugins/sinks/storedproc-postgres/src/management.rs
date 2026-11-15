// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Update planning: every configuration field is classified.
//!
//! * Pause (in place): anything read per delivery: `routes`,
//!   `defaultTemplate`, `maxStreams`, `commandTimeoutMs` (applied to the
//!   session before the next command), `reconnectBudgetMs`,
//!   `shutdownTimeoutMs`.
//! * Stop (restart): the connection: `host`, `port`, `database`, `user`,
//!   `password`, `sslMode`, `allowRemotePlaintext`, the TLS material and
//!   `connectTimeoutMs`.
//! * Replace (recreate, data preserved): `outputs` and the incoming edges.
//! * Unsupported: `progressTable` and the graph identity. They change the
//!   destination cursor's identity, so committed operations would no longer
//!   be recognised. Remove the component (Erase) and add a new one instead.

use crate::config::PostgresStoredProcConfig;
use anyhow::{Context, Result};
use drasi_computation_contracts::SinkOutputs;
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, UpdateBoundary},
    update_planning::{ensure_same_component, inputs_changed, BoundaryPlanner},
    UpdateRequest,
};

/// Classifies every field of `current` against `proposed`.
pub fn plan(
    current: &PostgresStoredProcConfig,
    proposed: &PostgresStoredProcConfig,
    inputs_changed: bool,
) -> Assessment {
    use UpdateBoundary::*;
    let (c, p) = (current, proposed);
    if c.progress_table != p.progress_table {
        return Assessment::unsupported(
            "progressTable is the destination cursor identity; remove the component with Erase \
             and add a new one to change it",
        );
    }
    let mut planner = BoundaryPlanner::default();
    planner.field("inputs", inputs_changed, Replace);
    planner.field("outputs", c.outputs != p.outputs, Replace);
    planner.field("host", c.host != p.host, Stop);
    planner.field("port", c.port != p.port, Stop);
    planner.field("database", c.database != p.database, Stop);
    planner.field("user", c.user != p.user, Stop);
    planner.field("password", c.password != p.password, Stop);
    planner.field("sslMode", c.ssl_mode != p.ssl_mode, Stop);
    planner.field(
        "allowRemotePlaintext",
        c.allow_remote_plaintext != p.allow_remote_plaintext,
        Stop,
    );
    planner.field("tlsCaPem", c.tls_ca_pem != p.tls_ca_pem, Stop);
    planner.field(
        "tlsClientCertificatePem",
        c.tls_client_certificate_pem != p.tls_client_certificate_pem,
        Stop,
    );
    planner.field(
        "tlsClientPrivateKeyPem",
        c.tls_client_private_key_pem != p.tls_client_private_key_pem,
        Stop,
    );
    planner.field(
        "connectTimeoutMs",
        c.connect_timeout_ms != p.connect_timeout_ms,
        Stop,
    );
    planner.field("routes", c.routes != p.routes, Pause);
    planner.field(
        "defaultTemplate",
        c.default_template != p.default_template,
        Pause,
    );
    planner.field("maxStreams", c.max_streams != p.max_streams, Pause);
    planner.field(
        "commandTimeoutMs",
        c.command_timeout_ms != p.command_timeout_ms,
        Pause,
    );
    planner.field(
        "reconnectBudgetMs",
        c.reconnect_budget_ms != p.reconnect_budget_ms,
        Pause,
    );
    planner.field(
        "shutdownTimeoutMs",
        c.shutdown_timeout_ms != p.shutdown_timeout_ms,
        Pause,
    );
    planner.assessment()
}

/// Pure planning used by the factory and the live instance. The proposed
/// configuration must be valid for the proposed incoming edges.
pub fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure_same_component(request)?;
    let graph = |request: &drasi_computation_plugin_sdk::CreateRequest| {
        request.scope.as_ref().map(|scope| scope.graph_id.clone())
    };
    if graph(&request.current) != graph(&request.proposed) {
        return Ok(Assessment::unsupported(
            "the graph identity determines the destination cursor owner",
        ));
    }
    let current = PostgresStoredProcConfig::parse(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = PostgresStoredProcConfig::parse(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    let outputs = SinkOutputs::from_inputs(&request.proposed.inputs, &proposed.outputs)?;
    proposed.validate_outputs(&outputs)?;
    Ok(plan(&current, &proposed, inputs_changed(request)))
}
