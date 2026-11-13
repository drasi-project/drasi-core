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
//! Per-component `ComponentName` selectors are exact current names; use the paired
//! `_by_id` methods for explicit [`crate::config::ComponentId`] selection.

use std::collections::HashMap;

use crate::config::ComponentId;
use crate::error::Result;
use crate::lib_core::DrasiLib;
use crate::metrics::{
    LifecycleMetricsSnapshot, QueryOutputMetricsSnapshot, ReactionMetricsSnapshot,
};
use crate::ComponentName;

impl DrasiLib {
    /// Read committed input and produced-output progress for ready queries.
    ///
    /// This bounded, synchronous observation performs no storage or network I/O
    /// and is suitable for OpenTelemetry observable-instrument callbacks.
    /// Stopped, unavailable and unready queries are omitted, never reported as
    /// zero. Component generation/operation and output generation distinguish
    /// replacement, lifecycle changes and output resets.
    ///
    /// These values do not prove delivery or completion of scheduled work.
    pub fn get_query_progress_metrics(
        &self,
    ) -> Result<Vec<crate::metrics::QueryProgressMetricsSnapshot>> {
        self.state_guard.require_initialized()?;
        self.computation_runtime
            .query_progress_metrics()
            .map_err(|error| {
                crate::DrasiError::operation_failed(
                    "instance",
                    crate::error::OperationSubject::Instance(self.config.id.clone()),
                    "get_query_progress_metrics",
                    error.to_string(),
                )
                .with_cause(error)
            })
    }

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
    /// let metrics = core.get_query_output_metrics_by_name(&drasi_lib::ComponentName::try_new("my_query")?).await?;
    /// println!("Outbox size: {}", metrics.outbox_size);
    /// println!("Sequence advances: {}", metrics.result_seq_advances);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_output_metrics_by_name(
        &self,
        query_name: &ComponentName,
    ) -> Result<QueryOutputMetricsSnapshot> {
        let query_id = self.resolve_component_name(query_name, "query")?;
        self.get_query_output_metrics_by_id(&query_id).await
    }

    /// Get query output metrics by immutable UUID.
    pub async fn get_query_output_metrics_by_id(
        &self,
        query_id: &ComponentId,
    ) -> Result<QueryOutputMetricsSnapshot> {
        self.inspection
            .get_query_output_metrics_by_id(query_id)
            .await
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
    /// let metrics = core.get_reaction_metrics_by_name(&drasi_lib::ComponentName::try_new("my_reaction")?).await?;
    /// for (query_id, snapshot) in &metrics {
    ///     println!("  Query '{}': dedup_skip={}", query_id, snapshot.dedup_skip_count);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_metrics_by_name(
        &self,
        reaction_name: &ComponentName,
    ) -> Result<HashMap<ComponentId, ReactionMetricsSnapshot>> {
        let reaction_id = self.resolve_component_name(reaction_name, "reaction")?;
        self.get_reaction_metrics_by_id(&reaction_id).await
    }

    /// Get reaction metrics by immutable UUID.
    pub async fn get_reaction_metrics_by_id(
        &self,
        reaction_id: &ComponentId,
    ) -> Result<HashMap<ComponentId, ReactionMetricsSnapshot>> {
        self.inspection
            .get_reaction_metrics_by_id(reaction_id)
            .await
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
