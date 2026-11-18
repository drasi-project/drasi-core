// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The coordinated providers are opened from the paired journal source's
//! coordinator, which snapshots with the source's own path, tables and keys.
//! They take no configuration: the graph pairs them with the source whose
//! bootstrap input they are connected to, and refuses that edge unless the
//! source declares the same coordination protocol (a journal source with
//! `coordinatedSnapshot: true`).

use drasi_computation_contracts::ImplementationIdentity;
use drasi_computation_plugin_sdk::PairedBootstrapFactory;
use drasi_sqlite_common::native::coordination_protocol;

pub const COORDINATED_BOOTSTRAP: &str = "drasi.sqlite/coordinated-bootstrap";
pub const SHARED_BOOTSTRAP: &str = "drasi.sqlite/shared-bootstrap";

fn factory(name: &str, shared: bool) -> PairedBootstrapFactory {
    PairedBootstrapFactory::new(
        ImplementationIdentity::try_new(name, "1").expect("constant implementation"),
        coordination_protocol(shared),
    )
}

/// The provider paired with a `drasi.sqlite/replay-transactions` source.
pub fn coordinated_factory() -> PairedBootstrapFactory {
    factory(COORDINATED_BOOTSTRAP, false)
}

/// The provider paired with a `drasi.sqlite/shared-transactions` source.
pub fn shared_factory() -> PairedBootstrapFactory {
    factory(SHARED_BOOTSTRAP, true)
}
