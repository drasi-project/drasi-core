// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Update planning. Every field is classified by the weakest boundary at
//! which a change is safe:
//!
//! - **InPlace (Pause):** `pollIntervalMs`, `maxRows`, `maxBytes`,
//!   `reconnectBudgetMs` — read by the worker before each poll or
//!   reconnect, applied after a completed pause.
//! - **Restart (Stop):** connection, TLS, credentials, timeouts,
//!   `serverTimeZone` and `startPosition`. The next start reconnects and
//!   resumes at the members' committed minimum; the binding check refuses a
//!   different database. `startPosition` only matters to a group without
//!   committed cursors.
//! - **Unsupported:** `stream`, `database`, `tables`, `tableKeys`,
//!   `barrierTable` — what the committed cursors and owned markers are bound
//!   to. Remove the source with erase and add it again; a replacement would
//!   need every downstream query rebuilt from a new snapshot, which this
//!   source cannot coordinate, so it does not claim a Recreate plan.
//!
//! Consumer membership is a host dependency, not configuration: changing it
//! is a host-level replacement of the source.

use crate::config::SourceConfig;
use anyhow::{Context, Result};
use drasi_computation_contracts::ComponentUpdateData;
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, UpdateBoundary},
    update_planning::{ensure_same_component, BoundaryPlanner},
    UpdateRequest,
};

/// The parsed current and proposed configurations of `request`.
pub(crate) fn configurations(request: &UpdateRequest) -> Result<(SourceConfig, SourceConfig)> {
    ensure_same_component(request)?;
    Ok((
        SourceConfig::from_value(&request.current.configuration)
            .context("current configuration")?,
        SourceConfig::from_value(&request.proposed.configuration)
            .context("proposed configuration")?,
    ))
}

/// Pure planning: configuration only, no I/O, clock or live state.
pub(crate) fn plan(current: &SourceConfig, proposed: &SourceConfig) -> Assessment {
    use UpdateBoundary::{Pause, Stop};
    let (c, p) = (&current.settings, &proposed.settings);
    let bound = [
        ("stream", current.stream != proposed.stream),
        ("database", c.database != p.database),
        ("tables", c.tables != p.tables),
        ("tableKeys", c.table_keys != p.table_keys),
        ("barrierTable", c.barrier_table != p.barrier_table),
    ];
    let refused: Vec<_> = bound
        .iter()
        .filter(|(_, changed)| *changed)
        .map(|(name, _)| *name)
        .collect();
    if !refused.is_empty() {
        return Assessment::Unsupported {
            reason: format!(
                "{} cannot change for an existing SQL Server source; remove the source with \
                 erase and add it again",
                refused.join(", ")
            ),
        };
    }
    let mut planner = BoundaryPlanner::default();
    planner.field(
        "pollIntervalMs",
        c.poll_interval_ms != p.poll_interval_ms,
        Pause,
    );
    planner.field("maxRows", c.max_rows != p.max_rows, Pause);
    planner.field("maxBytes", c.max_bytes != p.max_bytes, Pause);
    planner.field(
        "reconnectBudgetMs",
        c.reconnect_budget_ms != p.reconnect_budget_ms,
        Pause,
    );
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
    planner.field(
        "connectTimeoutMs",
        c.connect_timeout_ms != p.connect_timeout_ms,
        Stop,
    );
    planner.field(
        "commandTimeoutMs",
        c.command_timeout_ms != p.command_timeout_ms,
        Stop,
    );
    planner.field(
        "serverTimeZone",
        c.server_time_zone != p.server_time_zone,
        Stop,
    );
    planner.field("startPosition", c.start_position != p.start_position, Stop);
    planner.assessment()
}

/// Factory-level planning.
pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    let (current, proposed) = configurations(request)?;
    Ok(plan(&current, &proposed))
}

