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

//! Shared **query-consumption** engine.
//!
//! Some components consume the results of continuous queries: reactions (to act on
//! them) and *query-consuming sources* (to derive a graph from them, e.g. a state
//! machine). Both need the same robust consumption behavior:
//!
//! - **catch-up** on start: replay a query's outbox / snapshot from the last
//!   checkpoint so results produced before the consumer subscribed are not lost;
//! - **checkpointing**: persist the per-`(consumer, query)` position so a restart
//!   resumes where it left off;
//! - **gap/lag recovery**: detect broadcast lag or sequence gaps and apply a
//!   recovery policy.
//!
//! This module centralizes that behavior behind the [`QueryConsumer`] trait and
//! the shared [`engine::ConsumptionEngine`], which both `ReactionManager` and
//! `SourceManager` embed — so reactions and query-consuming sources share **one**
//! implementation instead of divergent copies. Components adapt to the engine via
//! thin wrappers ([`ReactionConsumer`], [`SourceConsumer`]), leaving the
//! `Reaction`/`Source` traits and their base types unchanged.
//!
//! # Terminology: "catch-up" vs "bootstrap"
//!
//! For a source these are two *different* things, so this module deliberately uses
//! **catch-up** for the consumption/input side:
//!
//! - **catch-up** (this module, [`CatchupContext`]): upstream query → consumer.
//!   "When I start consuming a query, replay its snapshot/outbox to my checkpoint."
//! - **bootstrap** ([`crate::bootstrap::BootstrapProvider`]): source → downstream
//!   query. "When a query subscribes to me, seed it with my current graph."
//!
//! Reactions keep the public method name [`crate::Reaction::bootstrap`] as a thin
//! alias — a reaction's [`QueryConsumer::catch_up`] delegates to it — so the
//! reaction API and its FFI stay unchanged.

use anyhow::Result;
use async_trait::async_trait;

use crate::channels::QueryResult;
use crate::recovery::ReactionRecoveryPolicy;

mod adapters;
pub mod engine;

pub use adapters::{ReactionConsumer, SourceConsumer};
pub use engine::ConsumptionEngine;

/// The context handed to [`QueryConsumer::catch_up`], exposing the query's
/// `fetch_snapshot()` / `fetch_outbox()` and checkpoint read/write.
///
/// This is the same type reactions receive from [`crate::Reaction::bootstrap`];
/// it is re-exported here under the consumption-side name. (Kept as an alias to
/// avoid churning the reaction API and FFI — see the module docs.)
pub use crate::reactions::bootstrap_context::BootstrapContext as CatchupContext;

/// A component that consumes continuous-query results (a reaction, or a
/// query-consuming source).
///
/// The shared consumption engine drives any `QueryConsumer`: it subscribes to the
/// consumed queries, catches up from the checkpoint, forwards live results into
/// [`Self::enqueue_query_result`], and recovers from gaps per
/// [`Self::default_recovery_policy`].
#[async_trait]
pub trait QueryConsumer: Send + Sync {
    /// Unique id of the consuming component (used as the checkpoint partition key).
    fn consumer_id(&self) -> &str;

    /// The query IDs whose results this component consumes.
    fn consumed_query_ids(&self) -> Vec<String>;

    /// Enqueue a query result for processing (typically delegates to the
    /// component's [`QueryConsumerCore::enqueue_query_result`]).
    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()>;

    /// Whether this consumer requires a durable state store for its checkpoints.
    fn is_durable(&self) -> bool {
        false
    }

    /// Whether this consumer needs a full snapshot on a fresh start (no prior
    /// checkpoint), rather than just replaying the outbox.
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    /// The default recovery policy for this consumer.
    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }

    /// Catch up on a query subscription (fresh start or gap recovery). The
    /// [`CatchupContext`] provides `fetch_snapshot`/`fetch_outbox` and checkpoint
    /// read/write. Default is a no-op (outbox replay is handled by the engine).
    async fn catch_up(&self, _ctx: CatchupContext) -> Result<()> {
        Ok(())
    }
}
