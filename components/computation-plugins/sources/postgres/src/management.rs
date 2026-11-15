// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Update planning.
//!
//! Every field is classified by the weakest boundary at which a change is
//! safe:
//!
//! - **InPlace (Pause):** `reconnectBudgetMs`, read only when a connection
//!   fails.
//! - **Restart (Stop):** connection, TLS, timing and protocol limits. The
//!   next start reopens replication at the committed cursor; the binding
//!   check refuses a server or database that is not the same system.
//!   Transaction limits may only grow, so a retained transaction can never
//!   become unpublishable.
//! - **Unsupported:** anything the committed cursor is bound to (stream,
//!   database, publication, tables, keys, slot ownership and name, mode
//!   flags). Remove the source with erase and add it again; a replacement
//!   would need a rebuild of every downstream query, which this source does
//!   not claim to coordinate.
//! - **`startLsn`:** Restart while no checkpoint exists (the live object
//!   checks), otherwise unsupported.

use crate::config::{Mode, SourceConfig};
use anyhow::{Context, Result};
use drasi_computation_contracts::ComponentUpdateData;
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, UpdateBoundary},
    update_planning::{ensure_same_component, BoundaryPlanner},
    UpdateRequest,
};

/// Fingerprint naming the live fact a `startLsn` change relies on.
pub(crate) const NO_CHECKPOINT: &str = "postgres:no-committed-cursor";

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
    let bound = [
        ("stream", current.stream != proposed.stream),
        ("database", c.database != p.database),
        ("publication", c.publication != p.publication),
        ("tables", c.tables != p.tables),
        ("tableKeys", c.table_keys != p.table_keys),
        ("slotOwnership", c.slot_ownership != p.slot_ownership),
        ("slotName", c.slot_name != p.slot_name),
        (
            "coordinatedSnapshot",
            current.coordinated_snapshot != proposed.coordinated_snapshot,
        ),
        ("persistent", current.persistent != proposed.persistent),
        (
            "maxTransactionChanges",
            p.max_transaction_changes < c.max_transaction_changes,
        ),
        (
            "maxTransactionBytes",
            p.max_transaction_bytes < c.max_transaction_bytes,
        ),
        ("startLsn", c.start_lsn != p.start_lsn && !uncommitted),
    ];
    let refused: Vec<_> = bound
        .iter()
        .filter(|(_, changed)| *changed)
        .map(|(name, _)| *name)
        .collect();
    if !refused.is_empty() {
        return Assessment::Unsupported {
            reason: format!(
                "{} cannot change for an existing PostgreSQL source (transaction limits may only grow); remove the source with erase and add it again",
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
    planner.field(
        "feedbackIntervalMs",
        c.feedback_interval_ms != p.feedback_interval_ms,
        Stop,
    );
    let start_changed = c.start_lsn != p.start_lsn;
    planner.field("startLsn", start_changed, Stop);
    let assessment = planner.assessment();
    match assessment {
        Assessment::Supported { boundary, data, .. } if start_changed => Assessment::Supported {
            boundary,
            data,
            fingerprint: Some(NO_CHECKPOINT.into()),
        },
        other => other,
    }
}

/// Factory-level planning, without live facts: a `startLsn` change is
/// refused because the factory cannot see whether a cursor was committed.
pub(crate) fn assess(mode: Mode, request: &UpdateRequest) -> Result<Assessment> {
    let (current, proposed) = configurations(mode, request)?;
    Ok(plan(&current, &proposed, false))
}

/// Whether two assessments name the same supported plan.
pub(crate) fn same_plan(left: &Assessment, boundary: UpdateBoundary) -> bool {
    matches!(
        left,
        Assessment::Supported { boundary: b, data: ComponentUpdateData::Preserve, .. } if *b == boundary
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn base() -> Value {
        json!({"stream": "changes", "database": "app", "user": "drasi", "publication": "pub"})
    }

    fn assess_change(mode: Mode, field: &str, value: Value, uncommitted: bool) -> Assessment {
        let current = SourceConfig::from_value(mode, &base()).unwrap();
        let mut proposed_value = base();
        proposed_value[field] = value;
        let proposed = SourceConfig::from_value(mode, &proposed_value).unwrap();
        plan(&current, &proposed, uncommitted)
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

    #[test]
    fn every_field_has_a_tested_boundary() {
        use UpdateBoundary::{Pause, Stop};
        let mode = Mode::Rows;
        let cases: Vec<(&str, Value, Option<UpdateBoundary>)> = vec![
            ("reconnectBudgetMs", json!(1000), Some(Pause)),
            ("host", json!("db2"), Some(Stop)),
            ("port", json!(5433), Some(Stop)),
            ("user", json!("other"), Some(Stop)),
            ("password", json!("x"), Some(Stop)),
            ("sslMode", json!("require"), Some(Stop)),
            ("tlsCaPem", json!("pem"), Some(Stop)),
            ("maxTransactionChanges", json!(200_000), Some(Stop)),
            ("maxTransactionChanges", json!(10), None),
            ("maxTransactionBytes", json!(128 * 1024 * 1024), Some(Stop)),
            ("maxTransactionBytes", json!(1024), None),
            ("maxTransactionDurationMs", json!(1000), Some(Stop)),
            ("maxProtocolBytes", json!(1 << 20), Some(Stop)),
            ("ioTimeoutMs", json!(1000), Some(Stop)),
            ("feedbackIntervalMs", json!(1000), Some(Stop)),
            ("stream", json!("other"), None),
            ("database", json!("other"), None),
            ("publication", json!("other"), None),
            ("tables", json!(["t"]), None),
            (
                "tableKeys",
                json!([{"table": "t", "keyColumns": ["id"]}]),
                None,
            ),
            ("persistent", json!(true), None),
            ("startLsn", json!("0/1"), None),
        ];
        for (field, value, expected) in cases {
            assert_eq!(
                boundary(&assess_change(mode, field, value.clone(), false)),
                expected,
                "{field} = {value}"
            );
        }
        let mut external = base();
        external["slotOwnership"] = json!("external");
        external["slotName"] = json!("ops");
        let current = SourceConfig::from_value(mode, &base()).unwrap();
        let proposed = SourceConfig::from_value(mode, &external).unwrap();
        assert_eq!(boundary(&plan(&current, &proposed, false)), None);
        assert_eq!(
            boundary(&plan(&current, &current, false)),
            Some(Pause),
            "no change is applied in place"
        );
    }

    #[test]
    fn start_lsn_is_a_restart_only_before_the_first_checkpoint() {
        let before = assess_change(Mode::Transactions, "startLsn", json!("0/1"), true);
        assert_eq!(boundary(&before), Some(UpdateBoundary::Stop));
        assert!(matches!(
            before,
            Assessment::Supported { fingerprint: Some(ref f), .. } if f == NO_CHECKPOINT
        ));
        assert_eq!(
            boundary(&assess_change(
                Mode::Transactions,
                "startLsn",
                json!("0/1"),
                false
            )),
            None
        );
    }
}
