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

pub mod bootstrap_context;
pub mod checkpoint;
pub mod common;
pub mod manager;
pub mod snapshot_fetcher;
mod traits;

#[cfg(test)]
pub(crate) mod tests;

// Re-export the Reaction trait. QueryProvider is internal to ReactionManager.
pub(crate) use traits::QueryProvider;
pub use traits::Reaction;

pub use bootstrap_context::BootstrapBackend;
pub use bootstrap_context::BootstrapContext;
pub use checkpoint::ReactionCheckpoint;
pub use common::base::{ReactionBase, ReactionBaseParams};
pub use manager::ReactionManager;
pub use snapshot_fetcher::SnapshotFetcher;
