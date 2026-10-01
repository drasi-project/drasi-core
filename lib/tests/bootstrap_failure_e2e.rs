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
//! Verifies that failed queries report `Error` and can be stopped through both
//! the query runtime and managed lifecycle APIs.

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

async fn wait_for_query_status(
    core: &DrasiLib,
    query_id: &str,
    expected: ComponentStatus,
) -> Result<ComponentStatus> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut status = core.get_query_status(query_id).await?;
    while tokio::time::Instant::now() < deadline {
        if status == expected {
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
    let status = wait_for_query_status(&core, "q1", ComponentStatus::Error).await?;

    assert_eq!(
        status,
        ComponentStatus::Error,
        "Query should transition to Error state when bootstrap fails, got {status:?}"
    );

    let query = core
        .query_manager()
        .get_query_instance("q1")
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(query.status().await, ComponentStatus::Error);
    assert!(
        query.subscription_count().await > 0,
        "a failed query still owns source forwarders until stopped"
    );
    query.stop().await?;
    assert_eq!(query.subscription_count().await, 0);
    assert_eq!(query.status().await, ComponentStatus::Stopped);
    query.stop().await?;
    assert_eq!(
        wait_for_query_status(&core, "q1", ComponentStatus::Stopped).await?,
        ComponentStatus::Stopped
    );
    core.stop().await?;

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

    let status = wait_for_query_status(&core, "q-multi", ComponentStatus::Error).await?;

    assert_eq!(
        status,
        ComponentStatus::Error,
        "Query should transition to Error state when multiple bootstraps fail, got {status:?}"
    );

    let query = core
        .query_manager()
        .get_query_instance("q-multi")
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(query.subscription_count().await > 0);
    core.stop_query("q-multi").await?;
    assert_eq!(query.subscription_count().await, 0);
    assert_eq!(query.status().await, ComponentStatus::Stopped);
    assert_eq!(
        wait_for_query_status(&core, "q-multi", ComponentStatus::Stopped).await?,
        ComponentStatus::Stopped
    );
    let repeated_stop = core.stop_query("q-multi").await.unwrap_err();
    assert!(repeated_stop.to_string().contains("already stopped"));
    core.stop().await?;

    Ok(())
}

async fn core_with_conflicting_query() -> Result<DrasiLib> {
    let (source_a, _handle_a) = MockSource::new("source-a")?;
    let (source_b, _handle_b) = MockSource::new("source-b")?;
    let mut query = Query::cypher("conflicting-query")
        .query("MATCH (n:Sensor) RETURN n")
        .from_source("source-a")
        .from_source("source-b")
        .auto_start(false)
        .build();
    for subscription in &mut query.sources {
        subscription.nodes = vec!["Sensor".to_string()];
    }
    let core = DrasiLib::builder()
        .with_source(source_a)
        .with_source(source_b)
        .with_query(query)
        .build()
        .await?;
    core.start().await?;

    let error = core
        .start_query("conflicting-query")
        .await
        .expect_err("duplicate labels across sources must fail startup");
    assert!(
        error
            .to_string()
            .contains("Node label 'Sensor' is configured in multiple sources"),
        "expected the duplicate-label startup failure, got {error}"
    );
    assert_eq!(
        wait_for_query_status(&core, "conflicting-query", ComponentStatus::Error).await?,
        ComponentStatus::Error
    );
    Ok(core)
}

#[tokio::test]
async fn query_runtime_stop_after_duplicate_label_failure() -> Result<()> {
    let core = core_with_conflicting_query().await?;
    let query = core
        .query_manager()
        .get_query_instance("conflicting-query")
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(query.status().await, ComponentStatus::Error);

    query.stop().await?;
    assert_eq!(query.status().await, ComponentStatus::Stopped);
    assert_eq!(query.subscription_count().await, 0);
    query.stop().await?;
    assert_eq!(
        wait_for_query_status(&core, "conflicting-query", ComponentStatus::Stopped).await?,
        ComponentStatus::Stopped
    );
    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn managed_query_stop_after_duplicate_label_failure() -> Result<()> {
    let core = core_with_conflicting_query().await?;
    let query = core
        .query_manager()
        .get_query_instance("conflicting-query")
        .await
        .map_err(anyhow::Error::msg)?;

    for _ in 0..2 {
        core.stop_query("conflicting-query").await?;
        assert_eq!(query.status().await, ComponentStatus::Stopped);
        assert_eq!(
            wait_for_query_status(&core, "conflicting-query", ComponentStatus::Stopped).await?,
            ComponentStatus::Stopped
        );
        let error = core.start_query("conflicting-query").await.unwrap_err();
        assert!(error.to_string().contains("multiple sources"));
        assert_eq!(
            wait_for_query_status(&core, "conflicting-query", ComponentStatus::Error).await?,
            ComponentStatus::Error
        );
    }
    core.stop_query("conflicting-query").await?;
    assert_eq!(
        wait_for_query_status(&core, "conflicting-query", ComponentStatus::Stopped).await?,
        ComponentStatus::Stopped
    );
    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn query_runtime_stop_preserves_successful_restart() -> Result<()> {
    let (source, _handle) = MockSource::new("test-source")?;
    let core = DrasiLib::builder()
        .with_source(source)
        .with_query(
            Query::cypher("restart-query")
                .query("MATCH (n:Sensor) RETURN n")
                .from_source("test-source")
                .auto_start(false)
                .build(),
        )
        .build()
        .await?;
    core.start().await?;
    let query = core
        .query_manager()
        .get_query_instance("restart-query")
        .await
        .map_err(anyhow::Error::msg)?;

    for _ in 0..2 {
        core.start_query("restart-query").await?;
        assert_eq!(
            wait_for_query_status(&core, "restart-query", ComponentStatus::Running).await?,
            ComponentStatus::Running
        );
        assert!(query.subscription_count().await > 0);
        query.stop().await?;
        query.stop().await?;
        assert_eq!(query.subscription_count().await, 0);
        assert_eq!(query.status().await, ComponentStatus::Stopped);
        assert_eq!(
            wait_for_query_status(&core, "restart-query", ComponentStatus::Stopped).await?,
            ComponentStatus::Stopped
        );
    }
    core.stop().await?;
    Ok(())
}
