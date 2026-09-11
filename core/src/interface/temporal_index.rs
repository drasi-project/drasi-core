// Copyright 2026 The Drasi Authors.
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

use async_trait::async_trait;

use crate::evaluation::temporal::{
    QueryEpoch, TemporalBatch, TemporalCatalog, TemporalKey, TemporalRecord,
};

use super::IndexError;

/// Retained temporal state in the graph/result/queue transaction domain.
///
/// Persistent implementations require an active session for writes and expose its staged
/// writes to reads. A successful `apply` stages a batch; it does not commit a root.
/// Memory applies immediately and requires the root's dirty-abort rebuild fence.
/// The runtime must serialize read/modify/write sequences through the query's root lease.
/// At startup, load the catalog to discover the namespace, then read `TemporalKey::Epoch`.
/// Discovery keys are independent of codec and plan versions, so incompatible persisted
/// state produces an error rather than a cache miss.
///
/// Do not write records for parts without temporal effects. Removing an epoch is an explicit
/// history reset, not a recovery fallback. Clear its queue and coordinate checkpoints/results
/// in the same root. Never reuse a cleared epoch for another lifetime.
#[async_trait]
pub trait TemporalIndex: Send + Sync {
    async fn load_catalog(&self) -> Result<Option<TemporalCatalog>, IndexError>;
    async fn store_catalog(&self, catalog: TemporalCatalog) -> Result<(), IndexError>;
    async fn get(&self, key: &TemporalKey) -> Result<Option<TemporalRecord>, IndexError>;
    async fn apply(&self, batch: TemporalBatch) -> Result<(), IndexError>;
    /// Remove this epoch's records without changing the query catalog.
    async fn clear_epoch(&self, epoch: QueryEpoch) -> Result<(), IndexError>;
    /// Remove all temporal namespaces and the catalog without decoding their payloads.
    async fn clear(&self) -> Result<(), IndexError>;
}
