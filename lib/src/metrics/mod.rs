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

//! Observability metrics for the Resumable Reactions pipeline.
//!
//! Provides lock-free atomic metric counters organized into three categories:
//! - [`QueryOutputMetrics`] — per-query outbox, sequence, snapshot health
//! - [`ReactionMetrics`] — per-reaction per-query checkpoint, dedup, recovery
//! - [`LifecycleMetrics`] — startup rejections, resets, hash mismatches
//!
//! All structs use `Arc` sharing and `std::sync::atomic` for thread-safe,
//! lock-free updates. Each provides a `.snapshot()` method returning a plain
//! `Clone` + `Debug` struct for easy consumption.

pub mod lifecycle_metrics;
pub mod query_metrics;
pub mod reaction_metrics;

pub use lifecycle_metrics::{LifecycleMetrics, LifecycleMetricsSnapshot};
pub use query_metrics::{QueryOutputMetrics, QueryOutputMetricsSnapshot};
pub use reaction_metrics::{ReactionMetrics, ReactionMetricsSnapshot};
