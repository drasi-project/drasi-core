// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The destination-cursor delivery protocol of the native stored-procedure
//! sinks (PostgreSQL, MySQL, SQL Server), implemented once.
//!
//! * [`cursor`]: the format-4 cursor row, its owner digest, the progress and
//!   dead-letter table names and the pure per-operation decision.
//! * [`driver`]: the per-operation transaction over a database
//!   [`driver::Session`], the commit-outcome and transaction-control rules,
//!   the single retry model and the atomic skip of the declared
//!   `onPermanentFailure: skip` policy.
//! * [`context`]: the template context (including `deliveryId`).
//! * [`settings`]: configuration rules every sink applies the same way.
//!
//! Each database implements only `Session` (SQL and error classification).

pub mod context;
pub mod cursor;
pub mod driver;
pub mod settings;

#[cfg(any(test, feature = "conformance"))]
pub mod conformance;

pub use context::OperationContext;
pub use cursor::{owner, CursorError, CursorRow, Position, ProgressTable, PROGRESS_FORMAT_VERSION};
pub use driver::{
    CommitError, Connector, Delivery, ErrorClass, EscapedEffect, Outcome, Session, SessionLost,
    TransactionEnded,
};
