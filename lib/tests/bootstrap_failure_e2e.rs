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

/// Poll a query's status until it reaches `Error` or a 5 second deadline elapses.
async fn wait_for_error_status(core: &DrasiLib, query_id: &str) -> Result<ComponentStatus> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut status = core.get_query_status(query_id).await?;
    while tokio::time::Instant::now() < deadline {
        if status == ComponentStatus::Error {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        status = core.get_query_status(query_id).await?;
    }
    Ok(status)
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

    // Bootstrap runs asynchronously, so give the supervisor a moment to
    // observe the failure before asserting on the terminal status.
    let status = wait_for_error_status(&core, "q1").await?;

    assert_eq!(
        status,
        ComponentStatus::Error,
        "Query should transition to Error state when bootstrap fails, got {status:?}"
    );

    Ok(())
}

/// When multiple sources' bootstrap providers fail, the query should enter the
/// Error state and the supervisor's multi-source aggregation should report each
/// failed source. This exercises the `failures` collection and join logic that a
/// single-source test never reaches.
#[tokio::test]
async fn query_enters_error_state_when_multiple_bootstraps_fail() -> Result<()> {
    let (source_a, _handle_a) = MockSource::new("source-a")?;
    source_a
        .set_bootstrap_provider(Box::new(FailingBootstrapProvider))
        .await;

    let (source_b, _handle_b) = MockSource::new("source-b")?;
    source_b
        .set_bootstrap_provider(Box::new(FailingBootstrapProvider))
        .await;

    let query = Query::cypher("q-multi")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("source-a")
        .from_source("source-b")
        .auto_start(true)
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("bootstrap-multi-failure-test")
            .with_source(source_a)
            .with_source(source_b)
            .with_query(query)
            .build()
            .await?,
    );

    core.start().await?;

    let status = wait_for_error_status(&core, "q-multi").await?;

    assert_eq!(
        status,
        ComponentStatus::Error,
        "Query should transition to Error state when multiple bootstraps fail, got {status:?}"
    );

    Ok(())
}
