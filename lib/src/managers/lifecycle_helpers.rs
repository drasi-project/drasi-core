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

//! Shared lifecycle helpers for component managers.
//!
//! The start/stop/list/status patterns are identical across SourceManager,
//! QueryManager, and ReactionManager. These helpers eliminate the duplication
//! by parameterizing the component type string and runtime trait.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use std::future::Future;
use tokio::sync::RwLock;

use crate::channels::ComponentStatus;
use crate::component_graph::{ComponentGraph, ComponentKind};

/// Trait for component runtime instances that can be started and stopped.
///
/// Implemented by `Arc<dyn Source>`, `Arc<dyn Query>`, and `Arc<dyn Reaction>`.
/// Allows the shared lifecycle helpers to call start/stop without knowing
/// the concrete component type.
///
/// Uses `#[async_trait]` (boxed futures) rather than RPITIT to avoid
/// higher-ranked lifetime issues when futures flow through axum handlers.
#[async_trait]
pub trait ComponentRuntime: Send + Sync + 'static {
    async fn start(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn deprovision(&self) -> Result<()>;
}

#[async_trait]
impl ComponentRuntime for Arc<dyn crate::sources::Source> {
    async fn start(&self) -> Result<()> {
        crate::sources::Source::start(self.as_ref()).await
    }
    async fn stop(&self) -> Result<()> {
        crate::sources::Source::stop(self.as_ref()).await
    }
    async fn deprovision(&self) -> Result<()> {
        crate::sources::Source::deprovision(self.as_ref()).await
    }
}

#[async_trait]
impl ComponentRuntime for Arc<dyn crate::queries::manager::Query> {
    async fn start(&self) -> Result<()> {
        crate::queries::manager::Query::start(self.as_ref()).await
    }
    async fn stop(&self) -> Result<()> {
        crate::queries::manager::Query::stop(self.as_ref()).await
    }
    async fn deprovision(&self) -> Result<()> {
        Ok(()) // Queries don't have deprovision
    }
}

#[async_trait]
impl ComponentRuntime for Arc<dyn crate::reactions::Reaction> {
    async fn start(&self) -> Result<()> {
        crate::reactions::Reaction::start(self.as_ref()).await
    }
    async fn stop(&self) -> Result<()> {
        crate::reactions::Reaction::stop(self.as_ref()).await
    }
    async fn deprovision(&self) -> Result<()> {
        crate::reactions::Reaction::deprovision(self.as_ref()).await
    }
}

/// Get a component's current status from the graph.
///
/// Reads the graph node and returns its status field.
///
/// # Errors
/// Returns an error if the component is not found in the graph.
pub async fn get_component_status(
    graph: &Arc<RwLock<ComponentGraph>>,
    id: &str,
    component_type: &str,
) -> Result<ComponentStatus> {
    let graph = graph.read().await;
    let node = graph
        .get_component(id)
        .ok_or_else(|| anyhow::anyhow!("{component_type} not found: {id}"))?;
    Ok(node.status)
}

/// List all components of a given kind from the graph.
pub async fn list_components(
    graph: &Arc<RwLock<ComponentGraph>>,
    kind: &ComponentKind,
) -> Vec<(String, ComponentStatus)> {
    let graph = graph.read().await;
    graph.list_by_kind(kind)
}

/// Get a component's runtime instance from the graph, cloning the Arc.
///
/// Returns `None` if the component has no runtime or the type doesn't match.
pub async fn get_runtime<T: 'static + Clone>(
    graph: &Arc<RwLock<ComponentGraph>>,
    id: &str,
) -> Option<T> {
    let graph = graph.read().await;
    graph.get_runtime::<T>(id).cloned()
}

