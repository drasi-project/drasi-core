// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native SQL Server CDC engine shared by the native source and its
//! bootstrap providers: flat settings, catalog and binding, value mapping,
//! bounded paging over a [`CdcSession`], snapshots and owned barrier
//! markers.

pub mod catalog;
mod cursor;
mod engine;
pub mod error;
pub mod identifiers;
pub mod markers;
#[cfg(any(test, feature = "test-support"))]
pub mod memory;
pub mod paging;
pub mod session;
pub mod settings;
mod snapshot;
pub mod sql_session;
pub mod values;
mod work;

pub use catalog::{describe_schema, Catalog, Column, Table};
pub use cursor::{Cursor, CursorComparator};
pub use engine::{element, element_size, Change, Poll};
pub use identifiers::{quoted, TableName};
pub use paging::{PageLimits, Poller};
pub use session::{CdcConnector, CdcSession};
pub use settings::{NativeSettings, StartPosition, TableKey};
pub use snapshot::{start_snapshot, SnapshotRows, SnapshotRun};
pub use sql_session::{TiberiusConnector, TiberiusSession};
pub use work::Work;
