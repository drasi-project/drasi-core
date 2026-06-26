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

//! Integration test for bootstrap failure handling.
//!
//! Verifies that when a query's bootstrap provider fails, the query transitions
//! to the `Error` state instead of silently becoming `Running`.

mod mock_source;

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{BootstrapEventSender, ComponentStatus};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::{DrasiLib, Query, Source};
use mock_source::MockSource;
use std::sync::Arc;
use std::time::Duration;

/// A bootstrap provider that always fails.
struct FailingBootstrapProvider;

#[async_trait]
impl BootstrapProvider for FailingBootstrapProvider {
    async fn bootstrap(
        &self,
        _request: BootstrapRequest,
        _context: &BootstrapContext,
        _event_tx: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        Err(anyhow::anyhow!("simulated bootstrap failure"))
    }
}

/// When the bootstrap provider fails, the query should end up in the Error state.
#[tokio::test]
async fn query_enters_error_state_when_bootstrap_fails() -> Result<()> {
    let (mock_source, _handle) = MockSource::new("test-source")?;
    mock_source
        .set_bootstrap_provider(Box::new(FailingBootstrapProvider))
        .await;

    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .auto_start(true)
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("bootstrap-failure-test")
            .with_source(mock_source)
            .with_query(query)
            .build()
            .await?,
    );

    core.start().await?;

    // Poll for the query to reach a terminal status. Bootstrap runs
    // asynchronously, so give the supervisor a moment to observe the failure.
    const MAX_STATUS_POLL_ATTEMPTS: usize = 50;
    let mut status = core.get_query_status("q1").await?;
    for _ in 0..MAX_STATUS_POLL_ATTEMPTS {
        if status == ComponentStatus::Error {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        status = core.get_query_status("q1").await?;
    }

    assert_eq!(
        status,
        ComponentStatus::Error,
        "Query should transition to Error state when bootstrap fails, got {status:?}"
    );

    Ok(())
}
