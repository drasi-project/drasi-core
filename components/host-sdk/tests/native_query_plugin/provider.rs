// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::Scope;
use drasi_host_sdk::computation::{NativeBootstrapProxy, NativeBootstrapResource};
use std::path::Path;

const INSTANCE: &str = "native-middleware-bootstrap";

#[path = "middleware_bootstrap.rs"]
mod middleware;

pub(crate) fn bootstrap_library() -> PathBuf {
    library().with_file_name(format!(
        "{}drasi_computation_bootstrap_sqlite{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

pub(crate) fn create_database(path: &Path) -> Result<()> {
    let connection = rusqlite::Connection::open(path)?;
    connection.execute_batch(
        "CREATE TABLE items (id INTEGER PRIMARY KEY, amount INTEGER NOT NULL);
         INSERT INTO items VALUES (1, 10), (2, 20);",
    )?;
    Ok(())
}

/// The session a file snapshot serves outside a graph: the source and
/// stream its connection to a source's bootstrap input would supply.
pub(crate) fn provider_session(
    source: &str,
    stream: &str,
) -> Result<drasi_computation_plugin_sdk::BootstrapSession> {
    Ok(drasi_computation_plugin_sdk::BootstrapSession::new(
        Some(drasi_computation_plugin_sdk::ServedSource {
            source: ComponentId::try_new(source)?,
            stream: StreamId::try_new(stream)?,
        }),
        None,
    ))
}

pub(crate) const PROVIDER_SOURCE: &str = "ba51bf8f-7ec0-52a4-a2ff-37a49d49c710";

pub(crate) fn provider_configuration(path: &Path) -> serde_json::Value {
    json!({
        "path": path,
        "tables": ["items"],
        "maxTables": 4,
        "maxBytes": 65536,
        "maxDurationMs": 5000,
        "outputCapacity": 1,
        "shutdownTimeoutMs": 5000
    })
}
