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
mod runtime_support;

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{BootstrapEventSender, ComponentStatus};
use drasi_lib::computation::v1::{
    ComponentId, ComponentLifecycle, FailureDisposition, FailurePhase,
};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::{DrasiLib, Query, Source};
use mock_source::{MockSource, MockSourceHandle};
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

/// Wait on actual lifecycle publications, with a bounded failure deadline.
async fn wait_for_query_status(
    core: &DrasiLib,
    query_id: &drasi_core_types::ComponentName,
    expected: ComponentStatus,
) -> Result<ComponentStatus> {
    let mut events = core.subscribe_all_component_events();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = core.get_query_status_by_name(query_id).await?;
            if status == expected {
                return Ok(status);
            }
            match events.recv().await {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await?
}

async fn core_with_failed_bootstrap_query() -> Result<(DrasiLib, MockSourceHandle)> {
    let (mock_source, handle) = MockSource::new("test-source")?;
    mock_source
        .set_bootstrap_provider(Box::new(FailingBootstrapProvider))
        .await;

    let query = Query::cypher(
        drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .query("MATCH (p:Person) RETURN p.name AS name")
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("test-source").expect("fixture component name"),
    )
    .auto_start(true)
    .build()?;

    let core = runtime_support::builder()
        .with_allow_external_component_ids(true)
        .with_id("bootstrap-failure-test")
        .with_source(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
            mock_source,
        )
        .with_query(query)
        .build()
        .await?;

    if let Err(error) = core.start().await {
        anyhow::ensure!(
            error
                .downcast_ref::<drasi_lib::error::PartialStartError>()
                .is_some(),
            "unexpected startup failure: {error:#}"
        );
    }

    let status = wait_for_query_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        ComponentStatus::Error,
    )
    .await?;

    assert_eq!(
        status,
        ComponentStatus::Error,
        "Query should transition to Error state when bootstrap fails, got {status:?}"
    );
    let failed = core
        .component_by_name(&drasi_lib::ComponentName::try_new("q1")?)?
        .observed()?;
    anyhow::ensure!(
        failed.failure.as_ref().is_some_and(|failure| {
            format!("{:#}", failure.cause).contains("simulated bootstrap failure")
        }),
        "expected the configured bootstrap failure, got {failed:?}"
    );
    handle.wait_for_subscription_counts(1, None).await?;
    Ok((core, handle))
}

/// When the bootstrap provider fails, the query should end up in the Error state.
#[tokio::test]
async fn query_enters_error_state_when_bootstrap_fails() -> Result<()> {
    let (core, source) = core_with_failed_bootstrap_query().await?;
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(query.status().await, ComponentStatus::Error);
    query.stop().await?;
    source.wait_for_subscription_counts(1, Some(0)).await?;
    assert_eq!(query.subscription_count().await, 0);
    assert_eq!(query.status().await, ComponentStatus::Stopped);
    query.stop().await?;
    assert_eq!(
        wait_for_query_status(
            &core,
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            ComponentStatus::Stopped
        )
        .await?,
        ComponentStatus::Stopped
    );
    let repeated_stop = core
        .stop_query_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .await
        .unwrap_err();
    assert!(repeated_stop.to_string().contains("already stopped"));
    core.shutdown().await?;
    source.wait_for_subscription_counts(1, Some(0)).await?;

    Ok(())
}

