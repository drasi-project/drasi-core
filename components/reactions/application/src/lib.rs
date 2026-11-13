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

//! Application reactions for in-process Drasi hosts.
//!
//! This plugin implements Application reactions for Drasi.
//! Use [`NativeApplicationReaction`] for direct ComputationGraph envelope
//! delivery with bounded channel or awaited callback completion. The existing
//! [`ApplicationReaction`] remains the legacy `QueryResult` interface.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_application::ApplicationReaction;
//!
//! let (reaction, handle) = ApplicationReaction::builder("my-app-reaction")
//!     .with_queries(vec!["query1".to_string()])
//!     .build();
//!
//! // Use handle to receive results
//! let mut subscription = handle.subscribe_with_options(Default::default()).await?;
//! while let Some(result) = subscription.recv().await {
//!     println!("Result: {:?}", result);
//! }
//! ```

pub mod application;
pub mod config;
pub mod native;
pub mod subscription;

pub use application::ApplicationReaction;
pub use application::ApplicationReactionHandle;
pub use config::ApplicationReactionConfig;
pub use native::ApplicationReaction as NativeApplicationReaction;

/// Builder for Application reaction
pub struct ApplicationReactionBuilder {
    id: drasi_lib::ComponentId,
    queries: Vec<drasi_lib::config::ComponentReference>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl ApplicationReactionBuilder {
    /// Create a new Application reaction builder with the given ID
    pub fn new(id: drasi_lib::ComponentId) -> Self {
        Self {
            id: id.clone(),
            queries: Vec::new(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set explicit name or ID references for every query input.
    pub fn with_query_inputs(mut self, inputs: Vec<drasi_lib::config::ComponentReference>) -> Self {
        self.queries = inputs;
        self
    }

    /// Set all query inputs by their current names.
    pub fn with_queries_by_name(self, names: Vec<drasi_lib::ComponentName>) -> Self {
        self.with_query_inputs(
            names
                .into_iter()
                .map(drasi_lib::config::ComponentReference::by_name)
                .collect(),
        )
    }

    /// Set all query inputs by their immutable IDs.
    pub fn with_queries_by_id(self, ids: Vec<drasi_lib::ComponentId>) -> Self {
        self.with_query_inputs(
            ids.into_iter()
                .map(drasi_lib::config::ComponentReference::by_id)
                .collect(),
        )
    }

    /// Add a query input by its current name.
    pub fn with_query_by_name(mut self, query_name: drasi_lib::ComponentName) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_name(query_name));
        self
    }

    /// Add a query input by its immutable ID.
    pub fn with_query_by_id(mut self, query_id: drasi_lib::ComponentId) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_id(query_id));
        self
    }

    /// Set the priority queue capacity
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Build the Application reaction
    ///
    /// Returns a tuple of (reaction, handle) where the handle can be used
    /// to receive query results in the application.
    pub fn build(self) -> (ApplicationReaction, ApplicationReactionHandle) {
        ApplicationReaction::from_builder(
            self.id,
            self.queries,
            self.priority_queue_capacity,
            self.auto_start,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::Reaction;

    #[test]
    fn test_application_builder_defaults() {
        let (reaction, _handle) = ApplicationReactionBuilder::new(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID"),
        )
        .build();
        assert_eq!(
            reaction.id(),
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID")
        );
    }

    #[test]
    fn test_application_builder_custom() {
        let (reaction, _handle) = ApplicationReaction::builder(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID"),
        )
        .with_queries_by_name(vec![
            drasi_lib::ComponentName::try_new("query1").expect("fixture component name")
        ])
        .build();

        assert_eq!(
            reaction.id(),
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID")
        );
        assert_eq!(
            reaction.query_inputs(),
            vec![drasi_lib::config::ComponentReference::by_name(
                drasi_lib::ComponentName::try_new("query1").expect("fixture component name")
            )]
        );
    }

    #[test]
    fn test_application_new_constructor() {
        let (reaction, _handle) = ApplicationReaction::new(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID"),
            vec![drasi_lib::config::ComponentReference::by_name(
                drasi_lib::ComponentName::try_new("query1").expect("fixture component name"),
            )],
        );
        assert_eq!(
            reaction.id(),
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID")
        );
    }
}