/// Start a component: validate transition → call start → revert on error.
///
/// This is the shared pattern used by all three managers. After calling this,
/// the manager may perform type-specific post-start actions (e.g., ReactionManager
/// subscribes to queries).
///
/// # Arguments
/// * `graph` — shared component graph
/// * `id` — component ID
/// * `component_type` — display name ("source", "query", "reaction")
/// * `runtime` — the component's runtime instance (must implement start/stop)
pub async fn start_component<R: ComponentRuntime>(
    graph: &Arc<RwLock<ComponentGraph>>,
    id: &str,
    component_type: &str,
    runtime: &R,
) -> Result<()> {
    // Validate and apply Starting transition atomically through the graph
    {
        let mut g = graph.write().await;
        g.validate_and_transition(
            id,
            ComponentStatus::Starting,
            Some(format!("Starting {component_type}")),
        )?;
    }

    if let Err(e) = runtime.start().await {
        // Revert graph status so the component isn't stuck at Starting
        let mut g = graph.write().await;
        let _ = g.validate_and_transition(
            id,
            ComponentStatus::Error,
            Some(format!("Start failed: {e}")),
        );
        return Err(e);
    }

    Ok(())
}

/// Stop a component: validate transition → call stop → revert on error.
///
/// This is the shared pattern used by all three managers. Before calling this,
/// the manager may perform type-specific pre-stop actions (e.g., ReactionManager
/// aborts subscription tasks).
pub async fn stop_component<R: ComponentRuntime>(
    graph: &Arc<RwLock<ComponentGraph>>,
    id: &str,
    component_type: &str,
    runtime: &R,
) -> Result<()> {
    // Validate and apply Stopping transition atomically through the graph
    {
        let mut g = graph.write().await;
        g.validate_and_transition(
            id,
            ComponentStatus::Stopping,
            Some(format!("Stopping {component_type}")),
        )?;
    }

    if let Err(e) = runtime.stop().await {
        // Revert graph status so the component isn't stuck at Stopping
        let mut g = graph.write().await;
        let _ = g.validate_and_transition(
            id,
            ComponentStatus::Error,
            Some(format!("Stop failed: {e}")),
        );
        return Err(e);
    }

    Ok(())
}

/// Teardown a component: claim teardown intent → stop if running → wait →
/// optionally deprovision → remove runtime → clean up logs.
///
/// This is the shared pattern used by all three managers. The caller may
/// provide a `pre_stop` hook for type-specific cleanup (e.g., ReactionManager
/// aborts subscription tasks before stopping).
///
/// # Arguments
/// * `graph` — shared component graph
/// * `id` — component ID
/// * `component_type` — display name ("source", "query", "reaction")
/// * `log_component_type` — the `ComponentType` enum value for log key construction
/// * `instance_id` — the DrasiLib instance ID
/// * `log_registry` — shared log registry for cleanup
/// * `cleanup` — if true, calls `deprovision()` on the runtime
/// * `pre_stop` — optional async closure called before `runtime.stop()` (e.g., abort tasks)
pub async fn teardown_component<T: ComponentRuntime + Clone + 'static, F, Fut>(
    graph: &Arc<RwLock<ComponentGraph>>,
    id: &str,
    component_type: &str,
    log_component_type: crate::channels::ComponentType,
    instance_id: &str,
    log_registry: &crate::managers::ComponentLogRegistry,
    cleanup: bool,
    pre_stop: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let runtime = get_runtime::<T>(graph, id)
        .await
        .ok_or_else(|| anyhow::anyhow!("{component_type} not found: {id}"))?;

    // Atomically read status and claim teardown intent under a single write lock.
    let needs_stop = {
        let mut g = graph.write().await;
        let node = g
            .get_component(id)
            .ok_or_else(|| anyhow::anyhow!("{component_type} '{id}' not found in graph"))?;
        let status = node.status;

        if matches!(
            status,
            ComponentStatus::Stopping | ComponentStatus::Reconfiguring
        ) {
            return Err(anyhow::anyhow!(
                "Cannot delete {component_type} '{id}' while it is {status:?}"
            ));
        }

        if matches!(status, ComponentStatus::Running | ComponentStatus::Starting) {
            g.validate_and_transition(
                id,
                ComponentStatus::Stopping,
                Some("Stopping for teardown".to_string()),
            )?;
            true
        } else {
            false
        }
    };

    if needs_stop {
        log::info!("Stopping {component_type} '{id}' before teardown");
        pre_stop().await;
        if let Err(e) = runtime.stop().await {
            log::warn!(
                "Failed to stop {component_type} '{id}' during teardown (may already be stopped): {e}"
            );
        }

        if crate::component_graph::wait_for_status(
            graph,
            id,
            &[ComponentStatus::Stopped, ComponentStatus::Error],
            std::time::Duration::from_secs(10),
        )
        .await
        .is_err()
        {
            log::warn!(
                "{component_type} '{id}' did not reach Stopped state within timeout, proceeding with teardown"
            );
        }
    }

    if cleanup {
        if let Err(e) = runtime.deprovision().await {
            log::warn!("Deprovision failed for {component_type} '{id}': {e}");
        }
    }

    // Remove runtime from graph and clean up log history
    {
        let mut g = graph.write().await;
        g.take_runtime::<T>(id);
    }
    let log_key = crate::managers::ComponentLogKey::new(instance_id, log_component_type, id);
    log_registry.remove_component_by_key(&log_key).await;
    log::info!("Teardown {component_type}: {id}");

    Ok(())
}

