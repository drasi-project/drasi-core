// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Update planning.
//!
//! Every field is classified by the weakest boundary at which a change is
//! safe; the plan takes the strongest boundary among the changed fields.
//!
//! - **InPlace (Pause):** `authToken`, `authTokenSecondary`. The REST
//!   listener reads the accepted tokens per request, so rotating a token
//!   needs no restart.
//! - **Restart (Stop):** everything bound when the worker and listener start:
//!   limits, capacities, timeouts, `walMode`, `initSql` and every other REST
//!   field. In the journal factories, `maxTransactionChanges`,
//!   `maxTransactionBytes`, `maxJournalTransactions` and `maxJournalBytes`
//!   may only grow, so retained journal records always stay replayable.
//! - **Unsupported:** `stream`, `path`, `tables`, `tableKeys` and
//!   `coordinatedSnapshot` identify the captured data and, in the journal
//!   factories, the journal binding. Remove the source (with erase for a
//!   journal factory) and add it again. A coordinated source must match its
//!   source/bootstrap coordinator exactly, so every change to it is
//!   unsupported.
//!
//! The plan is pure: configuration only, with no I/O, clock or live state.

use crate::config::{Mode, SourceConfig};
use anyhow::{Context, Result};
use drasi_computation_contracts::ComponentUpdateData;
use drasi_computation_plugin_sdk::{
    lifecycle::{Assessment, UpdateBoundary},
    update_planning::{ensure_same_component, BoundaryPlanner},
    UpdateRequest,
};

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

/// Pure planning from two validated configurations.
pub(crate) fn plan(current: &SourceConfig, proposed: &SourceConfig) -> Assessment {
    use UpdateBoundary::{Pause, Stop};
    let (c, p) = (&current.settings, &proposed.settings);
    let journal = current.mode.journal();
    if current.coordinated_snapshot && current != proposed {
        return Assessment::Unsupported {
            reason: "a coordinated SQLite source must match its source/bootstrap coordinator; remove it with erase and add it again".into(),
        };
    }
    let bound = [
        ("stream", current.stream != proposed.stream),
        ("path", c.path != p.path),
        ("tables", c.tables != p.tables),
        ("tableKeys", c.table_keys != p.table_keys),
        (
            "coordinatedSnapshot",
            current.coordinated_snapshot != proposed.coordinated_snapshot,
        ),
        (
            "maxTransactionChanges",
            journal && p.max_transaction_changes < c.max_transaction_changes,
        ),
        (
            "maxTransactionBytes",
            journal && p.max_transaction_bytes < c.max_transaction_bytes,
        ),
        (
            "maxJournalTransactions",
            proposed.max_journal_transactions < current.max_journal_transactions,
        ),
        (
            "maxJournalBytes",
            proposed.max_journal_bytes < current.max_journal_bytes,
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
                "{} cannot change for an existing SQLite source (journal limits may only grow); remove the source{} and add it again",
                refused.join(", "),
                if journal { " with erase" } else { "" },
            ),
        };
    }
    let mut planner = BoundaryPlanner::default();
    planner.field("authToken", c.auth_token != p.auth_token, Pause);
    planner.field(
        "authTokenSecondary",
        c.auth_token_secondary != p.auth_token_secondary,
        Pause,
    );
    // Enabling or removing authentication changes the listener's exposure
    // check and the /api/sql requirement, so it restarts.
    planner.field(
        "authentication",
        c.auth_token.is_some() != p.auth_token.is_some(),
        Stop,
    );
    for (field, changed) in [
        (
            "maxTransactionChanges",
            c.max_transaction_changes != p.max_transaction_changes,
        ),
        (
            "maxTransactionBytes",
            c.max_transaction_bytes != p.max_transaction_bytes,
        ),
        (
            "maxTransactionDurationMs",
            c.max_transaction_duration_ms != p.max_transaction_duration_ms,
        ),
        ("maxSqlBytes", c.max_sql_bytes != p.max_sql_bytes),
        ("commandCapacity", c.command_capacity != p.command_capacity),
        ("outputCapacity", c.output_capacity != p.output_capacity),
        (
            "shutdownTimeoutMs",
            c.shutdown_timeout_ms != p.shutdown_timeout_ms,
        ),
        ("walMode", c.wal_mode != p.wal_mode),
        ("initSql", c.init_sql != p.init_sql),
        ("restApi", c.rest_api != p.rest_api),
        ("restHost", c.rest_host != p.rest_host),
        ("restPort", c.rest_port != p.rest_port),
        (
            "allowUnauthenticated",
            c.allow_unauthenticated != p.allow_unauthenticated,
        ),
        (
            "tlsCertificatePem",
            c.tls_certificate_pem != p.tls_certificate_pem,
        ),
        (
            "tlsPrivateKeyPem",
            c.tls_private_key_pem != p.tls_private_key_pem,
        ),
        ("tlsClientCaPem", c.tls_client_ca_pem != p.tls_client_ca_pem),
        (
            "maxRequestBytes",
            c.max_request_bytes != p.max_request_bytes,
        ),
        (
            "requestTimeoutMs",
            c.request_timeout_ms != p.request_timeout_ms,
        ),
        ("maxConnections", c.max_connections != p.max_connections),
        ("pageSize", c.page_size != p.page_size),
        ("sqlApi", c.sql_api != p.sql_api),
        (
            "maxJournalTransactions",
            current.max_journal_transactions != proposed.max_journal_transactions,
        ),
        (
            "maxJournalBytes",
            current.max_journal_bytes != proposed.max_journal_bytes,
        ),
    ] {
        planner.field(field, changed, Stop);
    }
    planner.assessment()
}

