// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Update planning for `drasi.mysql/storedproc`.
//!
//! * Pause (in place): everything read per operation or per connect —
//!   `routes`, `defaultTemplate`, `commandTimeoutMs`, `connectTimeoutMs`,
//!   `shutdownTimeoutMs` and `maxStreams`.
//! * Stop (restart): the login and transport — `user`, `password`,
//!   `sslMode`, `allowRemotePlaintext`, `tlsCaPem` and `maxProtocolBytes`.
//! * Replace: output aliases, the incoming edges, `deliveryGuarantee` and
//!   `onPermanentFailure` (both are declared in the description the graph
//!   checks).
//! * Unsupported: `host`, `port`, `database` and `progressTable`. They locate
//!   the destination cursor: moving it would drop the receipt of an operation
//!   in flight (a possible duplicate) and orphan this owner's rows, which no
//!   Dispose would ever erase. Remove the component (Erase) and add it again
//!   to change them, which makes the break explicit.

use crate::config::MySqlStoredProcConfig;
use anyhow::{Context, Result};
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, UpdateBoundary},
    update_planning::{ensure_same_component, inputs_changed, BoundaryPlanner},
    UpdateRequest,
};

fn parse(value: &serde_json::Value, credentials_bound: bool) -> Result<MySqlStoredProcConfig> {
    // A configuration without an account is valid only with activation
    // credentials; planning accepts it either way and lets create decide.
    MySqlStoredProcConfig::parse_with_activation_credentials(value.clone(), credentials_bound)
}

/// Classifies each changed field by the weakest safe boundary.
pub fn plan(
    current: &MySqlStoredProcConfig,
    proposed: &MySqlStoredProcConfig,
    inputs_changed: bool,
) -> Assessment {
    use UpdateBoundary::*;
    let (c, p) = (current, proposed);
    for (field, changed) in [
        ("host", c.host != p.host),
        ("port", c.port != p.port),
        ("database", c.database != p.database),
        ("progressTable", c.progress().ok() != p.progress().ok()),
    ] {
        if changed {
            return Assessment::unsupported(
                drasi_computation_storedproc_common::settings::cursor_identity_changed(field),
            );
        }
    }
    let mut planner = BoundaryPlanner::default();
    planner.field("inputs", inputs_changed, Replace);
    planner.field("outputs", c.outputs != p.outputs, Replace);
    planner.field(
        "onPermanentFailure",
        c.on_permanent_failure != p.on_permanent_failure,
        Replace,
    );
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
        "maxProtocolBytes",
        c.max_protocol_bytes != p.max_protocol_bytes,
        Stop,
    );
    planner.field(
        "deliveryGuarantee",
        c.delivery_guarantee != p.delivery_guarantee,
        Replace,
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
        "connectTimeoutMs",
        c.connect_timeout_ms != p.connect_timeout_ms,
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
    let current = parse(&request.current.configuration, true).context("current configuration")?;
    let proposed =
        parse(&request.proposed.configuration, true).context("proposed configuration")?;
    let outputs = drasi_computation_contracts::SinkOutputs::from_inputs(
        &request.proposed.inputs,
        &proposed.outputs,
    )?;
    proposed.validate_outputs(&outputs)?;
    Ok(plan(&current, &proposed, inputs_changed(request)))
}
