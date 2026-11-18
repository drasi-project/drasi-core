// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Query-owned native MySQL initialization.
//!
//! - The coordinated providers pair with a native MySQL source configured
//!   with `startFrom: snapshot` through its declared coordination protocol.
//!   The source's coordinator opens their providers from the source's own
//!   identity, stream, server id, tables, keys and retention, so they take
//!   no configuration.
//! - `drasi.mysql/snapshot` is a standalone repeatable-read snapshot in the
//!   namespace of the source it is connected to; it takes the same flat
//!   settings as the source (see `config`) plus `labels`.
//!
//! Every provider reads only the tables whose label a query declares.

pub mod config;
mod standalone;
#[cfg(test)]
mod tests;
pub use standalone::MySqlStandaloneBootstrapFactory;

use anyhow::{Context, Result};
use async_trait::async_trait;
use config::BootstrapConfig;
use drasi_computation_contracts::{
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ComponentId,
    ComputationBootstrapSnapshot, GraphChangeCodec, ImplementationIdentity, SourceProgressView,
};
use drasi_computation_plugin_sdk::{
    BootstrapCompletionKind, BootstrapFactory, BootstrapFactoryMetadata,
    BootstrapOutputDescription, BootstrapSession, CreateRequest, DurableBootstrapProvider,
    ElementNamespace, NativeBootstrapProvider, NativeSourceProgress, PairedBootstrapFactory,
    PluginDefinition,
};
use drasi_mysql_common::native::coordination;
use std::sync::Arc;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MYSQL_BOOTSTRAP: &str = "drasi.mysql/coordinated-bootstrap";
pub const MYSQL_SHARED_TRANSACTION_BOOTSTRAP: &str = "drasi.mysql/shared-transaction-bootstrap";
pub const MYSQL_SHARED_ROW_BOOTSTRAP: &str = "drasi.mysql/shared-row-bootstrap";
pub const MYSQL_STANDALONE_BOOTSTRAP: &str = "drasi.mysql/snapshot";

fn paired(name: &str, protocol: &str) -> PairedBootstrapFactory {
    PairedBootstrapFactory::new(
        ImplementationIdentity::try_new(name, "1").expect("constant implementation"),
        coordination::protocol(protocol),
    )
}

/// Paired with `drasi.mysql/transactions` (`startFrom: snapshot`).
pub fn coordinated_factory() -> PairedBootstrapFactory {
    paired(MYSQL_BOOTSTRAP, coordination::TRANSACTIONS)
}

/// Paired with `drasi.mysql/shared-transactions` (`startFrom: snapshot`).
pub fn shared_transaction_factory() -> PairedBootstrapFactory {
    paired(
        MYSQL_SHARED_TRANSACTION_BOOTSTRAP,
        coordination::SHARED_TRANSACTIONS,
    )
}

/// Paired with `drasi.mysql/shared-changes` (`startFrom: snapshot`).
pub fn shared_row_factory() -> PairedBootstrapFactory {
    paired(MYSQL_SHARED_ROW_BOOTSTRAP, coordination::SHARED_ROWS)
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new_with_bootstrap(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        Vec::new(),
        vec![
            Arc::new(coordinated_factory()),
            Arc::new(shared_transaction_factory()),
            Arc::new(shared_row_factory()),
            Arc::new(MySqlStandaloneBootstrapFactory),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());