/// Reconfigure a component: transition to Reconfiguring → stop if running →
/// init/replace → restart or transition to Stopped.
///
/// This is the shared reconfiguration state machine used by all three managers.
/// It fixes the stuck-in-Reconfiguring bug by reverting to `Error` on any failure
/// after entering the `Reconfiguring` state.
///
/// # Arguments
/// * `graph` — shared component graph
/// * `id` — component ID
/// * `component_type` — display name ("source", "query", "reaction")
/// * `old_runtime` — reference to the old runtime (for calling stop)
/// * `pre_stop` — async closure called before stop (e.g., abort subscription tasks)
/// * `init_and_replace` — async closure that initializes the new runtime and replaces it in the graph
/// * `restart` — async closure that restarts the component (e.g., start_source, start_reaction)
pub async fn reconfigure_component<T, Fut1, Fut2, Fut3>(
    graph: &Arc<RwLock<ComponentGraph>>,
    id: &str,
    component_type: &str,
    old_runtime: &T,
    pre_stop: impl FnOnce() -> Fut1,
    init_and_replace: impl FnOnce() -> Fut2,
    restart: impl FnOnce() -> Fut3,
) -> Result<()>
where
    T: ComponentRuntime,
    Fut1: Future<Output = ()>,
    Fut2: Future<Output = Result<()>>,
    Fut3: Future<Output = Result<()>>,
{
    // Read status from the graph (source of truth) and determine if running
    let was_running = {
        let g = graph.read().await;
        let node = g
            .get_component(id)
            .ok_or_else(|| anyhow::anyhow!("{component_type} '{id}' not found in graph"))?;
        matches!(
            node.status,
            ComponentStatus::Running | ComponentStatus::Starting
        )
    };

    // Validate and set Reconfiguring atomically through the graph
    {
        let mut g = graph.write().await;
        g.validate_and_transition(
            id,
            ComponentStatus::Reconfiguring,
            Some(format!("Reconfiguring {component_type}")),
        )?;
    }

    // If running or starting, stop first.
    // Revert to Error on failure to prevent stuck Reconfiguring state.
    if was_running {
        log::info!("Stopping {component_type} '{id}' for reconfiguration");
        pre_stop().await;

        if let Err(e) = async {
            old_runtime.stop().await?;
            crate::component_graph::wait_for_status(
                graph,
                id,
                &[ComponentStatus::Stopped, ComponentStatus::Error],
                std::time::Duration::from_secs(10),
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!("Timed out waiting for {component_type} '{id}' to stop: {e}")
            })
        }
        .await
        {
            let mut g = graph.write().await;
            let _ = g.validate_and_transition(
                id,
                ComponentStatus::Error,
                Some(format!("Reconfiguration failed during stop: {e}")),
            );
            return Err(e);
        }
    }

    // Initialize and replace with the new runtime.
    // Revert to Error on failure.
    if let Err(e) = init_and_replace().await {
        let mut g = graph.write().await;
        let _ = g.validate_and_transition(
            id,
            ComponentStatus::Error,
            Some(format!("Reconfiguration failed: {e}")),
        );
        return Err(e);
    }

    log::info!("Reconfigured {component_type} '{id}'");

    // Restart if it was running before, otherwise transition back to Stopped
    if was_running {
        restart().await
    } else {
        let mut g = graph.write().await;
        g.validate_and_transition(
            id,
            ComponentStatus::Stopped,
            Some("Reconfiguration complete".to_string()),
        )?;
        Ok(())
    }
}