/// Factory-level planning.
pub(crate) fn assess(mode: Mode, request: &UpdateRequest) -> Result<Assessment> {
    let (current, proposed) = configurations(mode, request)?;
    Ok(plan(&current, &proposed))
}

/// Whether `assessment` is a supported, data-preserving plan at `boundary`.
pub(crate) fn supported_at(assessment: &Assessment, boundary: UpdateBoundary) -> bool {
    matches!(
        assessment,
        Assessment::Supported { boundary: b, data: ComponentUpdateData::Preserve, .. } if *b == boundary
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const TOKEN: &str = "0123456789abcdef";

    fn base(mode: Mode) -> Value {
        let mut value = json!({"stream": "sqlite/out", "path": "app.db", "tables": ["items"]});
        if mode == Mode::Changes {
            value["authToken"] = json!(TOKEN);
        }
        value
    }

    fn assess_change(mode: Mode, field: &str, value: Value) -> Option<UpdateBoundary> {
        let current = SourceConfig::from_value(mode, &base(mode)).unwrap();
        let mut proposed_value = base(mode);
        proposed_value[field] = value;
        let proposed = SourceConfig::from_value(mode, &proposed_value)
            .unwrap_or_else(|error| panic!("{mode:?} {field}: {error:#}"));
        boundary(&plan(&current, &proposed))
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
        let common: Vec<(&str, Value, Option<UpdateBoundary>)> = vec![
            ("stream", json!("other"), None),
            ("path", json!("other.db"), None),
            ("tables", json!(["other"]), None),
            (
                "tableKeys",
                json!([{"table": "items", "keyColumns": ["id"]}]),
                None,
            ),
            ("maxTransactionDurationMs", json!(1000), Some(Stop)),
            ("maxSqlBytes", json!(4096), Some(Stop)),
            ("commandCapacity", json!(8), Some(Stop)),
            ("outputCapacity", json!(2), Some(Stop)),
            ("shutdownTimeoutMs", json!(1000), Some(Stop)),
            (
                "initSql",
                json!("CREATE TABLE IF NOT EXISTS t(id)"),
                Some(Stop),
            ),
            ("restApi", json!(true), Some(Stop)),
            ("restHost", json!("::1"), Some(Stop)),
            ("restPort", json!(9000), Some(Stop)),
            ("allowUnauthenticated", json!(true), Some(Stop)),
            ("tlsClientCaPem", json!("pem"), None),
            ("maxRequestBytes", json!(1024), Some(Stop)),
            ("requestTimeoutMs", json!(1000), Some(Stop)),
            ("maxConnections", json!(4), Some(Stop)),
            ("pageSize", json!(10), Some(Stop)),
        ];
        for mode in [
            Mode::Changes,
            Mode::Transactions,
            Mode::Replay,
            Mode::Shared,
        ] {
            for (field, value, expected) in &common {
                if *field == "tlsClientCaPem" {
                    // Needs a certificate and key, so the proposal is invalid.
                    let mut proposed = base(mode);
                    proposed[*field] = value.clone();
                    assert!(SourceConfig::from_value(mode, &proposed).is_err());
                    continue;
                }
                assert_eq!(
                    assess_change(mode, field, value.clone()),
                    *expected,
                    "{mode:?} {field} = {value}"
                );
            }
        }
        let volatile: Vec<(&str, Value, Option<UpdateBoundary>)> = vec![
            ("maxTransactionChanges", json!(20_000), Some(Stop)),
            ("maxTransactionChanges", json!(5_000), Some(Stop)),
            ("maxTransactionBytes", json!(1 << 25), Some(Stop)),
            ("maxTransactionBytes", json!(1 << 20), Some(Stop)),
            ("walMode", json!(true), Some(Stop)),
            ("authToken", json!("fedcba9876543210"), Some(Pause)),
            ("authTokenSecondary", json!("fedcba9876543210"), Some(Pause)),
            ("sqlApi", json!(true), None),
        ];
        for (field, value, expected) in &volatile {
            if *field == "sqlApi" {
                // sqlApi needs restApi as well: a combined change restarts.
                let current =
                    SourceConfig::from_value(Mode::Changes, &base(Mode::Changes)).unwrap();
                let mut proposed = base(Mode::Changes);
                proposed["sqlApi"] = json!(true);
                proposed["restApi"] = json!(true);
                let proposed = SourceConfig::from_value(Mode::Changes, &proposed).unwrap();
                assert_eq!(boundary(&plan(&current, &proposed)), Some(Stop));
                continue;
            }
            assert_eq!(
                assess_change(Mode::Changes, field, value.clone()),
                *expected,
                "{field} = {value}"
            );
        }
        // Adding or removing authentication restarts.
        let mut unauthenticated = base(Mode::Changes);
        unauthenticated.as_object_mut().unwrap().remove("authToken");
        let current = SourceConfig::from_value(Mode::Changes, &base(Mode::Changes)).unwrap();
        let proposed = SourceConfig::from_value(Mode::Changes, &unauthenticated).unwrap();
        assert_eq!(boundary(&plan(&current, &proposed)), Some(Stop));
        let journal: Vec<(&str, Value, Option<UpdateBoundary>)> = vec![
            ("maxTransactionChanges", json!(20_000), Some(Stop)),
            ("maxTransactionChanges", json!(5_000), None),
            ("maxTransactionBytes", json!(1 << 25), Some(Stop)),
            ("maxTransactionBytes", json!(1 << 20), None),
            ("maxJournalTransactions", json!(20_000), Some(Stop)),
            ("maxJournalTransactions", json!(10), None),
            ("maxJournalBytes", json!(1u64 << 30), Some(Stop)),
            ("maxJournalBytes", json!(1024), None),
            ("coordinatedSnapshot", json!(true), None),
        ];
        for mode in [Mode::Replay, Mode::Shared] {
            for (field, value, expected) in &journal {
                assert_eq!(
                    assess_change(mode, field, value.clone()),
                    *expected,
                    "{mode:?} {field} = {value}"
                );
            }
        }
        let same = SourceConfig::from_value(Mode::Changes, &base(Mode::Changes)).unwrap();
        assert_eq!(
            boundary(&plan(&same, &same)),
            Some(Pause),
            "no change is in place"
        );
    }

    #[test]
    fn tls_fields_restart() {
        let first = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let second = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut value = base(Mode::Changes);
        value["tlsCertificatePem"] = json!(first.cert.pem());
        value["tlsPrivateKeyPem"] = json!(first.key_pair.serialize_pem());
        let current = SourceConfig::from_value(Mode::Changes, &value).unwrap();
        let mut rotated = value.clone();
        rotated["tlsCertificatePem"] = json!(second.cert.pem());
        rotated["tlsPrivateKeyPem"] = json!(second.key_pair.serialize_pem());
        let proposed = SourceConfig::from_value(Mode::Changes, &rotated).unwrap();
        assert_eq!(
            boundary(&plan(&current, &proposed)),
            Some(UpdateBoundary::Stop)
        );
        let mut mutual = value.clone();
        mutual["tlsClientCaPem"] = json!(second.cert.pem());
        let proposed = SourceConfig::from_value(Mode::Changes, &mutual).unwrap();
        assert_eq!(
            boundary(&plan(&current, &proposed)),
            Some(UpdateBoundary::Stop)
        );
    }

    #[test]
    fn coordinated_sources_refuse_every_change() {
        let mut value = base(Mode::Replay);
        value["coordinatedSnapshot"] = json!(true);
        let current = SourceConfig::from_value(Mode::Replay, &value).unwrap();
        assert_eq!(
            boundary(&plan(&current, &current)),
            Some(UpdateBoundary::Pause)
        );
        value["maxSqlBytes"] = json!(4096);
        let proposed = SourceConfig::from_value(Mode::Replay, &value).unwrap();
        assert_eq!(boundary(&plan(&current, &proposed)), None);
    }
}