/// Whether `assessment` is the supported, data-preserving plan at `boundary`.
pub(crate) fn same_plan(assessment: &Assessment, boundary: UpdateBoundary) -> bool {
    matches!(
        assessment,
        Assessment::Supported { boundary: b, data: ComponentUpdateData::Preserve, .. } if *b == boundary
    )
}

#[cfg(test)]
pub(crate) use tests::cases as tests_cases;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::{json, Value};

    pub(crate) fn base() -> Value {
        json!({"stream": "items/out", "database": "app", "user": "drasi", "password": "p"})
    }

    fn boundary(assessment: &Assessment) -> Option<UpdateBoundary> {
        match assessment {
            Assessment::Supported { boundary, data, .. } => {
                assert_eq!(*data, ComponentUpdateData::Preserve);
                Some(*boundary)
            }
            Assessment::Unsupported { .. } => None,
        }
    }

    /// Every field, with its expected boundary.
    pub(crate) fn cases() -> Vec<(&'static str, Value, Option<UpdateBoundary>)> {
        use UpdateBoundary::{Pause, Stop};
        let pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
        vec![
            ("pollIntervalMs", json!(50), Some(Pause)),
            ("maxRows", json!(10), Some(Pause)),
            ("maxBytes", json!(4096), Some(Pause)),
            ("reconnectBudgetMs", json!(0), Some(Pause)),
            ("host", json!("sqlag-listener.corp"), Some(Stop)),
            ("port", json!(14330), Some(Stop)),
            ("user", json!("other"), Some(Stop)),
            ("password", json!("rotated"), Some(Stop)),
            ("encryption", json!("loginOnly"), Some(Stop)),
            ("trustServerCertificate", json!(true), Some(Stop)),
            ("trustedCaPem", json!(pem), Some(Stop)),
            ("hostNameInCertificate", json!("sql.corp"), Some(Stop)),
            ("connectTimeoutMs", json!(1000), Some(Stop)),
            ("commandTimeoutMs", json!(1000), Some(Stop)),
            ("serverTimeZone", json!("UTC"), Some(Stop)),
            ("startPosition", json!("beginning"), Some(Stop)),
            ("stream", json!("other/out"), None),
            ("database", json!("other"), None),
            ("tables", json!(["dbo.Items"]), None),
            (
                "tableKeys",
                json!([{"table": "dbo.Items", "keyColumns": ["id"]}]),
                None,
            ),
            ("barrierTable", json!("dbo.Barrier"), None),
        ]
    }

    #[test]
    fn every_field_has_a_tested_boundary() {
        let current = SourceConfig::from_value(&base()).unwrap();
        let mut covered: Vec<&str> = Vec::new();
        for (field, value, expected) in cases() {
            let mut proposed = base();
            proposed[field] = value.clone();
            let proposed = SourceConfig::from_value(&proposed).unwrap();
            assert_eq!(
                boundary(&plan(&current, &proposed)),
                expected,
                "{field} = {value}"
            );
            covered.push(field);
        }
        covered.push("stream");
        for name in drasi_mssql_common::native::settings::SETTINGS_FIELDS {
            assert!(covered.contains(name), "{name} has no tested boundary");
        }
        assert_eq!(
            boundary(&plan(&current, &current)),
            Some(UpdateBoundary::Pause),
            "no change is applied in place"
        );
    }

    #[test]
    fn the_strongest_changed_field_wins_and_bound_fields_refuse_the_plan() {
        let current = SourceConfig::from_value(&base()).unwrap();
        let mut mixed = base();
        mixed["maxRows"] = json!(5);
        mixed["host"] = json!("other");
        let mixed = SourceConfig::from_value(&mixed).unwrap();
        assert_eq!(
            boundary(&plan(&current, &mixed)),
            Some(UpdateBoundary::Stop)
        );
        let mut refused = base();
        refused["host"] = json!("other");
        refused["tables"] = json!(["dbo.Other"]);
        let refused = SourceConfig::from_value(&refused).unwrap();
        assert_eq!(boundary(&plan(&current, &refused)), None);
    }
}
