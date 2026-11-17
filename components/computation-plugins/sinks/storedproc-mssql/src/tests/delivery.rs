// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! SQL Server error classification. The cursor protocol itself is the shared
//! one (`drasi_computation_storedproc_common`); its protocol table runs
//! against the in-memory session there and against every live backend.

use crate::delivery::{classify, classify_server_code, ConnectTimeout, FailureClass};
use drasi_computation_storedproc_common::{driver::ErrorClass, owner};
use std::time::Duration;

fn io() -> anyhow::Error {
    std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset").into()
}

#[test]
fn owner_is_the_shared_digest_of_graph_and_component() {
    use sha2::{Digest, Sha256};
    let expected = format!(
        "{:x}",
        Sha256::digest(format!(
            "drasi.storedproc/owner/v1\0{}\0{}",
            super::GRAPH,
            super::SINK_ID
        ))
    );
    assert_eq!(owner(super::GRAPH, super::SINK_ID), expected);
}

#[test]
fn errors_are_classified_by_number_and_type_never_by_text() {
    for code in [1205, 1222, 3960, 3961, 41301, 41302, 41305, 41325, 59_101] {
        assert_eq!(
            classify_server_code(code),
            FailureClass::Retryable { reconnect: false },
            "{code}"
        );
    }
    for code in [
        233, 64, 4221, 10053, 10054, 10060, 10928, 10929, 40197, 40501, 40613, 49918, 49919, 49920,
    ] {
        assert_eq!(
            classify_server_code(code),
            FailureClass::Retryable { reconnect: true },
            "{code}"
        );
    }
    // Procedure THROW/RAISERROR, constraint and key violations, permission,
    // login, missing object and syntax errors are permanent.
    for code in [
        50000, 51000, 547, 2627, 2601, 229, 262, 18456, 2812, 102, 8144,
    ] {
        assert_eq!(
            classify_server_code(code),
            FailureClass::Permanent,
            "{code}"
        );
    }
    let reconnect = ErrorClass::Transient { reconnect: true };
    assert_eq!(classify(&io()), reconnect);
    assert_eq!(classify(&io().context("wrapped with context")), reconnect);
    assert_eq!(
        classify(&anyhow::Error::from(tiberius::error::Error::Protocol(
            "desync".into()
        ))),
        reconnect
    );
    assert_eq!(
        classify(&anyhow::Error::from(tiberius::error::Error::Tls(
            "bad certificate".into()
        ))),
        ErrorClass::Permanent
    );
    assert_eq!(
        classify(&anyhow::Error::new(ConnectTimeout(Duration::from_secs(1)))),
        reconnect
    );
    assert_eq!(
        classify(&anyhow::anyhow!(
            "deadlock victim 1205 timeout connection reset"
        )),
        ErrorClass::Permanent,
        "text is never classified"
    );
    assert_eq!(
        classify(&anyhow::Error::from(
            drasi_mssql_common::sql::ParameterLimitExceeded { count: 2099 }
        )),
        ErrorClass::Permanent
    );
}