#[tokio::test]
async fn recreating_failed_query_releases_old_subscriptions_without_restarting() -> Result<()> {
    let (core, source) = core_with_failed_bootstrap_query().await?;
    let old_query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let mut config = old_query.get_config().clone().into_authoring();
    config.query = "MATCH (p:Person) RETURN p".to_string();
    config.enable_bootstrap = false;
    config.id = None;
    config.auto_start = false;

    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await?;
    let replacement = core.add_query_with_handle(config.clone()).await?;
    replacement.wait_created().await?;
    source.wait_for_subscription_counts(1, Some(0)).await?;
    assert_eq!(old_query.subscription_count().await, 0);
    assert_eq!(old_query.status().await, ComponentStatus::Stopped);
    let new_query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert!(!Arc::ptr_eq(&old_query, &new_query));
    assert_eq!(new_query.get_config().query, config.query);
    assert_eq!(new_query.status().await, ComponentStatus::Added);
    assert_eq!(new_query.subscription_count().await, 0);
    assert_eq!(
        core.get_query_status_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name")
        )
        .await?,
        ComponentStatus::Added
    );
    let observed = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )?
        .observed()?;
    assert_eq!(observed.lifecycle, ComponentLifecycle::Stopped);
    assert!(!observed.started);

    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await?;
    assert_eq!(
        wait_for_query_status(
            &core,
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            ComponentStatus::Running
        )
        .await?,
        ComponentStatus::Running
    );
    assert_eq!(new_query.subscription_count().await, 1);
    source.wait_for_subscription_counts(2, Some(1)).await?;
    assert_eq!(old_query.subscription_count().await, 0);
    core.shutdown().await?;
    source.wait_for_subscription_counts(2, Some(0)).await?;
    Ok(())
}

#[tokio::test]
async fn removing_failed_query_releases_subscriptions() -> Result<()> {
    let (core, source) = core_with_failed_bootstrap_query().await?;
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
    )
    .await?;
    source.wait_for_subscription_counts(1, Some(0)).await?;
    assert_eq!(query.subscription_count().await, 0);
    assert_eq!(query.status().await, ComponentStatus::Stopped);
    assert!(core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name")
        )
        .await
        .is_err());
    core.shutdown().await?;
    source.wait_for_subscription_counts(1, Some(0)).await?;
    Ok(())
}

#[rstest::rstest]
#[case::instance(false)]
#[case::query_manager(true)]
#[tokio::test]
async fn stop_all_releases_failed_query_subscriptions(#[case] manager_only: bool) -> Result<()> {
    let (core, source) = core_with_failed_bootstrap_query().await?;
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    if manager_only {
        core.query_manager().stop_all().await?;
    } else {
        core.stop().await?;
    }
    source.wait_for_subscription_counts(1, Some(0)).await?;
    assert_eq!(query.subscription_count().await, 0);
    assert_eq!(query.status().await, ComponentStatus::Stopped);
    assert_eq!(
        wait_for_query_status(
            &core,
            &drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
            ComponentStatus::Stopped
        )
        .await?,
        ComponentStatus::Stopped
    );
    if manager_only {
        core.stop().await?;
    }
    core.shutdown().await?;
    source.wait_for_subscription_counts(1, Some(0)).await?;
    Ok(())
}

