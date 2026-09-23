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

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use super::output_state::{FetchError, OutboxResponse, SnapshotResponse};
use crate::{
    channels::{ComponentStatus, QuerySubscriptionResponse},
    config::QueryConfig,
    metrics::QueryOutputMetrics,
};

/// Application-facing query contract implemented by graph-owned queries and
/// plugin proxies. It does not select or construct an execution runtime.
#[async_trait]
pub trait Query: Send + Sync {
    async fn start(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    fn get_config(&self) -> &QueryConfig;
    fn as_any(&self) -> &dyn std::any::Any;

    /// Number of active subscription forwarders, for diagnostics.
    async fn subscription_count(&self) -> usize {
        0
    }

    /// Attach a receiver and sample the output head atomically with publication.
    ///
    /// Persistent implementations must hydrate output before sampling the head.
    async fn subscribe(&self, reaction_id: String) -> Result<QuerySubscriptionResponse>;

    /// Fetch the live result set and its sequence after bootstrap completes.
    async fn fetch_snapshot(&self) -> Result<SnapshotResponse, FetchError>;

    /// Fetch retained entries after a sequence, or report an outbox gap.
    async fn fetch_outbox(&self, after_sequence: u64) -> Result<OutboxResponse, FetchError>;

    /// Distinguish output sequence numbers after a wipe or rebuild.
    async fn output_generation(&self) -> u64 {
        0
    }

    fn output_metrics(&self) -> Option<Arc<QueryOutputMetrics>> {
        None
    }

    /// Whether query output is lost on process restart.
    ///
    /// Persistent implementations must override this so durable reactions can
    /// safely resume. Undeclared capabilities are treated as volatile.
    fn is_volatile(&self) -> bool {
        true
    }

    /// Release persistent handles without deleting stored data.
    ///
    /// Volatile implementations have nothing to release. Persistent
    /// implementations must release their retained handles at awaited shutdown.
    async fn release_persistent_handles(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ContractQuery(QueryConfig);

    #[async_trait]
    impl Query for ContractQuery {
        async fn start(&self) -> Result<()> {
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            Ok(())
        }
        async fn status(&self) -> ComponentStatus {
            ComponentStatus::Stopped
        }
        fn get_config(&self) -> &QueryConfig {
            &self.0
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        async fn subscribe(&self, _: String) -> Result<QuerySubscriptionResponse> {
            anyhow::bail!("test contract has no subscriptions")
        }
        async fn fetch_snapshot(&self) -> Result<SnapshotResponse, FetchError> {
            Err(FetchError::NotRunning {
                status: ComponentStatus::Stopped,
            })
        }
        async fn fetch_outbox(&self, _: u64) -> Result<OutboxResponse, FetchError> {
            Err(FetchError::NotRunning {
                status: ComponentStatus::Stopped,
            })
        }
    }

    #[test]
    fn query_is_volatile_defaults_to_true_when_not_overridden() {
        assert!(
            ContractQuery(crate::Query::cypher("contract").query("RETURN 1").build()).is_volatile()
        );
    }
}
