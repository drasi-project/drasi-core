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

//! Application reaction plugin for Drasi
//!
//! This plugin implements Application reactions for Drasi.
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
pub mod descriptor;
pub mod subscription;

pub use application::ApplicationReaction;
pub use application::ApplicationReactionHandle;
pub use config::ApplicationReactionConfig;

/// Builder for Application reaction
pub struct ApplicationReactionBuilder {
    id: String,
    queries: Vec<String>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl ApplicationReactionBuilder {
    /// Create a new Application reaction builder with the given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set the query IDs to subscribe to
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add a query ID to subscribe to
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
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
        let (reaction, _handle) = ApplicationReactionBuilder::new("test-reaction").build();
        assert_eq!(reaction.id(), "test-reaction");
    }

    #[test]
    fn test_application_builder_custom() {
        let (reaction, _handle) = ApplicationReaction::builder("test-reaction")
            .with_queries(vec!["query1".to_string()])
            .build();

        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }

    #[test]
    fn test_application_new_constructor() {
        let (reaction, _handle) =
            ApplicationReaction::new("test-reaction", vec!["query1".to_string()]);
        assert_eq!(reaction.id(), "test-reaction");
    }
}