/// Start all components of a given kind that have auto_start enabled.
///
/// Components are started via the provided `start_fn` closure, which allows
/// each manager to use its own start logic (e.g., reactions need subscription setup).
///
/// Returns an error if any components failed to start, after attempting all.
pub async fn start_all_components<T, F, Fut>(
    graph: &Arc<RwLock<ComponentGraph>>,
    kind: &ComponentKind,
    component_type: &str,
    auto_start_filter: impl Fn(&T) -> bool,
    start_fn: F,
) -> Result<()>
where
    T: Clone + 'static,
    F: Fn(String, T) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let entries: Vec<(String, T)> = {
        let g = graph.read().await;
        g.list_by_kind(kind)
            .iter()
            .filter_map(|(id, _)| g.get_runtime::<T>(id).map(|r| (id.clone(), r.clone())))
            .collect()
    };

    let mut failures = Vec::new();

    for (id, runtime) in entries {
        if !auto_start_filter(&runtime) {
            log::info!("Skipping {component_type} '{id}' (auto_start=false)");
            continue;
        }

        log::info!("Starting {component_type}: {id}");
        if let Err(e) = start_fn(id.clone(), runtime).await {
            log::error!("Failed to start {component_type} {id}: {e}");
            failures.push((id, e.to_string()));
        }
    }

    if !failures.is_empty() {
        let error_msg = failures
            .iter()
            .map(|(id, err)| format!("{id}: {err}"))
            .collect::<Vec<_>>()
            .join(", ");
        Err(anyhow::anyhow!(
            "Failed to start some {component_type}s: {error_msg}"
        ))
    } else {
        Ok(())
    }
}

