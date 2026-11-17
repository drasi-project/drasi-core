// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Update planning for `drasi.mysql/storedproc`.
//!
//! * Pause (in place): everything read per operation or per connect —
//!   `routes`, `defaultTemplate`, `commandTimeoutMs`, `connectTimeoutMs`,
//!   `shutdownTimeoutMs`, `connectRetryAttempts`, `reconnectBudgetMs` and
//!   `maxStreams`.
//! * Stop (restart): the session — `hostname`, `port`, `database`, `user`,
//!   `password`, `ssl`, `tlsCaPem` and `maxProtocolBytes`.
//! * Replace: output aliases, the incoming edges and `deliveryGuarantee` (it
//!   is declared on the input port, so the graph re-checks every producer).
//! * Unsupported: `progressTable`. The cursor table is the identity of the
//!   exactly-once receipts. Moving it would drop the receipt of an operation
//!   in flight (a possible duplicate) and orphan this owner's rows in the old
//!   table, which no Dispose would ever erase. Remove the component (Erase)
//!   and add it again to change it, which makes the break explicit.

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
    if c.progress_table != p.progress_table {
        return Assessment::unsupported(
            "progressTable is the identity of the destination cursor; remove the component \
             (Erase) and add it again to move it",
        );
    }
    let mut planner = BoundaryPlanner::default();
    planner.field("inputs", inputs_changed, Replace);
    planner.field("outputs", c.outputs != p.outputs, Replace);
    planner.field("hostname", c.hostname != p.hostname, Stop);
    planner.field("port", c.port != p.port, Stop);
    planner.field("database", c.database != p.database, Stop);
    planner.field("user", c.user != p.user, Stop);
    planner.field("password", c.password != p.password, Stop);
    planner.field("ssl", c.ssl != p.ssl, Stop);
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
    planner.field(
        "connectRetryAttempts",
        c.connect_retry_attempts != p.connect_retry_attempts,
        Pause,
    );
    planner.field(
        "reconnectBudgetMs",
        c.reconnect_budget_ms != p.reconnect_budget_ms,
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
