// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Update planning.
//!
//! Every field is classified by the weakest boundary at which a change is
//! safe:
//!
//! - **InPlace (Pause):** `reconnectBudgetMs`, read only when a connection
//!   fails.
//! - **Restart (Stop):** connection, credentials, TLS, `serverId`, timing,
//!   protocol limits and `snapshotLockTimeoutMs`. The next start reopens
//!   replication at the committed cursor; the binding check refuses a server
//!   that is not the same system (`@@server_uuid`). Transaction limits may
//!   only grow, so a retained transaction never becomes unpublishable.
//! - **Unsupported:** anything the committed cursor or the emitted element
//!   identities are bound to: `stream`, `database`, `tables`, `tableKeys`,
//!   `retention` and `persistent`. Remove the source with erase and add it
//!   again; a replacement would need a rebuild of every downstream query,
//!   which this source does not claim to coordinate.
//! - **Start fields** (`startFrom`, `startGtidSet`, `startBinlogFile`,
//!   `startBinlogPosition`): Restart for untracked factories (they have no
//!   cursor) and for tracked factories while no checkpoint exists (the live
//!   object checks); otherwise unsupported. Switching to or from
//!   `snapshot` is always unsupported for tracked factories.

use crate::config::{Mode, SourceConfig};
use anyhow::{Context, Result};
use drasi_computation_contracts::ComponentUpdateData;
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, UpdateBoundary},
    update_planning::{ensure_same_component, BoundaryPlanner},
    UpdateRequest,
};
use drasi_mysql_common::native::StartFrom;

/// Fingerprint naming the live fact a start-field change relies on.
pub(crate) const NO_CHECKPOINT: &str = "mysql:no-committed-cursor";

/// The parsed current and proposed configurations of `request`.
pub(crate) fn configurations(
    mode: Mode,
    request: &UpdateRequest,
) -> Result<(SourceConfig, SourceConfig)> {
    ensure_same_component(request)?;
    Ok((
        SourceConfig::from_value(mode, &request.current.configuration)
            .context("current configuration")?,
        SourceConfig::from_value(mode, &request.proposed.configuration)
            .context("proposed configuration")?,
    ))
}

