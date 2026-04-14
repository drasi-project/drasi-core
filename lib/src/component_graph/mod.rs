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

//! Component Dependency Graph — the single source of truth for DrasiLib configuration.
//!
//! The `ComponentGraph` maintains a directed graph of all components (Instance, Sources,
//! Queries, Reactions, BootstrapProviders, IdentityProviders) and their bidirectional
//! relationships. The DrasiLib instance itself is the root node.
//!
//! All three managers (SourceManager, QueryManager, ReactionManager) share the same
//! `Arc<RwLock<ComponentGraph>>`. Managers register components in the graph **first**
//! (registry-first pattern), then create and store runtime instances. The graph is the
//! authoritative source for component relationships, dependency tracking, and lifecycle
//! events.
//!
//! # Event Emission
//!
//! The graph emits [`ComponentEvent`]s via a built-in `broadcast::Sender` whenever
//! components are added, removed, or change status. Subscribers (ComponentGraphSource,
//! EventHistory, external consumers) receive events directly from the graph.
//!
//! # Status Update Channel
//!
//! Components report status changes via a shared `mpsc::Sender<ComponentUpdate>`.
//! A dedicated graph update loop (spawned externally) receives from this channel and
//! applies updates to the graph, emitting broadcast events. This decouples components
//! from the graph lock — status reporting is fire-and-forget.
//!
//! Structural mutations (`add_component`, `remove_component`, `add_relationship`) and
//! command-initiated transitions (`Starting`, `Stopping`) are applied directly by
//! managers, which hold the graph write lock on the cold path.

mod graph;
mod node;
mod transaction;
mod wait;

#[cfg(test)]
mod tests;

pub use graph::*;
pub use node::*;
pub use transaction::*;
pub use wait::*;