/// Stop all active components of a given kind (best-effort).
///
/// Components in Running or Starting state are stopped via the provided `stop_fn`.
/// Errors are logged but do not prevent stopping other components.
pub async fn stop_all_components<F, Fut>(
    graph: &Arc<RwLock<ComponentGraph>>,
    kind: &ComponentKind,
    component_type: &str,
    stop_fn: F,
) -> Result<()>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let ids: Vec<String> = {
        let g = graph.read().await;
        g.list_by_kind(kind)
            .iter()
            .map(|(id, _)| id.clone())
            .collect()
    };

    for id in ids {
        let is_active = {
            let g = graph.read().await;
            g.get_component(&id)
                .map(|n| {
                    matches!(
                        n.status,
                        ComponentStatus::Running | ComponentStatus::Starting
                    )
                })
                .unwrap_or(false)
        };

        if is_active {
            if let Err(e) = stop_fn(id.clone()).await {
                crate::managers::log_component_error(component_type, &id, &e.to_string());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component_graph::{ComponentGraph, ComponentNode};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct MockRuntime {
        should_fail: AtomicBool,
    }

    impl MockRuntime {
        fn new() -> Self {
            Self {
                should_fail: AtomicBool::new(false),
            }
        }

        fn set_should_fail(&self, fail: bool) {
            self.should_fail.store(fail, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ComponentRuntime for MockRuntime {
        async fn start(&self) -> Result<()> {
            if self.should_fail.load(Ordering::SeqCst) {
                Err(anyhow::anyhow!("start failed"))
            } else {
                Ok(())
            }
        }

        async fn stop(&self) -> Result<()> {
            if self.should_fail.load(Ordering::SeqCst) {
                Err(anyhow::anyhow!("stop failed"))
            } else {
                Ok(())
            }
        }

        async fn deprovision(&self) -> Result<()> {
            Ok(())
        }
    }

    fn source_node(id: &str) -> ComponentNode {
        ComponentNode {
            id: id.to_string(),
            kind: ComponentKind::Source,
            status: ComponentStatus::Stopped,
            metadata: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_start_component_success() {
        let (mut graph, _rx) = ComponentGraph::new("test");
        graph.add_component(source_node("s1")).unwrap();
        let graph = Arc::new(RwLock::new(graph));

        let runtime = MockRuntime::new();
        start_component(&graph, "s1", "source", &runtime)
            .await
            .unwrap();

        let g = graph.read().await;
        assert_eq!(
            g.get_component("s1").unwrap().status,
            ComponentStatus::Starting
        );
    }

    #[tokio::test]
    async fn test_start_component_failure_reverts_to_error() {
        let (mut graph, _rx) = ComponentGraph::new("test");
        graph.add_component(source_node("s1")).unwrap();
        let graph = Arc::new(RwLock::new(graph));

        let runtime = MockRuntime::new();
        runtime.set_should_fail(true);
        let result = start_component(&graph, "s1", "source", &runtime).await;
        assert!(result.is_err());

        let g = graph.read().await;
        assert_eq!(
            g.get_component("s1").unwrap().status,
            ComponentStatus::Error
        );
    }

    #[tokio::test]
    async fn test_stop_component_success() {
        let (mut graph, _rx) = ComponentGraph::new("test");
        graph.add_component(source_node("s1")).unwrap();
        // Move to Running first
        graph
            .validate_and_transition("s1", ComponentStatus::Starting, None)
            .unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Running, None)
            .unwrap();
        let graph = Arc::new(RwLock::new(graph));

        let runtime = MockRuntime::new();
        stop_component(&graph, "s1", "source", &runtime)
            .await
            .unwrap();

        let g = graph.read().await;
        assert_eq!(
            g.get_component("s1").unwrap().status,
            ComponentStatus::Stopping
        );
    }

    #[tokio::test]
    async fn test_stop_component_failure_reverts_to_error() {
        let (mut graph, _rx) = ComponentGraph::new("test");
        graph.add_component(source_node("s1")).unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Starting, None)
            .unwrap();
        graph
            .validate_and_transition("s1", ComponentStatus::Running, None)
            .unwrap();
        let graph = Arc::new(RwLock::new(graph));

        let runtime = MockRuntime::new();
        runtime.set_should_fail(true);
        let result = stop_component(&graph, "s1", "source", &runtime).await;
        assert!(result.is_err());

        let g = graph.read().await;
        assert_eq!(
            g.get_component("s1").unwrap().status,
            ComponentStatus::Error
        );
    }

    #[tokio::test]
    async fn test_get_component_status_found() {
        let (mut graph, _rx) = ComponentGraph::new("test");
        graph.add_component(source_node("s1")).unwrap();
        let graph = Arc::new(RwLock::new(graph));

        let status = get_component_status(&graph, "s1", "source").await.unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_get_component_status_not_found() {
        let (graph, _rx) = ComponentGraph::new("test");
        let graph = Arc::new(RwLock::new(graph));

        let result = get_component_status(&graph, "nope", "source").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_components() {
        let (mut graph, _rx) = ComponentGraph::new("test");
        graph.add_component(source_node("s1")).unwrap();
        graph.add_component(source_node("s2")).unwrap();
        let graph = Arc::new(RwLock::new(graph));

        let list = list_components(&graph, &ComponentKind::Source).await;
        assert_eq!(list.len(), 2);
    }
}
