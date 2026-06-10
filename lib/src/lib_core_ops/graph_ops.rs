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
    /// Returns `Ok(())` if safe to remove, or `Err(DrasiError::Validation { .. })`
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
            crate::error::DrasiError::validation(format!(
                "Cannot remove '{}': depended on by: {}",
                id,
                dependent_ids.join(", ")
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::component_graph::ComponentKind;
    use crate::sources::tests::TestMockSource;
    use crate::{DrasiLib, Query};

    // ========================================================================
    // get_graph
    // ========================================================================

    #[tokio::test]
    async fn get_graph_empty_has_instance_root() {
        let core = DrasiLib::builder()
            .with_id("empty-graph")
            .build()
            .await
            .unwrap();

        let snapshot = core.get_graph().await;

        assert_eq!(snapshot.instance_id, "empty-graph");
        // Instance root must be present
        assert!(snapshot
            .nodes
            .iter()
            .any(|n| n.id == "empty-graph" && n.kind == ComponentKind::Instance));
        // No user-added sources, queries, or reactions
        assert!(!snapshot
            .nodes
            .iter()
            .any(|n| n.kind == ComponentKind::Query));
        assert!(!snapshot
            .nodes
            .iter()
            .any(|n| n.kind == ComponentKind::Reaction));
    }

    #[tokio::test]
    async fn get_graph_with_components() {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("graph-test")
            .with_source(source)
            .with_query(
                Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("src1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let snapshot = core.get_graph().await;

        assert_eq!(snapshot.instance_id, "graph-test");
        // instance + source + query
        assert!(snapshot.nodes.len() >= 3);

        let kinds: Vec<_> = snapshot.nodes.iter().map(|n| &n.kind).collect();
        assert!(kinds.contains(&&ComponentKind::Instance));
        assert!(kinds.contains(&&ComponentKind::Source));
        assert!(kinds.contains(&&ComponentKind::Query));

        // There should be edges connecting the components
        assert!(!snapshot.edges.is_empty());
    }

    // ========================================================================
    // get_dependents
    // ========================================================================

    #[tokio::test]
    async fn get_dependents_source_with_query() {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("dep-test")
            .with_source(source)
            .with_query(
                Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("src1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let dependents = core.get_dependents("src1").await;

        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].id, "q1");
        assert_eq!(dependents[0].kind, ComponentKind::Query);
    }

    #[tokio::test]
    async fn get_dependents_leaf_returns_empty() {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("leaf-test")
            .with_source(source)
            .with_query(
                Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("src1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        // q1 is a leaf — nothing depends on it
        let dependents = core.get_dependents("q1").await;
        assert!(dependents.is_empty());
    }

    // ========================================================================
    // get_dependencies
    // ========================================================================

    #[tokio::test]
    async fn get_dependencies_query_depends_on_source() {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("deps-test")
            .with_source(source)
            .with_query(
                Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("src1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let deps = core.get_dependencies("q1").await;

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "src1");
        assert_eq!(deps[0].kind, ComponentKind::Source);
    }

    #[tokio::test]
    async fn get_dependencies_source_has_none() {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("no-deps-test")
            .with_source(source)
            .build()
            .await
            .unwrap();

        let deps = core.get_dependencies("src1").await;
        assert!(deps.is_empty());
    }

    // ========================================================================
    // can_remove_component
    // ========================================================================

    #[tokio::test]
    async fn can_remove_component_safe() {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("remove-safe")
            .with_source(source)
            .with_query(
                Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("src1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        // q1 is a leaf — safe to remove
        assert!(core.can_remove_component("q1").await.is_ok());
    }

    #[tokio::test]
    async fn can_remove_component_unsafe_has_dependents() {
        let source = TestMockSource::new("src1".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("remove-unsafe")
            .with_source(source)
            .with_query(
                Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("src1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        // src1 has q1 depending on it — cannot remove
        let result = core.can_remove_component("src1").await;
        assert!(result.is_err());

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("src1"),
            "Error should mention the component ID"
        );
        assert!(
            err_msg.contains("q1"),
            "Error should mention the dependent ID"
        );
    }
}