/// When multiple sources' bootstrap providers fail, the query should enter the
/// Error state and release both source receivers, including the source whose
/// bootstrap error was not the first one observed by the query.
#[tokio::test]
async fn query_enters_error_state_when_multiple_bootstraps_fail() -> Result<()> {
    let (source_a, handle_a) = MockSource::new("source-a")?;
    source_a
        .set_bootstrap_provider(Box::new(FailingBootstrapProvider))
        .await;

    let (source_b, handle_b) = MockSource::new("source-b")?;
    source_b
        .set_bootstrap_provider(Box::new(FailingBootstrapProvider))
        .await;

    let query = Query::cypher(
        drasi_core_types::ComponentName::try_new("q-multi").expect("fixture component name"),
    )
    .query("MATCH (p:Person) RETURN p.name AS name")
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("source-a").expect("fixture component name"),
    )
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("source-b").expect("fixture component name"),
    )
    .auto_start(true)
    .build()?;

    let core = Arc::new(
        runtime_support::builder()
            .with_allow_external_component_ids(true)
            .with_id("bootstrap-multi-failure-test")
            .with_source(
                drasi_core_types::ComponentName::try_new("source-a")
                    .expect("fixture component name"),
                source_a,
            )
            .with_source(
                drasi_core_types::ComponentName::try_new("source-b")
                    .expect("fixture component name"),
                source_b,
            )
            .with_query(query)
            .build()
            .await?,
    );

    if let Err(error) = core.start().await {
        anyhow::ensure!(
            error
                .downcast_ref::<drasi_lib::error::PartialStartError>()
                .is_some(),
            "unexpected startup failure: {error:#}"
        );
    }

    let status = wait_for_query_status(
        &core,
        &drasi_core_types::ComponentName::try_new("q-multi").expect("fixture component name"),
        ComponentStatus::Error,
    )
    .await?;

    assert_eq!(
        status,
        ComponentStatus::Error,
        "Query should transition to Error state when multiple bootstraps fail, got {status:?}"
    );

    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("q-multi").expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    handle_a.wait_for_subscription_counts(1, None).await?;
    handle_b.wait_for_subscription_counts(1, None).await?;
    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("q-multi").expect("fixture component name"),
    )
    .await?;
    handle_a.wait_for_subscription_counts(1, Some(0)).await?;
    handle_b.wait_for_subscription_counts(1, Some(0)).await?;
    assert_eq!(query.subscription_count().await, 0);
    assert_eq!(query.status().await, ComponentStatus::Stopped);
    assert_eq!(
        wait_for_query_status(
            &core,
            &drasi_core_types::ComponentName::try_new("q-multi").expect("fixture component name"),
            ComponentStatus::Stopped
        )
        .await?,
        ComponentStatus::Stopped
    );
    let repeated_stop = core
        .stop_query_by_name(
            &drasi_core_types::ComponentName::try_new("q-multi").expect("fixture component name"),
        )
        .await
        .unwrap_err();
    assert!(repeated_stop.to_string().contains("already stopped"));
    core.shutdown().await?;
    handle_a.wait_for_subscription_counts(1, Some(0)).await?;
    handle_b.wait_for_subscription_counts(1, Some(0)).await?;

    Ok(())
}

async fn core_with_conflicting_query() -> Result<DrasiLib> {
    let (source_a, _handle_a) = MockSource::new("source-a")?;
    let (source_b, _handle_b) = MockSource::new("source-b")?;
    let mut query = Query::cypher(
        drasi_core_types::ComponentName::try_new("conflicting-query")
            .expect("fixture component name"),
    )
    .query("MATCH (n:Sensor) RETURN n")
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("source-a").expect("fixture component name"),
    )
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("source-b").expect("fixture component name"),
    )
    .auto_start(false)
    .build()?;
    for subscription in &mut query.inputs {
        subscription.nodes = vec!["Sensor".to_string()];
    }
    let core = runtime_support::builder()
        .with_allow_external_component_ids(true)
        .with_source(
            drasi_core_types::ComponentName::try_new("source-a").expect("fixture component name"),
            source_a,
        )
        .with_source(
            drasi_core_types::ComponentName::try_new("source-b").expect("fixture component name"),
            source_b,
        )
        .with_query(query)
        .build()
        .await?;
    core.start().await?;

    let error = core
        .start_query_by_name(
            &drasi_core_types::ComponentName::try_new("conflicting-query")
                .expect("fixture component name"),
        )
        .await
        .expect_err("duplicate labels across sources must fail startup");
    assert!(
        error
            .to_string()
            .contains("Node label 'Sensor' is configured in multiple sources"),
        "expected the duplicate-label startup failure, got {error}"
    );
    assert_eq!(
        wait_for_query_status(
            &core,
            &drasi_core_types::ComponentName::try_new("conflicting-query")
                .expect("fixture component name"),
            ComponentStatus::Error
        )
        .await?,
        ComponentStatus::Error
    );
    Ok(core)
}

fn assert_invalid_query_has_stopped_without_hiding_its_error(core: &DrasiLib) -> Result<()> {
    let state = core
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("conflicting-query")
                .expect("fixture component name"),
        )?
        .observed()?;
    assert_eq!(state.lifecycle, ComponentLifecycle::Stopped);
    assert!(!state.started);
    let failure = state
        .failure
        .as_ref()
        .expect("invalid definition remains visible");
    assert_eq!(failure.phase, FailurePhase::Creation);
    assert_eq!(failure.disposition, FailureDisposition::Terminal);
    Ok(())
}

