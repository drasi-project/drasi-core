// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Ordinary query APIs and the result-reading types used by existing consumers.
//!
//! Use [`crate::Query`] to configure a query and [`crate::DrasiLib`] to operate it.
//! The manager delegates to the instance graph's native query component.
//! [`crate::queries::SnapshotResponse`], [`crate::queries::OutboxResponse`] and
//! [`crate::queries::FetchError`] remain active
//! read contracts. Query execution and recovery state belong to the instance
//! graph, not separate query base/state/dedup utilities.

pub mod label_extractor;
pub mod manager;
pub mod output_state;
pub mod priority_queue;
pub mod subscription_builder;
pub mod traits;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod joins_test;

#[cfg(test)]
mod checkpoint_tests;

#[cfg(test)]
mod e2e_checkpoint_tests;

pub use label_extractor::*;
pub use manager::*;
pub use output_state::{
    output_epoch_hash, FetchError, KeyedSnapshotRow, OutboxGap, OutboxResponse, OutboxStream,
    QueryResultSubscription, SnapshotResponse, SnapshotStream,
};
pub use priority_queue::*;
pub use subscription_builder::*;
pub use traits::Query;
