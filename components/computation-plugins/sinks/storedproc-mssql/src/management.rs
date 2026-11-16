// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Update planning: every configuration field is classified.
//!
//! * **InPlace (Pause):** `routes`, `defaultTemplate` (rendered per
//!   operation), `maxStreams`, `retryBudgetMs` (read per operation),
//!   `commandTimeoutMs`, `connectTimeoutMs` (applied by replacing the idle
//!   connection; the next operation reconnects).
//! * **Restart (Stop):** `host`, `port`, `user`, `password`, `encryption`,
//!   `trustServerCertificate`, `trustedCaPem`, `hostNameInCertificate`: the
//!   connection and its credentials are established at start;
//!   `deliveryGuarantee`: which producers the sink accepts is fixed for one
//!   activation.
//! * **Recreate (Replace):** `outputs` and a change to the bound inputs: the
//!   route aliases and delivery streams change, which is not a live update
//!   (CW-94).
//! * **Unsupported:** `database` and `progressTable`. They are the cursor's
//!   identity: the cursor lives in that table in that database, so moving it
//!   would restart every stream without its completed prefix (duplicate or
//!   refused operations) and leave the old rows behind. Remove the sink with
//!   Erase and create a new one instead.

use crate::config::MsSqlStoredProcConfig;
use anyhow::{Context, Result};
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, UpdateBoundary},
    update_planning::{ensure_same_component, inputs_changed, BoundaryPlanner},
    UpdateRequest,
};

/// The boundary each field needs, for documentation and tests.
pub const FIELD_BOUNDARIES: [(&str, Option<UpdateBoundary>); 19] = [
    ("host", Some(UpdateBoundary::Stop)),
    ("port", Some(UpdateBoundary::Stop)),
    ("database", None),
    ("user", Some(UpdateBoundary::Stop)),
    ("password", Some(UpdateBoundary::Stop)),
    ("encryption", Some(UpdateBoundary::Stop)),
    ("trustServerCertificate", Some(UpdateBoundary::Stop)),
    ("trustedCaPem", Some(UpdateBoundary::Stop)),
    ("hostNameInCertificate", Some(UpdateBoundary::Stop)),
    ("progressTable", None),
    ("maxStreams", Some(UpdateBoundary::Pause)),
    ("deliveryGuarantee", Some(UpdateBoundary::Stop)),
    ("outputs", Some(UpdateBoundary::Replace)),
    ("routes", Some(UpdateBoundary::Pause)),
    ("defaultTemplate", Some(UpdateBoundary::Pause)),
    ("commandTimeoutMs", Some(UpdateBoundary::Pause)),
    ("connectTimeoutMs", Some(UpdateBoundary::Pause)),
    ("retryBudgetMs", Some(UpdateBoundary::Pause)),
    ("inputs", Some(UpdateBoundary::Replace)),
];

/// Whether an in-place update must replace the idle connection.
pub(crate) fn changes_connection_settings(
    current: &MsSqlStoredProcConfig,
    proposed: &MsSqlStoredProcConfig,
) -> bool {
    current.command_timeout_ms != proposed.command_timeout_ms
        || current.connect_timeout_ms != proposed.connect_timeout_ms
}

/// Plans an update between two valid configurations.
pub fn plan(
    current: &MsSqlStoredProcConfig,
    proposed: &MsSqlStoredProcConfig,
    inputs_changed: bool,
) -> Assessment {
    use UpdateBoundary::*;
    let (c, p) = (current, proposed);
    if c.database != p.database {
        return Assessment::unsupported(
            "database holds the destination cursor; changing it would lose each stream's \
             completed prefix. Remove the sink (Erase) and create a new one",
        );
    }
    if c.progress_table != p.progress_table {
        return Assessment::unsupported(
            "progressTable is the destination cursor's identity; changing it would lose each \
             stream's completed prefix. Remove the sink (Erase) and create a new one",
        );
    }
    let mut planner = BoundaryPlanner::default();
    planner.field("host", c.host != p.host, Stop);
    planner.field("port", c.port != p.port, Stop);
    planner.field("user", c.user != p.user, Stop);
    planner.field("password", c.password != p.password, Stop);
    planner.field("encryption", c.encryption != p.encryption, Stop);
    planner.field(
        "trustServerCertificate",
        c.trust_server_certificate != p.trust_server_certificate,
        Stop,
    );
    planner.field("trustedCaPem", c.trusted_ca_pem != p.trusted_ca_pem, Stop);
    planner.field(
        "hostNameInCertificate",
        c.host_name_in_certificate != p.host_name_in_certificate,
        Stop,
    );
    planner.field("maxStreams", c.max_streams != p.max_streams, Pause);
    planner.field(
        "deliveryGuarantee",
        c.delivery_guarantee != p.delivery_guarantee,
        Stop,
    );
    planner.field("outputs", c.outputs != p.outputs, Replace);
    planner.field("routes", c.routes != p.routes, Pause);
    planner.field(
        "defaultTemplate",
        c.default_template != p.default_template,
        Pause,
    );
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
        "retryBudgetMs",
        c.retry_budget_ms != p.retry_budget_ms,
        Pause,
    );
    planner.field("inputs", inputs_changed, Replace);
    planner.assessment()
}

/// Pure assessment of a host update request.
pub fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure_same_component(request)?;
    let current = MsSqlStoredProcConfig::parse(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = MsSqlStoredProcConfig::parse(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    Ok(plan(&current, &proposed, inputs_changed(request)))
}