#[tokio::test]
async fn query_runtime_stop_after_duplicate_label_failure() -> Result<()> {
    let core = core_with_conflicting_query().await?;
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("conflicting-query")
                .expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    assert_eq!(query.status().await, ComponentStatus::Error);

    query.stop().await?;
    assert_invalid_query_has_stopped_without_hiding_its_error(&core)?;
    assert_eq!(query.status().await, ComponentStatus::Error);
    assert_eq!(query.subscription_count().await, 0);
    query.stop().await?;
    assert_eq!(
        wait_for_query_status(
            &core,
            &drasi_core_types::ComponentName::try_new("conflicting-query")
                .expect("fixture component name"),
            ComponentStatus::Error
        )
        .await?,
        ComponentStatus::Error
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn managed_query_stop_after_duplicate_label_failure() -> Result<()> {
    let core = core_with_conflicting_query().await?;

    for _ in 0..2 {
        core.stop_query_by_name(
            &drasi_core_types::ComponentName::try_new("conflicting-query")
                .expect("fixture component name"),
        )
        .await?;
        assert_invalid_query_has_stopped_without_hiding_its_error(&core)?;
        assert_eq!(
            wait_for_query_status(
                &core,
                &drasi_core_types::ComponentName::try_new("conflicting-query")
                    .expect("fixture component name"),
                ComponentStatus::Error
            )
            .await?,
            ComponentStatus::Error
        );
        let error = core
            .start_query_by_name(
                &drasi_core_types::ComponentName::try_new("conflicting-query")
                    .expect("fixture component name"),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("multiple sources"));
        assert_eq!(
            wait_for_query_status(
                &core,
                &drasi_core_types::ComponentName::try_new("conflicting-query")
                    .expect("fixture component name"),
                ComponentStatus::Error
            )
            .await?,
            ComponentStatus::Error
        );
    }
    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("conflicting-query")
            .expect("fixture component name"),
    )
    .await?;
    assert_invalid_query_has_stopped_without_hiding_its_error(&core)?;
    assert_eq!(
        wait_for_query_status(
            &core,
            &drasi_core_types::ComponentName::try_new("conflicting-query")
                .expect("fixture component name"),
            ComponentStatus::Error
        )
        .await?,
        ComponentStatus::Error
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn query_runtime_stop_preserves_successful_restart() -> Result<()> {
    let (source, _handle) = MockSource::new("test-source")?;
    let core = runtime_support::builder()
        .with_allow_external_component_ids(true)
        .with_source(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
            source,
        )
        .with_query(
            Query::cypher(
                drasi_core_types::ComponentName::try_new("restart-query")
                    .expect("fixture component name"),
            )
            .query("MATCH (n:Sensor) RETURN n")
            .from_input_by_name(
                drasi_core_types::ComponentName::try_new("test-source")
                    .expect("fixture component name"),
            )
            .auto_start(false)
            .build()?,
        )
        .build()
        .await?;
    core.start().await?;
    let query = core
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("restart-query")
                .expect("fixture component name"),
        )
        .await
        .map_err(anyhow::Error::msg)?;

    for _ in 0..2 {
        core.start_query_by_name(
            &drasi_core_types::ComponentName::try_new("restart-query")
                .expect("fixture component name"),
        )
        .await?;
        assert_eq!(
            wait_for_query_status(
                &core,
                &drasi_core_types::ComponentName::try_new("restart-query")
                    .expect("fixture component name"),
                ComponentStatus::Running
            )
            .await?,
            ComponentStatus::Running
        );
        assert!(query.subscription_count().await > 0);
        query.stop().await?;
        query.stop().await?;
        assert_eq!(query.subscription_count().await, 0);
        assert_eq!(query.status().await, ComponentStatus::Stopped);
        assert_eq!(
            wait_for_query_status(
                &core,
                &drasi_core_types::ComponentName::try_new("restart-query")
                    .expect("fixture component name"),
                ComponentStatus::Stopped
            )
            .await?,
            ComponentStatus::Stopped
        );
    }
    core.stop().await?;
    Ok(())
}