/// Pure planning. `uncommitted` is true only when the live object has
/// observed that no cursor has been committed yet.
pub(crate) fn plan(
    current: &SourceConfig,
    proposed: &SourceConfig,
    uncommitted: bool,
) -> Assessment {
    use UpdateBoundary::{Pause, Stop};
    let (c, p) = (&current.settings, &proposed.settings);
    let start_changed = c.start_from != p.start_from
        || c.start_gtid_set != p.start_gtid_set
        || c.start_binlog_file != p.start_binlog_file
        || c.start_binlog_position != p.start_binlog_position;
    let tracked = current.mode.tracked();
    let snapshot_switch =
        (c.start_from == StartFrom::Snapshot) != (p.start_from == StartFrom::Snapshot);
    let bound = [
        ("stream", current.stream != proposed.stream),
        ("database", c.database != p.database),
        ("tables", c.tables != p.tables),
        ("tableKeys", c.table_keys != p.table_keys),
        ("retention", current.retention != proposed.retention),
        ("persistent", current.persistent != proposed.persistent),
        (
            "maxTransactionChanges",
            p.max_transaction_changes < c.max_transaction_changes,
        ),
        (
            "maxTransactionBytes",
            p.max_transaction_bytes < c.max_transaction_bytes,
        ),
        (
            "startFrom",
            tracked && start_changed && (snapshot_switch || !uncommitted),
        ),
    ];
    let refused: Vec<_> = bound
        .iter()
        .filter(|(_, changed)| *changed)
        .map(|(name, _)| *name)
        .collect();
    if !refused.is_empty() {
        return Assessment::Unsupported {
            reason: format!(
                "{} cannot change for an existing MySQL source (transaction limits may only grow; start fields only before the first checkpoint); remove the source with erase and add it again",
                refused.join(", ")
            ),
        };
    }
    let mut planner = BoundaryPlanner::default();
    planner.field(
        "reconnectBudgetMs",
        c.reconnect_budget_ms != p.reconnect_budget_ms,
        Pause,
    );
    planner.field("host", c.host != p.host, Stop);
    planner.field("port", c.port != p.port, Stop);
    planner.field("user", c.user != p.user, Stop);
    planner.field("password", c.password != p.password, Stop);
    planner.field("sslMode", c.ssl_mode != p.ssl_mode, Stop);
    planner.field("tlsCaPem", c.tls_ca_pem != p.tls_ca_pem, Stop);
    planner.field(
        "tlsClientCertPem",
        c.tls_client_cert_pem != p.tls_client_cert_pem,
        Stop,
    );
    planner.field(
        "tlsClientKeyPem",
        c.tls_client_key_pem != p.tls_client_key_pem,
        Stop,
    );
    planner.field("serverId", c.server_id != p.server_id, Stop);
    planner.field(
        "heartbeatIntervalMs",
        c.heartbeat_interval_ms != p.heartbeat_interval_ms,
        Stop,
    );
    planner.field(
        "snapshotLockTimeoutMs",
        c.snapshot_lock_timeout_ms != p.snapshot_lock_timeout_ms,
        Stop,
    );
    planner.field(
        "maxTransactionChanges",
        c.max_transaction_changes != p.max_transaction_changes,
        Stop,
    );
    planner.field(
        "maxTransactionBytes",
        c.max_transaction_bytes != p.max_transaction_bytes,
        Stop,
    );
    planner.field(
        "maxTransactionDurationMs",
        c.max_transaction_duration_ms != p.max_transaction_duration_ms,
        Stop,
    );
    planner.field(
        "maxProtocolBytes",
        c.max_protocol_bytes != p.max_protocol_bytes,
        Stop,
    );
    planner.field("ioTimeoutMs", c.io_timeout_ms != p.io_timeout_ms, Stop);
    planner.field("startFrom", start_changed, Stop);
    let assessment = planner.assessment();
    match assessment {
        Assessment::Supported { boundary, data, .. } if start_changed && tracked => {
            Assessment::Supported {
                boundary,
                data,
                fingerprint: Some(NO_CHECKPOINT.into()),
            }
        }
        other => other,
    }
}

/// Factory-level planning, without live facts: a tracked start change is
/// refused because the factory cannot see whether a cursor was committed.
pub(crate) fn assess(mode: Mode, request: &UpdateRequest) -> Result<Assessment> {
    let (current, proposed) = configurations(mode, request)?;
    Ok(plan(&current, &proposed, false))
}

