// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native component messages and contracts shared by the runtime and plugins.
//! This crate contains no graph executor, query evaluator, parser or storage backend.

mod admission_messages;
#[doc(hidden)]
pub mod binary_codec;
mod bootstrap;
#[doc(hidden)]
pub mod codec;
mod component_data;
#[doc(hidden)]
pub mod delivery;
#[doc(hidden)]
pub mod output_identity;
pub use admission_messages::*;
#[doc(hidden)]
pub mod data;
#[doc(hidden)]
pub mod envelope;
#[doc(hidden)]
pub mod error;
pub mod exchange;
#[doc(hidden)]
pub mod graph_codec;
mod identity;
mod internal;
#[doc(hidden)]
pub mod ports;
mod producer_identity;
mod progress;
#[doc(hidden)]
pub mod query_codec;
mod query_identity;
mod snapshot;
#[doc(hidden)]
pub mod source_transaction;
pub mod workers;

pub use binary_codec::*;
pub use codec::*;
pub use component_data::*;
pub use data::*;
pub use drasi_core_types::{profiling, schema};
pub use envelope::*;
pub use error::{ContractError, Result};
pub use graph_codec::*;
pub use identity::*;
pub use ports::*;
pub use progress::*;
pub use query_codec::*;
pub use snapshot::*;
pub use source_transaction::*;

/// Maximum opaque source cursor size, shared by plugin and runtime validation.
pub const MAX_SOURCE_POSITION_BYTES: usize = 65_536;
pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 16 * 1024;
pub const MAX_CONTROL_JSON_DEPTH: usize = 64;

pub use producer_identity::{
    has_distinct_producer_lineage, GraphProducerIdentity, GraphProducerProgress,
};
pub use query_identity::*;

pub use bootstrap::*;
pub use delivery::{
    DeliveryBatch, DeliveryBatchIdentity, DeliveryHandler, DeliveryId, DeliveryItem,
    DeliveryPosition, DeliveryScope,
};
pub use output_identity::ReplayRejection;
