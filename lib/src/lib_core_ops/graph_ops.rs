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

//! Component graph operations for DrasiLib
//!
//! Provides public API methods for querying the component dependency graph,
//! including graph snapshots, dependency lookups, and impact analysis.

use crate::component_graph::{ComponentNode, GraphSnapshot};
use crate::error::Result;
use crate::lib_core::DrasiLib;

impl DrasiLib {
    // ============================================================================
    // Component Graph Operations
    // ============================================================================

    /// Get a serializable snapshot of the full component dependency graph.
    ///
    /// The snapshot includes the instance root node, all component nodes
    /// (sources, queries, reactions), and all bidirectional relationship edges.
    /// The returned `GraphSnapshot` is serializable to JSON via serde.
    ///
    /// This method does NOT require the server to be initialized or running.
    /// The graph is available immediately after construction.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let snapshot = core.get_graph().await;
    /// println!("Instance: {}", snapshot.instance_id);
    /// println!("Components: {}", snapshot.nodes.len());
    /// println!("Relationships: {}", snapshot.edges.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_graph(&self) -> GraphSnapshot {
        self.component_graph.read().await.snapshot()
    }

    /// Get all components that depend on the given component.
    ///
    /// "Dependents" are components that would be affected if this component
    /// were removed or stopped. For example, queries that subscribe to a source,
    /// or reactions that subscribe to a query.
    ///
    /// Returns an empty list if the component has no dependents or doesn't exist.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let dependents = core.get_dependents("orders_db").await;
    /// for dep in &dependents {
    ///     println!("  {} ({:?}) depends on orders_db", dep.id, dep.kind);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_dependents(&self, id: &str) -> Vec<ComponentNode> {
        self.component_graph
            .read()
            .await
            .get_dependents(id)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Get all components that the given component depends on.
    ///
    /// "Dependencies" are components that this component needs to function.
    /// For example, the sources that a query subscribes to, or the queries
    /// that a reaction subscribes to.
    ///
    /// Returns an empty list if the component has no dependencies or doesn't exist.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let deps = core.get_dependencies("active_orders").await;
    /// for dep in &deps {
    ///     println!("  active_orders depends on {} ({:?})", dep.id, dep.kind);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_dependencies(&self, id: &str) -> Vec<ComponentNode> {
        self.component_graph
            .read()
            .await
            .get_dependencies(id)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Check if a component can be safely removed without breaking dependents.
    ///
    /// Returns `Ok(())` if safe to remove, or `Err(DrasiError::HasDependents)`
    /// with the list of dependent component IDs if removal would break the graph.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// match core.can_remove_component("orders_db").await {
    ///     Ok(()) => println!("Safe to remove"),
    ///     Err(e) => println!("Cannot remove: {}", e),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn can_remove_component(&self, id: &str) -> Result<()> {
        let graph = self.component_graph.read().await;
        graph.can_remove(id).map_err(|dependent_ids| {
            crate::error::DrasiError::Internal(anyhow::anyhow!(
                "Cannot remove '{}': depended on by: {}",
                id,
                dependent_ids.join(", ")
            ))
        })
    }
}
