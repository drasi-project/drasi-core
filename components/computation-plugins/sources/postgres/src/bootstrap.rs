// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The source's paired bootstrap: its coordinator opens one provider per
//! query from the source's own identity, stream and settings, so a provider
//! can only create slots under this source's managed prefix and only read
//! the publication this source streams. The bootstrap library's paired
//! factories (`drasi.postgres/coordinated-bootstrap`, ...) take no
//! configuration; the graph pairs them with this source through the
//! declared coordination protocol.

pub(crate) mod coordinator;
pub(crate) mod shared;
pub(crate) mod snapshot;

pub use shared::PostgresSharedSnapshot;
pub use snapshot::PostgresSnapshot;

use crate::config::Mode;
use drasi_computation_contracts::{BootstrapCoordination, BootstrapProtocol};
use drasi_postgres_common::replication::coordination;

/// The coordination protocol of a source in `mode`.
pub fn protocol(mode: Mode) -> BootstrapProtocol {
    coordination::protocol(match mode {
        Mode::Transactions => coordination::COORDINATED_TRANSACTIONS,
        Mode::Rows => coordination::COORDINATED_ROWS,
        Mode::SharedRows => coordination::SHARED_ROWS,
        Mode::SharedTransactions => coordination::SHARED_TRANSACTIONS,
    })
}

/// The source's coordination offer, if any. A single-owner source offers it
/// only with `coordinatedSnapshot: true`, and then requires it: it starts
/// from the slot its paired provider hands over. A shared source always
/// offers it and also accepts an unpaired snapshot.
pub fn offer(mode: Mode, coordinated_snapshot: bool) -> Option<BootstrapCoordination> {
    let required = match mode {
        Mode::Transactions | Mode::Rows if coordinated_snapshot => true,
        Mode::Transactions | Mode::Rows => return None,
        Mode::SharedRows | Mode::SharedTransactions => false,
    };
    Some(BootstrapCoordination {
        protocol: protocol(mode),
        required,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_owner_sources_offer_coordination_only_when_configured() {
        for mode in [Mode::Transactions, Mode::Rows] {
            assert_eq!(offer(mode, false), None);
            let offered = offer(mode, true).expect("offered");
            assert!(
                offered.required,
                "a coordinated source requires its provider"
            );
            assert_eq!(offered.protocol, protocol(mode));
        }
        for mode in [Mode::SharedRows, Mode::SharedTransactions] {
            let offered = offer(mode, false).expect("offered");
            assert!(!offered.required);
        }
        let protocols: std::collections::BTreeSet<_> = [
            Mode::Transactions,
            Mode::Rows,
            Mode::SharedRows,
            Mode::SharedTransactions,
        ]
        .into_iter()
        .map(protocol)
        .collect();
        assert_eq!(protocols.len(), 4, "each mode pairs with its own provider");
    }
}
