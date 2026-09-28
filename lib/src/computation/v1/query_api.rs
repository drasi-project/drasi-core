// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{sync::Arc, time::Duration};

use crate::{
    config::QueryConfig,
    metrics::QueryOutputMetrics,
    queries::{FetchError, OutboxResponse, SnapshotResponse},
};

use super::{query_catalog::CatalogQuery, QueryResultsCatalog};

/// Read access to the results of an existing query. It does not own an evaluator
/// or perform lifecycle operations; the containing component remains the owner.
pub struct QueryApi {
    pub(crate) config: QueryConfig,
    pub(crate) catalog: Option<QueryResultsCatalog>,
    pub(crate) metrics: Arc<QueryOutputMetrics>,
    pub(super) output: CatalogQuery,
    pub(super) links:
        std::sync::Mutex<Vec<(QueryResultsCatalog, super::query_catalog::CatalogLink)>>,
}

impl QueryApi {
    pub(crate) fn link_catalog(&self, target: &QueryResultsCatalog) -> anyhow::Result<()> {
        let source = self.catalog.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Query '{}' does not expose result subscriptions",
                self.config.id
            )
        })?;
        let mut links = self
            .links
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue links poisoned"))?;
        if !links
            .iter()
            .any(|(catalog, _)| catalog.same_registry(target))
        {
            links.push((target.clone(), target.link_query(&self.config.id, source)?));
        }
        Ok(())
    }

    pub(crate) async fn snapshot(&self, timeout: Duration) -> anyhow::Result<SnapshotResponse> {
        tokio::time::timeout(timeout, self.output.results.wait_ready()).await??;
        self.metrics.record_snapshot_fetch();
        super::plugin_reaction::snapshot_response(
            &self.config.id,
            &super::plugin_reaction::output_view(&self.output)?,
        )
    }

    pub(crate) async fn outbox(
        &self,
        after: u64,
        timeout: Duration,
    ) -> Result<OutboxResponse, FetchError> {
        tokio::time::timeout(timeout, self.output.results.wait_ready())
            .await
            .map_err(|_| FetchError::TimedOut)?
            .map_err(|error| {
                log::error!("Query output is unavailable: {error:#}");
                FetchError::NotRunning {
                    status: crate::ComponentStatus::Error,
                }
            })?;
        let view = super::plugin_reaction::output_view(&self.output).map_err(|error| {
            log::error!("Cannot read query output: {error:#}");
            FetchError::NotRunning {
                status: crate::ComponentStatus::Error,
            }
        })?;
        super::plugin_reaction::outbox_response(&view, after)
    }

    pub(crate) fn is_persistent(&self) -> anyhow::Result<bool> {
        self.output
            .output_persistence
            .read()
            .map_err(|_| anyhow::anyhow!("query persistence observation poisoned"))?
            .ok_or_else(|| anyhow::anyhow!("query output persistence is not yet known"))
    }

    pub(crate) fn generation(&self) -> anyhow::Result<u64> {
        Ok(self.output.results.snapshot()?.generation)
    }
}
