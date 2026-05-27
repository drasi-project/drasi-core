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

use petgraph::stable_graph::{EdgeIndex, NodeIndex};

use crate::channels::ComponentEvent;

use super::graph::ComponentGraph;
use super::{ComponentNode, RelationshipKind};

// ============================================================================
// Graph Transaction
// ============================================================================

/// A transactional wrapper for batching graph mutations.
///
/// Collects added nodes and edges, deferring event emission until [`commit()`](Self::commit).
/// If dropped without commit, all additions are rolled back (nodes and edges removed).
///
/// The `&'g mut ComponentGraph` borrow ensures Rust's borrow checker prevents
/// any concurrent access to the graph during the transaction — zero-cost safety.
///
/// # Cross-system usage
///
/// For operations that span the graph and external systems (runtime initialization,
/// HashMap insertion), use this pattern:
///
/// ```ignore
/// // Phase 1: Graph transaction
/// {
///     let mut graph = self.graph.write().await;
///     let mut txn = graph.begin();
///     txn.add_component(node)?;
///     txn.commit(); // graph is consistent
/// }
///
/// // Phase 2: Runtime init (no graph lock)
/// let instance = match RuntimeType::new(...) {
///     Ok(i) => i,
///     Err(e) => {
///         // Compensating rollback
///         let mut graph = self.graph.write().await;
///         let _ = graph.remove_component(&id);
///         return Err(e);
///     }
/// };
/// ```
pub struct GraphTransaction<'g> {
    graph: &'g mut ComponentGraph,
    added_nodes: Vec<(NodeIndex, String)>,
    added_edges: Vec<EdgeIndex>,
    pending_events: Vec<ComponentEvent>,
    committed: bool,
}

impl<'g> GraphTransaction<'g> {
    pub(super) fn new(graph: &'g mut ComponentGraph) -> Self {
        Self {
            graph,
            added_nodes: Vec::new(),
            added_edges: Vec::new(),
            pending_events: Vec::new(),
            committed: false,
        }
    }

    /// Add a component node to the graph within this transaction.
    ///
    /// The node is added immediately (so subsequent `add_relationship` calls
    /// can reference it), but the event is deferred until `commit()`.
    /// On rollback (drop without commit), the node and its ownership edges
    /// are removed.
    pub fn add_component(&mut self, node: ComponentNode) -> anyhow::Result<NodeIndex> {
        let id = node.id.clone();
        let (node_idx, event, owns_edge, owned_by_edge) =
            self.graph.add_component_internal(node)?;
        self.added_nodes.push((node_idx, id));

        // Record the ownership edges for rollback
        self.added_edges.push(owns_edge);
        self.added_edges.push(owned_by_edge);

        if let Some(event) = event {
            self.pending_events.push(event);
        }

        Ok(node_idx)
    }

    /// Add a bidirectional relationship within this transaction.
    ///
    /// The edges are added immediately; on rollback they are removed.
    /// Idempotent: if the relationship already exists, this is a no-op.
    pub fn add_relationship(
        &mut self,
        from_id: &str,
        to_id: &str,
        forward: RelationshipKind,
    ) -> anyhow::Result<()> {
        let (fwd, rev) = self
            .graph
            .add_relationship_internal(from_id, to_id, forward)?;
        if let Some(e) = fwd {
            self.added_edges.push(e);
        }
        if let Some(e) = rev {
            self.added_edges.push(e);
        }
        Ok(())
    }

    /// Commit the transaction: emit all deferred events.
    ///
    /// After commit, the mutations are permanent and cannot be rolled back
    /// through this transaction. For cross-system rollback, use compensating
    /// actions (e.g., `graph.remove_component()`).
    pub fn commit(mut self) {
        self.committed = true;
        for event in &self.pending_events {
            let _ = self.graph.event_tx.send(event.clone());
            self.graph.event_history.record_event(event.clone());
        }
    }
}

impl<'g> Drop for GraphTransaction<'g> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        // Rollback: remove all added edges first (reverse order to maintain index stability)
        for &edge_idx in self.added_edges.iter().rev() {
            self.graph.graph.remove_edge(edge_idx);
        }

        // Remove all added nodes (reverse order)
        for (node_idx, ref id) in self.added_nodes.iter().rev() {
            self.graph.index.remove(id.as_str());
            self.graph.graph.remove_node(*node_idx);
        }
    }
}
