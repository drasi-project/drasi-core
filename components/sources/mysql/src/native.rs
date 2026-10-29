// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native Rust adapters over the shared, directly owned MySQL protocol engine.

use anyhow::{Context, Result};
#[cfg(test)]
use drasi_lib::computation::v1::SourceTransactionLimits;
#[cfg(test)]
use std::{
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    time::Duration,
};

use drasi_mysql_common::native::{binlog, catalog, connection, file_number};
pub use drasi_mysql_common::native::{
    MySqlConfig, MySqlConnectionConfig, MySqlOutput, MySqlRetention, MySqlServerError,
    MySqlStartPosition, MySqlTlsMode,
};

mod factory;
mod snapshot;
mod source;
#[cfg(test)]
mod tests;
pub use factory::MySqlSourceFactory;
pub use snapshot::MySqlSnapshot;
pub use source::MySqlSource;
