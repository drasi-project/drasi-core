#![allow(unexpected_cfgs)]
// Copyright 2026 The Drasi Authors.
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

//! Azure Storage reaction plugin for Drasi.

pub(crate) mod blob;
pub mod config;
pub mod descriptor;
mod logging;
pub(crate) mod queue;
pub mod reaction;
pub(crate) mod table;
pub(crate) mod util;

pub use config::{
    AzureStorageReactionConfig, OperationType, QueryConfig, StorageTarget, TemplateSpec,
};
pub use reaction::AzureStorageReaction;

use std::collections::HashMap;

/// Builder for [`AzureStorageReaction`].
///
/// Use [`AzureStorageReaction::builder`] as the entry point.
pub struct AzureStorageReactionBuilder {
    id: drasi_lib::ComponentId,
    queries: Vec<drasi_lib::config::ComponentReference>,
    config: AzureStorageReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl AzureStorageReactionBuilder {
    pub fn new(id: drasi_lib::ComponentId) -> Self {
        Self {
            id: id.clone(),
            queries: Vec::new(),
            config: AzureStorageReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
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

    pub fn with_account_name(mut self, account_name: impl Into<String>) -> Self {
        self.config.account_name = account_name.into();
        self
    }

    pub fn with_access_key(mut self, access_key: impl Into<String>) -> Self {
        self.config.access_key = access_key.into();
        self
    }

    pub fn with_target(mut self, target: StorageTarget) -> Self {
        self.config.target = target;
        self
    }

    pub fn with_blob_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.blob_endpoint = Some(endpoint.into());
        self
    }

    pub fn with_queue_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.queue_endpoint = Some(endpoint.into());
        self
    }

    pub fn with_table_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.table_endpoint = Some(endpoint.into());
        self
    }

    pub fn with_default_template(mut self, config: QueryConfig) -> Self {
        self.config.default_template = Some(config);
        self
    }

    pub fn with_route(mut self, query_id: impl Into<String>, config: QueryConfig) -> Self {
        self.config.routes.insert(query_id.into(), config);
        self
    }

    pub fn with_routes(mut self, routes: HashMap<String, QueryConfig>) -> Self {
        self.config.routes = routes;
        self
    }

    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_config(mut self, config: AzureStorageReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Builds the [`AzureStorageReaction`].
    ///
    /// # Errors
    /// Returns an error if required configuration fields (`account_name`, `access_key`, and
    /// target-specific fields) are empty or invalid.
    pub fn build(self) -> anyhow::Result<AzureStorageReaction> {
        AzureStorageReaction::from_builder(
            self.id,
            self.queries,
            self.config,
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
    fn test_builder_defaults() {
        let reaction = AzureStorageReactionBuilder::new(
            drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000072")
                .expect("fixture UUID"),
        )
        .with_account_name("account")
        .with_access_key("key")
        .build()
        .expect("build should succeed");
        assert_eq!(
            reaction.id(),
            &drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000072")
                .expect("fixture UUID")
        );
    }

    #[test]
    fn test_builder_custom_values() {
        let reaction = AzureStorageReaction::builder(
            drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000072")
                .expect("fixture UUID"),
        )
        .with_account_name("acct")
        .with_access_key("key")
        .with_target(StorageTarget::Queue {
            queue_name: "events".to_string(),
        })
        .with_query_by_name(
            drasi_core::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .build()
        .expect("build should succeed");

        assert_eq!(
            reaction.query_inputs(),
            vec![drasi_lib::config::ComponentReference::by_name(
                drasi_core::ComponentName::try_new("q1").expect("fixture component name")
            )]
        );
        let props = reaction.properties();
        assert_eq!(
            props.get("account_name"),
            Some(&serde_json::Value::String("acct".to_string()))
        );
    }

    #[test]
    fn test_config_validation_fails_without_account() {
        let result = AzureStorageReaction::builder(
            drasi_core::ComponentId::try_new("00000000-0000-0000-0000-000000000072")
                .expect("fixture UUID"),
        )
        .with_access_key("key")
        .build();
        assert!(result.is_err());
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "azure-storage-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::AzureStorageReactionDescriptor],
    bootstrap_descriptors = [],
);
