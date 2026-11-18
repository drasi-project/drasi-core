// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Source/bootstrap coordination protocols shared by the native PostgreSQL
//! source (which declares and implements them) and the bootstrap library
//! (whose paired factories require them). The graph refuses a paired
//! provider on a source that does not declare the same protocol.

use drasi_computation_contracts::BootstrapProtocol;

/// `drasi/postgres-transactions` with `coordinatedSnapshot: true` and
/// `drasi.postgres/coordinated-bootstrap`.
pub const COORDINATED_TRANSACTIONS: &str = "drasi.postgres/coordinated-transactions";
/// `drasi.postgres/changes` with `coordinatedSnapshot: true` and
/// `drasi.postgres/coordinated-row-bootstrap`.
pub const COORDINATED_ROWS: &str = "drasi.postgres/coordinated-rows";
/// `drasi.postgres/shared-changes` and `drasi.postgres/shared-row-bootstrap`.
pub const SHARED_ROWS: &str = "drasi.postgres/shared-rows";
/// `drasi.postgres/shared-transactions` and
/// `drasi.postgres/shared-transaction-bootstrap`.
pub const SHARED_TRANSACTIONS: &str = "drasi.postgres/shared-transactions";
/// Version of every PostgreSQL coordination protocol.
pub const VERSION: u32 = 1;

/// The versioned protocol named `name` (one of the constants above).
pub fn protocol(name: &str) -> BootstrapProtocol {
    BootstrapProtocol::new(name, VERSION)
}
