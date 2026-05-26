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

//! Metrics inspection operations for DrasiLib
//!
//! This module exposes observability metrics snapshots for queries, reactions,
//! and lifecycle events.

use std::collections::HashMap;

use crate::error::Result;
use crate::lib_core::DrasiLib;
use crate::metrics::{
    LifecycleMetricsSnapshot, QueryOutputMetricsSnapshot, ReactionMetricsSnapshot,
};

impl DrasiLib {
    /// Get per-query output metrics for a specific query.
    ///
    /// # Errors
    /// Returns `DrasiError::ComponentNotFound` if the query does not exist.
    /// Returns `DrasiError::InvalidState` if the system is not initialized.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let metrics = core.get_query_output_metrics("my_query").await?;
    /// println!("Outbox size: {}", metrics.outbox_size);
    /// println!("Sequence advances: {}", metrics.result_seq_advances);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_output_metrics(
        &self,
        query_id: &str,
    ) -> Result<QueryOutputMetricsSnapshot> {
        self.inspection.get_query_output_metrics(query_id).await
    }

    /// Get per-reaction metrics for a specific reaction.
    ///
    /// Returns a map from query_id to its `ReactionMetricsSnapshot`.
    ///
    /// # Errors
    /// Returns `DrasiError::ComponentNotFound` if the reaction does not exist.
    /// Returns `DrasiError::InvalidState` if the system is not initialized.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let metrics = core.get_reaction_metrics("my_reaction").await?;
    /// for (query_id, snapshot) in &metrics {
    ///     println!("  Query '{}': dedup_skip={}", query_id, snapshot.dedup_skip_count);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_metrics(
        &self,
        reaction_id: &str,
    ) -> Result<HashMap<String, ReactionMetricsSnapshot>> {
        self.inspection.get_reaction_metrics(reaction_id).await
    }

    /// Get global lifecycle metrics.
    ///
    /// These track startup rejections, auto-reset completions, and config hash mismatches
    /// across all reactions managed by this instance.
    ///
    /// # Errors
    /// Returns `DrasiError::InvalidState` if the system is not initialized.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let lifecycle = core.get_lifecycle_metrics().await?;
    /// println!("Hash mismatches: {}", lifecycle.hash_mismatch_count);
    /// println!("Auto-reset completions: {}", lifecycle.auto_reset_completions);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_lifecycle_metrics(&self) -> Result<LifecycleMetricsSnapshot> {
        self.inspection.get_lifecycle_metrics().await
    }
}
