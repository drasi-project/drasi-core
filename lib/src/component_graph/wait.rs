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

use super::ComponentGraph;
use crate::channels::ComponentStatus;

/// Wait on authoritative computation publications, without polling or a second
/// mutable component graph.
pub async fn wait_for_status(
    graph: &ComponentGraph,
    component_id: &str,
    target_statuses: &[ComponentStatus],
    timeout: std::time::Duration,
) -> anyhow::Result<ComponentStatus> {
    let mut changes = graph.inspector()?.subscribe();
    tokio::time::timeout(timeout, async {
        loop {
            changes.borrow_and_update();
            let snapshot = graph.snapshot().await?;
            let component = snapshot
                .get_component(component_id)
                .ok_or_else(|| anyhow::anyhow!("Component '{component_id}' not found in graph"))?;
            if target_statuses.contains(&component.status) {
                return Ok(component.status);
            }
            changes.changed().await?;
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Timed out waiting for component '{component_id}' to reach {target_statuses:?}"
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sources::tests::TestMockSource, DrasiLib};
    use std::time::Duration;

    async fn instance() -> DrasiLib {
        DrasiLib::builder()
            .with_source(TestMockSource::with_auto_start("source-1".into(), false).unwrap())
            .build()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_wait_for_status_already_reached() {
        let core = instance().await;
        let status = wait_for_status(
            &core.component_graph(),
            "source-1",
            &[ComponentStatus::Added],
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        assert_eq!(status, ComponentStatus::Added);
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_status_component_not_found() {
        let core = instance().await;
        let error = wait_for_status(
            &core.component_graph(),
            "nonexistent",
            &[ComponentStatus::Running],
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("not found"));
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_status_timeout() {
        let core = instance().await;
        let error = wait_for_status(
            &core.component_graph(),
            "source-1",
            &[ComponentStatus::Running],
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Timed out"));
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_status_reaches_target_via_update() {
        let core = instance().await;
        let graph = core.component_graph();
        let waiter = wait_for_status(
            &graph,
            "source-1",
            &[ComponentStatus::Running],
            Duration::from_secs(2),
        );
        tokio::pin!(waiter);
        assert!(futures::poll!(&mut waiter).is_pending());
        core.start_source("source-1").await.unwrap();
        assert_eq!(waiter.await.unwrap(), ComponentStatus::Running);
        core.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_status_multiple_targets() {
        let core = instance().await;
        let status = wait_for_status(
            &core.component_graph(),
            "source-1",
            &[ComponentStatus::Running, ComponentStatus::Added],
            Duration::from_millis(100),
        )
        .await
        .unwrap();
        assert_eq!(status, ComponentStatus::Added);
        core.shutdown().await.unwrap();
    }
}