/// Whether an assessment names a supported, data-preserving plan at
/// `boundary`.
pub(crate) fn same_plan(left: &Assessment, boundary: UpdateBoundary) -> bool {
    matches!(
        left,
        Assessment::Supported { boundary: b, data: ComponentUpdateData::Preserve, .. } if *b == boundary
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MODES;
    use serde_json::{json, Value};

    fn base(mode: Mode) -> Value {
        let mut value = json!({"stream": "changes", "database": "app", "user": "drasi"});
        if mode.requires_fixed_start() {
            value["startFrom"] = json!("position");
            value["startBinlogFile"] = json!("binlog.000001");
            value["startBinlogPosition"] = json!(4);
        }
        if mode.positional() {
            value["tables"] = json!(["items"]);
            value["tableKeys"] = json!([{"table": "items", "keyColumns": ["@0"]}]);
        }
        value
    }

    fn change(mode: Mode, edits: &[(&str, Value)], uncommitted: bool) -> Option<UpdateBoundary> {
        let current = SourceConfig::from_value(mode, &base(mode)).unwrap();
        let mut proposed = base(mode);
        for (field, value) in edits {
            if value.is_null() {
                proposed.as_object_mut().unwrap().remove(*field);
            } else {
                proposed[*field] = value.clone();
            }
        }
        let proposed = SourceConfig::from_value(mode, &proposed).unwrap();
        match plan(&current, &proposed, uncommitted) {
            Assessment::Supported { boundary, data, .. } => {
                assert_eq!(data, ComponentUpdateData::Preserve);
                Some(boundary)
            }
            Assessment::Unsupported { .. } => None,
        }
    }

    #[test]
    fn every_field_has_a_tested_boundary_in_every_mode() {
        use UpdateBoundary::{Pause, Stop};
        for mode in MODES {
            let mut cases: Vec<(&str, Value, Option<UpdateBoundary>)> = vec![
                ("reconnectBudgetMs", json!(1000), Some(Pause)),
                ("host", json!("db2"), Some(Stop)),
                ("port", json!(3307), Some(Stop)),
                ("user", json!("other"), Some(Stop)),
                ("password", json!("x"), Some(Stop)),
                ("sslMode", json!("require_verify_full"), Some(Stop)),
                ("tlsCaPem", json!("pem"), Some(Stop)),
                ("serverId", json!(42), Some(Stop)),
                ("heartbeatIntervalMs", json!(1000), Some(Stop)),
                ("snapshotLockTimeoutMs", json!(1000), Some(Stop)),
                ("maxTransactionChanges", json!(200_000), Some(Stop)),
                ("maxTransactionChanges", json!(10), None),
                ("maxTransactionBytes", json!(128 * 1024 * 1024), Some(Stop)),
                ("maxTransactionBytes", json!(1024), None),
                ("maxTransactionDurationMs", json!(1000), Some(Stop)),
                ("maxProtocolBytes", json!(1 << 20), Some(Stop)),
                ("ioTimeoutMs", json!(1000), Some(Stop)),
                ("stream", json!("other"), None),
                ("database", json!("other"), None),
                (
                    "tableKeys",
                    json!([{"table": "items", "keyColumns": ["@1"]}]),
                    None,
                ),
            ];
            if !mode.positional() {
                cases.push(("tables", json!(["other"]), None));
            }
            for (field, value, expected) in cases {
                assert_eq!(
                    change(mode, &[(field, value.clone())], false),
                    expected,
                    "{mode:?}: {field} = {value}"
                );
            }
            assert_eq!(
                change(
                    mode,
                    &[
                        ("tlsClientCertPem", json!("cert")),
                        ("tlsClientKeyPem", json!("key"))
                    ],
                    false
                ),
                Some(Stop),
                "{mode:?}: client certificate"
            );
            if mode.tracked() {
                assert_eq!(
                    change(mode, &[("retention", json!("untilProcessed"))], false),
                    None
                );
            }
            if mode.has_persistent() {
                assert_eq!(change(mode, &[("persistent", json!(true))], false), None);
            }
            assert_eq!(
                change(mode, &[], false),
                Some(Pause),
                "no change is in place"
            );
        }
    }

    #[test]
    fn start_fields_restart_only_without_a_committed_cursor() {
        let position = [
            ("startFrom", json!("position")),
            ("startBinlogFile", json!("binlog.000002")),
            ("startBinlogPosition", json!(4)),
        ];
        for mode in MODES {
            let expected_committed = if mode.tracked() {
                None
            } else {
                Some(UpdateBoundary::Stop)
            };
            assert_eq!(
                change(mode, &position, false),
                expected_committed,
                "{mode:?}"
            );
            assert_eq!(
                change(mode, &position, true),
                Some(UpdateBoundary::Stop),
                "{mode:?}"
            );
        }
        assert_eq!(
            change(
                Mode::Transactions,
                &[
                    ("startFrom", json!("snapshot")),
                    ("startBinlogFile", Value::Null),
                    ("startBinlogPosition", Value::Null)
                ],
                true
            ),
            None,
            "switching to a coordinated snapshot is never a restart"
        );
    }
}
